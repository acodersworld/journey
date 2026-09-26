use std::{
    collections::BTreeMap, fs::{self, File, OpenOptions}, io::{Read, Seek, SeekFrom, Write}, num::NonZeroUsize, ops::DerefMut, os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}, sync::{Arc, Mutex}, time::{SystemTime, UNIX_EPOCH},
};

use bytes::{Bytes, BytesMut};
use http::HeaderValue;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::storage_interface::{
    ContentType, GetResult, Key, ListCursor, ListPage, ListRequest, ObjectInterface, ObjectMetadata,
    PutCondition, PutContextInterface, ReadObject, ReadRange, ReadSpan, StoreError,
    StoreErrorKind, StoreInterface,
};

const FIXED_METADATA_LEN: usize = 84;
const MIN_METADATA_LEN: usize = 86;
const MAX_METADATA_LEN: usize = 1_236;
const MAX_KEY_LEN: usize = 1_024;
const MAX_CONTENT_TYPE_LEN: usize = 128;
const MAGIC: &[u8; 8] = b"OBJSTORE";
const FORMAT_VERSION: u16 = 1;

#[derive(Clone, Debug)]
pub struct FilesystemStoreConfig {
    root: PathBuf,
    journal_path: PathBuf,
    max_list_page_size: NonZeroUsize,
}

impl FilesystemStoreConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            journal_path: root.join("journal"),
            root,
            max_list_page_size: NonZeroUsize::new(1_000).unwrap(),
        }
    }

    pub fn with_journal_path(mut self, journal_path: impl Into<PathBuf>) -> Self {
        self.journal_path = journal_path.into();
        self
    }

    pub fn with_max_list_page_size(mut self, max_list_page_size: NonZeroUsize) -> Self {
        self.max_list_page_size = max_list_page_size;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub fn max_list_page_size(&self) -> NonZeroUsize {
        self.max_list_page_size
    }
}

#[derive(Clone, Debug)]
pub struct FilesystemStore {
    config: FilesystemStoreConfig,
    objects_dir: PathBuf,
    part_dir: PathBuf,
    index: Arc<RwLock<BTreeMap<Key, IndexEntry>>>,
    // Keep the flock descriptor alive so this process retains exclusive root ownership.
    _root_lock_file: Arc<File>,
}

#[derive(Clone, Debug)]
enum IndexEntry {
    Healthy {
        metadata: ObjectMetadata,
        metadata_len: u64,
        file: Arc<File>,
        object_id: String,
    },
    Corrupt {
        physical_name: String,
        reason: String,
        object_id: Option<String>,
    },
}

impl FilesystemStore {
    /// Acquires exclusive storage-root ownership, clears stale uploads, and builds the index.
    pub async fn open(config: FilesystemStoreConfig) -> Result<Self, StoreError> {
        let startup_config = config.clone();
        let output = tokio::task::spawn_blocking(move || initialize_store(&startup_config))
            .await
            .map_err(|error| unavailable(format!("Storage startup task failed: {error}")))??;
        let mut config = config;
        config.root = output.root;
        Ok(Self {
            config,
            objects_dir: output.objects_dir,
            part_dir: output.part_dir,
            index: Arc::new(RwLock::new(output.index)),
            _root_lock_file: output.root_lock_file,
        })
    }

    fn object_path(&self, key: &Key) -> PathBuf {
        self.objects_dir.join(format!("{}.obj", key_digest(key.as_str())))
    }

    fn corrupt_error(
        &self,
        key: &Key,
        physical_name: &str,
        reason: &str,
        object_id: Option<&str>,
    ) -> StoreError {
        emit_event(
            &self.config.journal_path,
            "corrupt_object_accessed",
            Some(key.as_str()),
            Some(physical_name),
            reason,
            object_id,
        );
        StoreError::new(StoreErrorKind::Corrupt, format!("Object {key} is corrupt: {reason}"))
    }

    async fn verify_file_length(
        &self,
        key: &Key,
        metadata: &ObjectMetadata,
        metadata_len: u64,
        file: Arc<File>,
        physical_name: &str,
        object_id: &str,
    ) -> Result<(), StoreError> {
        let expected_len = metadata_len
            .checked_add(metadata.payload_length())
            .ok_or_else(|| self.corrupt_error(key, physical_name, "file length overflows u64", Some(object_id)))?;
        let actual_len = tokio::task::spawn_blocking(move || file.metadata().map(|metadata| metadata.len()))
            .await
            .map_err(|error| unavailable(format!("File metadata task failed: {error}")))?
            .map_err(|error| io_error("read object file metadata", error))?;
        if actual_len != expected_len {
            return Err(self.corrupt_error(
                key,
                physical_name,
                &format!("file length is {actual_len}, expected {expected_len}"),
                Some(object_id),
            ));
        }
        Ok(())
    }
}

impl StoreInterface for FilesystemStore {
    type Object = FilesystemObjectReader;
    type PutContext = FilesystemPutContext;

    async fn get(
        &self,
        key: &Key,
        range: Option<ReadRange>,
    ) -> Result<GetResult<Self::Object>, StoreError> {
        let entry = self.index.read().await.get(key).cloned().ok_or_else(|| {
            StoreError::new(StoreErrorKind::NotFound, format!("Object not found: {key}"))
        })?;
        let (metadata, metadata_len, file, object_id) = match entry {
            IndexEntry::Healthy { metadata, metadata_len, file, object_id } => {
                (metadata, metadata_len, file, object_id)
            }
            IndexEntry::Corrupt { physical_name, reason, object_id } => {
                return Err(self.corrupt_error(key, &physical_name, &reason, object_id.as_deref()));
            }
        };
        let physical_name = format!("{}.obj", key_digest(key.as_str()));
        self.verify_file_length(key, &metadata, metadata_len, Arc::clone(&file), &physical_name, &object_id)
            .await?;
        let selected_span = match range {
            Some(range) => match resolve_range(range, metadata.payload_length())? {
                Some(span) => Some(span),
                None => return Ok(GetResult::Unsatisfiable { complete_length: metadata.payload_length() }),
            },
            None => None,
        };
        let span = selected_span.unwrap_or_else(|| ReadSpan::new(0, metadata.payload_length()));
        let reader = FilesystemObjectReader { file, metadata_len, span, cursor: 0 };
        let read_object = match selected_span {
            Some(span) => ReadObject::with_selected_span(metadata, reader, span),
            None => ReadObject::new(metadata, reader),
        };
        Ok(GetResult::Found(read_object))
    }

    async fn stat(&self, key: &Key) -> Result<ObjectMetadata, StoreError> {
        let entry = self.index.read().await.get(key).cloned().ok_or_else(|| {
            StoreError::new(StoreErrorKind::NotFound, format!("Object not found: {key}"))
        })?;
        match entry {
            IndexEntry::Healthy { metadata, metadata_len, file, object_id } => {
                let physical_name = format!("{}.obj", key_digest(key.as_str()));
                self.verify_file_length(key, &metadata, metadata_len, file, &physical_name, &object_id)
                    .await?;
                Ok(metadata)
            }
            IndexEntry::Corrupt { physical_name, reason, object_id } => Err(self.corrupt_error(
                key,
                &physical_name,
                &reason,
                object_id.as_deref(),
            )),
        }
    }

    async fn list(&self, request: ListRequest) -> Result<ListPage, StoreError> {
        let entries = self.index.read().await;
        let limit = self.config.max_list_page_size.min(request.requested_limit());
        let lower_bound = match request.cursor() {
            Some(cursor) if cursor.start_key().as_str() > request.prefix() => {
                std::ops::Bound::Included(cursor.start_key().as_str())
            }
            Some(_) | None => std::ops::Bound::Included(request.prefix()),
        };
        let mut metadata = Vec::new();
        let mut next_page_key = None;
        for (key, entry) in entries.range::<str, _>((lower_bound, std::ops::Bound::Unbounded)) {
            if !key.as_str().starts_with(request.prefix()) {
                break;
            }
            let IndexEntry::Healthy { metadata: object_metadata, .. } = entry else {
                continue;
            };
            if metadata.len() == limit.get() {
                next_page_key = Some(key.clone());
                break;
            }
            metadata.push(object_metadata.clone());
        }
        let next_cursor = next_page_key.map(ListCursor::new);
        Ok(ListPage::new(metadata, next_cursor))
    }

    async fn delete(&self, key: &Key) -> Result<(), StoreError> {
        let mut entries = self.index.write().await;
        let previous = entries.get(key).cloned();
        let path = self.object_path(key);
        if let Err(error) = fs::remove_file(path) && error.kind() != std::io::ErrorKind::NotFound {
            return Err(io_error("delete object file", error));
        }
        entries.remove(key);
        drop(entries);
        if let Some(IndexEntry::Corrupt { physical_name, reason, object_id }) = previous {
            emit_event(
                &self.config.journal_path,
                "corrupt_object_deleted",
                Some(key.as_str()),
                Some(&physical_name),
                &reason,
                object_id.as_deref(),
            );
        }
        Ok(())
    }

    async fn put_context(
        &self,
        key: Key,
        content_type: ContentType,
        condition: PutCondition,
    ) -> Result<Self::PutContext, StoreError> {
        let metadata_len = metadata_length(&key, &content_type)?;
        let (temp_path, file) = create_part_file(&self.part_dir, metadata_len)?;
        Ok(FilesystemPutContext {
            key,
            content_type,
            condition,
            metadata_len,
            publication_state: PublicationState::Unpublished { temp_path },
            upload_state: Arc::new(Mutex::new(UploadState::Active(ActiveUpload {
                file,
                digest: Sha256::new(),
                payload_len: 0,
            }))),
        })
    }

    async fn put(&self, mut context: Self::PutContext) -> Result<(), StoreError> {
        let key = context.key.clone();
        let content_type = context.content_type.clone();
        let condition = context.condition;
        let metadata_len = context.metadata_len as u64;
        let upload_state = Arc::clone(&context.upload_state);
        let key_for_finalize = key.clone();
        let content_type_for_finalize = content_type.clone();
        let finalized = tokio::task::spawn_blocking(move || {
            finalize_upload(upload_state, key_for_finalize, content_type_for_finalize)
        })
        .await
        .map_err(|error| unavailable(format!("Upload finalization task failed: {error}")))??;

        let mut entries = self.index.write().await;
        let present = entries.contains_key(&key);
        let final_path = self.object_path(&key);
        match fs::symlink_metadata(&final_path) {
            Ok(_) if !present => {
                return Err(StoreError::new(
                    StoreErrorKind::Corrupt,
                    format!("Unindexed file occupies object path {}", final_path.display()),
                ));
            }
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                return Err(StoreError::new(
                    StoreErrorKind::Corrupt,
                    format!("Non-regular file occupies object path {}", final_path.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error("inspect object publication path", error)),
        }
        let condition_satisfied = match condition {
            PutCondition::Unconditional => true,
            PutCondition::CreateOnly => !present,
            PutCondition::ReplaceOnly => present,
        };
        if !condition_satisfied {
            return Err(StoreError::new(
                StoreErrorKind::PreconditionFailed,
                format!("PUT condition failed for key: {key}"),
            ));
        }

        let temp_path = match &context.publication_state {
            PublicationState::Unpublished { temp_path } => temp_path,
            PublicationState::Published => {
                return Err(StoreError::new(StoreErrorKind::Internal, "Upload is already published"));
            }
        };
        fs::rename(temp_path, &final_path).map_err(|error| io_error("publish object file", error))?;

        entries.insert(
            key,
            IndexEntry::Healthy {
                metadata: finalized.metadata,
                metadata_len,
                file: finalized.file,
                object_id: finalized.object_id,
            },
        );
        context.publication_state = PublicationState::Published;
        Ok(())
    }
}

#[derive(Debug)]
pub struct FilesystemPutContext {
    key: Key,
    content_type: ContentType,
    condition: PutCondition,
    metadata_len: usize,
    publication_state: PublicationState,
    upload_state: Arc<Mutex<UploadState>>,
}

#[derive(Debug)]
enum PublicationState {
    Unpublished { temp_path: PathBuf },
    Published,
}

impl PutContextInterface for FilesystemPutContext {
    async fn append(&mut self, bytes: &Bytes) -> Result<(), StoreError> {
        let upload_state = Arc::clone(&self.upload_state);
        let bytes = bytes.clone();
        tokio::task::spawn_blocking(move || {
            let mut upload_state_guard = upload_state.lock().map_err(|_| {
                StoreError::new(StoreErrorKind::Internal, "Upload state lock is poisoned")
            })?;
            let active_upload = match upload_state_guard.deref_mut() {
                UploadState::Active(active_upload) => active_upload,
                UploadState::Failed { reason } => return Err(unavailable(reason.clone())),
            };

            let next_len = active_upload.payload_len.checked_add(bytes.len() as u64).ok_or_else(|| {
                StoreError::new(StoreErrorKind::Capacity, "Payload length exceeds u64")
            })?;
            if let Err(error) = active_upload.file.write_all(&bytes) {
                let reason = format!("Write upload payload: {error}");
                *upload_state_guard = UploadState::Failed { reason: reason.clone() };
                return Err(unavailable(reason));
            }
            active_upload.digest.update(&bytes);
            active_upload.payload_len = next_len;
            Ok(())
        })
        .await
        .map_err(|error| unavailable(format!("Upload append task failed: {error}")))?
    }
}

impl Drop for FilesystemPutContext {
    fn drop(&mut self) {
        if let PublicationState::Unpublished { temp_path } = &self.publication_state
            && let Err(error) = fs::remove_file(temp_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("failed to remove unpublished upload {}: {error}", temp_path.display());
        }
    }
}

#[derive(Debug)]
struct ActiveUpload {
    file: File,
    digest: Sha256,
    payload_len: u64,
}

#[derive(Debug)]
enum UploadState {
    Active(ActiveUpload),
    Failed { reason: String },
}

struct FinalizedUpload {
    metadata: ObjectMetadata,
    file: Arc<File>,
    object_id: String,
}

fn finalize_upload(
    upload_state: Arc<Mutex<UploadState>>,
    key: Key,
    content_type: ContentType,
) -> Result<FinalizedUpload, StoreError> {
    let mut upload_state_guard = upload_state.lock().map_err(|_| {
        StoreError::new(StoreErrorKind::Internal, "Upload state lock is poisoned")
    })?;

    let active_upload = match upload_state_guard.deref_mut() {
        UploadState::Active(active_upload) => active_upload,
        UploadState::Failed { reason } => return Err(unavailable(reason.clone())),
    };

    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StoreError::new(StoreErrorKind::Internal, format!("System clock error: {error}")))?
        .as_millis();
    let created_at_ms = u64::try_from(created_at_ms)
        .map_err(|error| StoreError::new(StoreErrorKind::Internal, format!("Creation time overflow: {error}")))?;
    let object_id_bytes = random_object_id()?;
    let object_id = format_uuid(&object_id_bytes);
    let digest: [u8; 32] = active_upload.digest.clone().finalize().into();
    let metadata_len = metadata_length(&key, &content_type)?;
    let metadata = encode_metadata(
        &key,
        &content_type,
        active_upload.payload_len,
        created_at_ms,
        &digest,
        &object_id_bytes,
        metadata_len,
    )?;
    active_upload.file.seek(SeekFrom::Start(0))
        .and_then(|_| active_upload.file.write_all(&metadata))
        .and_then(|_| active_upload.file.sync_all())
        .map_err(|error| io_error("finalize and sync upload file", error))?;
    let file = active_upload.file.try_clone().map_err(|error| io_error("clone published object handle", error))?;
    Ok(FinalizedUpload {
        metadata: ObjectMetadata::new(key, content_type, active_upload.payload_len),
        file: Arc::new(file),
        object_id,
    })
}

#[derive(Debug)]
pub struct FilesystemObjectReader {
    file: Arc<File>,
    metadata_len: u64,
    span: ReadSpan,
    cursor: u64,
}

impl ObjectInterface for FilesystemObjectReader {
    async fn read(
        &mut self,
        buffer: &mut BytesMut,
    ) -> Result<usize, StoreError> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let remaining = self.span.size().checked_sub(self.cursor).ok_or_else(|| {
            StoreError::new(StoreErrorKind::Internal, "Object reader cursor exceeds selected span")
        })?;
        if remaining == 0 {
            return Ok(0);
        }
        let requested = remaining.min(buffer.len() as u64) as usize;
        let logical_offset = self.span.offset().checked_add(self.cursor).ok_or_else(|| {
            StoreError::new(StoreErrorKind::Corrupt, "Object read offset overflow")
        })?;
        let physical_offset = self.metadata_len.checked_add(logical_offset).ok_or_else(|| {
            StoreError::new(StoreErrorKind::Corrupt, "Physical read offset overflow")
        })?;
        let file = Arc::clone(&self.file);
        let owned_buffer = std::mem::take(buffer);
        let read_result = tokio::task::spawn_blocking(move || {
            let mut buffer = owned_buffer;
            let mut count = 0usize;
            let result = loop {
                if count == requested {
                    break Ok(count);
                }
                let offset = match physical_offset.checked_add(count as u64) {
                    Some(offset) => offset,
                    None => break Err(StoreError::new(StoreErrorKind::Corrupt, "Physical read offset overflow")),
                };
                match file.read_at(&mut buffer[count..requested], offset) {
                    Ok(0) => break Err(StoreError::new(StoreErrorKind::Corrupt, "Unexpected EOF in object payload")),
                    Ok(read) => count += read,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => break Err(io_error("read object payload", error)),
                }
            };
            (buffer, result)
        })
        .await
        .map_err(|error| unavailable(format!("Object read task failed: {error}")))?;
        let (returned_buffer, result) = read_result;
        *buffer = returned_buffer;
        match result {
            Ok(count) => {
                self.cursor = self.cursor.checked_add(count as u64).ok_or_else(|| {
                    StoreError::new(StoreErrorKind::Internal, "Object reader cursor overflow")
                })?;
                Ok(count)
            }
            Err(error) => Err(error),
        }
    }
}

struct InitOutput {
    root: PathBuf,
    objects_dir: PathBuf,
    part_dir: PathBuf,
    index: BTreeMap<Key, IndexEntry>,
    root_lock_file: Arc<File>,
}

fn initialize_store(
    config: &FilesystemStoreConfig,
) -> Result<InitOutput, StoreError> {
    fs::create_dir_all(&config.root).map_err(|error| io_error("create storage root", error))?;
    let root = fs::canonicalize(&config.root).map_err(|error| io_error("resolve storage root", error))?;
    if !fs::metadata(&root).map_err(|error| io_error("inspect storage root", error))?.is_dir() {
        return Err(unavailable("Storage root is not a directory"));
    }
    let objects_dir = root.join("objects");
    let part_dir = root.join("part");
    ensure_directory(&objects_dir)?;
    ensure_directory(&part_dir)?;
    let objects_metadata = fs::metadata(&objects_dir).map_err(|error| io_error("inspect objects directory", error))?;
    let part_metadata = fs::metadata(&part_dir).map_err(|error| io_error("inspect part directory", error))?;
    if objects_metadata.dev() != part_metadata.dev() {
        return Err(unavailable("part/ and objects/ must be on the same filesystem"));
    }

    let lock_path = root.join(".journey-storage.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(&lock_path)
        .map_err(|error| io_error("open storage-root lock", error))?;
    let lock_result = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&lock_file),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if lock_result != 0 {
        let error = std::io::Error::last_os_error();
        return Err(unavailable(format!("Could not acquire exclusive storage-root ownership: {error}")));
    }
    let root_lock = Arc::new(lock_file);

    clear_part_directory(&part_dir, &config.journal_path)?;
    let (index, skipped) = scan_objects(&objects_dir, &config.journal_path)?;
    eprintln!("filesystem object store ready: indexed {} objects, skipped {skipped} files", index.len());
    Ok(InitOutput {
        root,
        objects_dir,
        part_dir,
        index,
        root_lock_file: root_lock,
    })
}

fn ensure_directory(path: &Path) -> Result<(), StoreError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(unavailable(
            format!("Storage path {} is not a real directory", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| io_error("create storage directory", error))
        }
        Err(error) => Err(io_error("inspect storage directory", error)),
    }
}

fn clear_part_directory(part_dir: &Path, journal_path: &Path) -> Result<(), StoreError> {
    let entries = match fs::read_dir(part_dir) {
        Ok(entries) => entries,
        Err(error) => {
            emit_event(
                journal_path,
                "part_cleanup_failed",
                None,
                Some("part/"),
                &error.to_string(),
                None,
            );
            return Err(io_error("enumerate part directory", error));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                emit_event(
                    journal_path,
                    "part_cleanup_failed",
                    None,
                    Some("part/"),
                    &error.to_string(),
                    None,
                );
                return Err(io_error("read part directory entry", error));
            }
        };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let result = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            emit_event(journal_path, "part_cleanup_failed", None, Some(&name), &error.to_string(), None);
            return Err(unavailable(format!("Could not clear unpublished upload {}: {error}", path.display())));
        }
    }
    Ok(())
}

fn scan_objects(
    objects_dir: &Path,
    journal_path: &Path,
) -> Result<(BTreeMap<Key, IndexEntry>, usize), StoreError> {
    let entries = fs::read_dir(objects_dir).map_err(|error| io_error("enumerate objects directory", error))?;
    let mut index = BTreeMap::new();
    let mut skipped = 0usize;
    for entry in entries {
        let entry = entry.map_err(|error| io_error("read objects directory entry", error))?;
        let physical_name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let kind = match entry.file_type() {
            Ok(kind) => kind,
            Err(error) => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), &error.to_string(), None);
                continue;
            }
        };
        if kind.is_symlink() || !kind.is_file() {
            skipped += 1;
            emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), "published entry is not a regular file", None);
            continue;
        }
        if !is_object_filename(&physical_name) {
            skipped += 1;
            emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), "invalid published filename", None);
            continue;
        }
        let file = match open_published_file(&path) {
            Ok(file) => file,
            Err(error) if error.raw_os_error() == Some(libc::EMFILE) || error.raw_os_error() == Some(libc::ENFILE) => {
                return Err(unavailable(format!("File descriptor exhaustion while opening {physical_name}: {error}")));
            }
            Err(error) => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), &error.to_string(), None);
                continue;
            }
        };
        let file_metadata = match file.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), "opened entry is not a regular file", None);
                continue;
            }
            Err(error) => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), &error.to_string(), None);
                continue;
            }
        };
        match parse_object_file(&file, &physical_name, file_metadata.len()) {
            ParsedObject::Healthy { key, metadata, metadata_len, object_id } => {
                index.insert(key, IndexEntry::Healthy {
                    metadata,
                    metadata_len: metadata_len as u64,
                    file: Arc::new(file),
                    object_id,
                });
            }
            ParsedObject::Corrupt { key: Some(key), reason, object_id } => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", Some(key.as_str()), Some(&physical_name), &reason, object_id.as_deref());
                index.insert(key, IndexEntry::Corrupt { physical_name, reason, object_id });
            }
            ParsedObject::Corrupt { key: None, reason, object_id } => {
                skipped += 1;
                emit_event(journal_path, "corrupt_file_skipped", None, Some(&physical_name), &reason, object_id.as_deref());
            }
        }
    }
    Ok((index, skipped))
}

fn open_published_file(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    options.open(path)
}

enum ParsedObject {
    Healthy {
        key: Key,
        metadata: ObjectMetadata,
        metadata_len: usize,
        object_id: String,
    },
    Corrupt {
        key: Option<Key>,
        reason: String,
        object_id: Option<String>,
    },
}

/*
 * Version 1 on-disk object layout
 * All integer fields are little-endian; ranges are half-open byte offsets.
 *
 * Fixed metadata (84 bytes):
 * +-----------+----------------+----------------+-----------------+
 * | [0, 2)    | [2, 10)        | [10, 12)       | [12, 16)        |
 * | version   | magic          | metadata_len   | metadata CRC    |
 * | u16 = 1   | "OBJSTORE"     | u16            | CRC-32/ISO-HDLC |
 * +-----------+----------------+----------------+-----------------+
 * +----------------+----------------+----------------+-------------+
 * | [16, 24)       | [24, 32)       | [32, 64)       | [64, 80)    |
 * | payload_len    | created_at_ms  | payload SHA-256| object UUID |
 * | u64            | u64, Unix ms   | raw 32 bytes   | raw 16 bytes|
 * +----------------+----------------+----------------+-------------+
 * +----------------+--------------------+
 * | [80, 82)       | [82, 84)           |
 * | key_len, u16   | content_type_len   |
 * |                | u16                |
 * +----------------+--------------------+
 *
 * Variable metadata (no padding):
 * +-----------------------------+----------------------------------+
 * | [84, 84 + key_len)          | [84 + key_len, metadata_len)    |
 * | exact UTF-8 logical key     | exact content-type header bytes |
 * +-----------------------------+----------------------------------+
 *
 * Complete file:
 * +--------------------------------------+----------------------+
 * | metadata, [0, metadata_len)          | payload              |
 * | 84 + key_len + content_type_len      | payload_len bytes    |
 * +--------------------------------------+----------------------+
 *
 * key_len is 1..=1,024 bytes, content_type_len is 1..=128 bytes, and
 * metadata_len is 86..=1,236 bytes. The CRC covers the entire metadata with
 * bytes [12, 16) zeroed. The file length must equal metadata_len + payload_len.
 * The parser validates the version before reading later fields. It checks the
 * metadata CRC and file length; it does not recompute the stored payload digest.
 */
fn parse_object_file(file: &File, physical_name: &str, file_len: u64) -> ParsedObject {
    let mut version_bytes = [0_u8; 2];
    if let Err(error) = read_exact_at(file, &mut version_bytes, 0) {
        return unkeyed_corruption(format!("could not read format version: {error}"));
    }
    let version = u16::from_le_bytes(version_bytes);
    if version != FORMAT_VERSION {
        return unkeyed_corruption(format!("unsupported object format version {version}"));
    }

    let mut fixed = [0_u8; FIXED_METADATA_LEN];
    fixed[..2].copy_from_slice(&version_bytes);
    if let Err(error) = read_exact_at(file, &mut fixed[2..], 2) {
        return unkeyed_corruption(format!("could not read fixed metadata: {error}"));
    }
    if &fixed[2..10] != MAGIC {
        return unkeyed_corruption("metadata magic mismatch".to_string());
    }
    let metadata_len = u16::from_le_bytes([fixed[10], fixed[11]]) as usize;
    if !(MIN_METADATA_LEN..=MAX_METADATA_LEN).contains(&metadata_len) {
        return unkeyed_corruption(format!("metadata length {metadata_len} is outside supported bounds"));
    }
    let key_len = u16::from_le_bytes([fixed[80], fixed[81]]) as usize;
    let content_type_len = u16::from_le_bytes([fixed[82], fixed[83]]) as usize;
    if key_len == 0 || key_len > MAX_KEY_LEN || content_type_len == 0 || content_type_len > MAX_CONTENT_TYPE_LEN {
        return unkeyed_corruption("metadata key or content-type length is outside supported bounds".to_string());
    }
    let expected_metadata_len = match FIXED_METADATA_LEN
        .checked_add(key_len)
        .and_then(|length| length.checked_add(content_type_len))
    {
        Some(length) if length <= MAX_METADATA_LEN => length,
        _ => return unkeyed_corruption("metadata field lengths exceed supported bounds".to_string()),
    };
    if expected_metadata_len != metadata_len {
        return unkeyed_corruption("metadata length does not match field lengths".to_string());
    }
    let mut metadata_bytes = vec![0_u8; expected_metadata_len];
    metadata_bytes[..FIXED_METADATA_LEN].copy_from_slice(&fixed);
    if let Err(error) = read_exact_at(
        file,
        &mut metadata_bytes[FIXED_METADATA_LEN..],
        FIXED_METADATA_LEN as u64,
    ) {
        return unkeyed_corruption(format!("could not read variable metadata: {error}"));
    }
    let key_end = FIXED_METADATA_LEN + key_len;
    let key_text = match std::str::from_utf8(&metadata_bytes[FIXED_METADATA_LEN..key_end]) {
        Ok(key) => key,
        Err(error) => return unkeyed_corruption(format!("logical key is not UTF-8: {error}")),
    };
    let key = match Key::new(key_text) {
        Ok(key) => key,
        Err(error) => return unkeyed_corruption(format!("logical key is invalid: {error}")),
    };
    if !is_object_filename(physical_name) || format!("{}.obj", key_digest(key.as_str())) != physical_name {
        return unkeyed_corruption("logical key does not match physical filename".to_string());
    }

    let object_id_bytes: [u8; 16] = fixed[64..80].try_into().unwrap();
    let object_id = is_uuid_v4(&object_id_bytes).then(|| format_uuid(&object_id_bytes));

    let content_type_bytes = &metadata_bytes[key_end..expected_metadata_len];
    let content_type = match HeaderValue::from_bytes(content_type_bytes)
        .ok()
        .and_then(|header| ContentType::try_from_header(&header).ok())
    {
        Some(content_type) => content_type,
        None => return keyed_corruption(key, "content-type metadata is invalid".to_string(), object_id),
    };
    let stored_crc = u32::from_le_bytes(metadata_bytes[12..16].try_into().unwrap());
    let mut crc_bytes = metadata_bytes.clone();
    crc_bytes[12..16].fill(0);
    if crc32_iso_hdlc(&crc_bytes) != stored_crc {
        return keyed_corruption(key, "metadata CRC mismatch".to_string(), object_id);
    }
    let Some(object_id) = object_id else {
        return keyed_corruption(key, "object identifier is not a version 4 UUID".to_string(), None);
    };
    let payload_len = u64::from_le_bytes(fixed[16..24].try_into().unwrap());
    let Some(expected_file_len) = (metadata_len as u64).checked_add(payload_len) else {
        return keyed_corruption(key, "stored file length overflows u64".to_string(), Some(object_id));
    };
    if expected_file_len != file_len {
        return keyed_corruption(
            key,
            format!("file length is {file_len}, expected {expected_file_len}"),
            Some(object_id),
        );
    }
    ParsedObject::Healthy {
        key: key.clone(),
        metadata: ObjectMetadata::new(key, content_type, payload_len),
        metadata_len,
        object_id,
    }
}

fn unkeyed_corruption(reason: String) -> ParsedObject {
    ParsedObject::Corrupt { key: None, reason, object_id: None }
}

fn keyed_corruption(key: Key, reason: String, object_id: Option<String>) -> ParsedObject {
    ParsedObject::Corrupt { key: Some(key), reason, object_id }
}

fn read_exact_at(file: &File, mut buffer: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    while !buffer.is_empty() {
        match file.read_at(buffer, offset) {
            Ok(0) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read")),
            Ok(count) => {
                offset = offset.checked_add(count as u64).ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidInput, "file offset overflow")
                })?;
                buffer = &mut buffer[count..];
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn metadata_length(key: &Key, content_type: &ContentType) -> Result<usize, StoreError> {
    let key_len = key.as_str().len();
    let content_type_len = content_type.as_header_value().as_bytes().len();
    if key_len == 0 || key_len > MAX_KEY_LEN || content_type_len == 0 || content_type_len > MAX_CONTENT_TYPE_LEN {
        return Err(StoreError::new(StoreErrorKind::InvalidRequest, "Object metadata exceeds format limits"));
    }
    let metadata_len = FIXED_METADATA_LEN
        .checked_add(key_len)
        .and_then(|length| length.checked_add(content_type_len))
        .ok_or_else(|| StoreError::new(StoreErrorKind::InvalidRequest, "Metadata length overflow"))?;
    if !(MIN_METADATA_LEN..=MAX_METADATA_LEN).contains(&metadata_len) {
        return Err(StoreError::new(StoreErrorKind::InvalidRequest, "Metadata length exceeds format limits"));
    }
    Ok(metadata_len)
}

fn create_part_file(part_dir: &Path, metadata_len: usize) -> Result<(PathBuf, File), StoreError> {
    for _ in 0..16 {
        let id = random_hex_id()?;
        let path = part_dir.join(format!("{id}.part"));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .read(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        match options.open(&path) {
            Ok(mut file) => {
                if let Err(error) = file
                    .set_len(metadata_len as u64)
                    .and_then(|_| file.seek(SeekFrom::Start(metadata_len as u64)).map(|_| ()))
                {
                    let _ = fs::remove_file(&path);
                    return Err(io_error("reserve upload metadata region", error));
                }
                return Ok((path, file));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(io_error("create exclusive upload file", error)),
        }
    }
    Err(unavailable("Could not allocate a unique upload filename"))
}

fn encode_metadata(
    key: &Key,
    content_type: &ContentType,
    payload_len: u64,
    created_at_ms: u64,
    payload_digest: &[u8; 32],
    object_id: &[u8; 16],
    metadata_len: usize,
) -> Result<Vec<u8>, StoreError> {
    if metadata_length(key, content_type)? != metadata_len {
        return Err(StoreError::new(StoreErrorKind::Internal, "Reserved metadata length changed"));
    }
    let mut metadata = vec![0_u8; metadata_len];
    metadata[0..2].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
    metadata[2..10].copy_from_slice(MAGIC);
    metadata[10..12].copy_from_slice(&(metadata_len as u16).to_le_bytes());
    metadata[16..24].copy_from_slice(&payload_len.to_le_bytes());
    metadata[24..32].copy_from_slice(&created_at_ms.to_le_bytes());
    metadata[32..64].copy_from_slice(payload_digest);
    metadata[64..80].copy_from_slice(object_id);
    let key_bytes = key.as_str().as_bytes();
    let content_type_bytes = content_type.as_header_value().as_bytes();
    metadata[80..82].copy_from_slice(&(key_bytes.len() as u16).to_le_bytes());
    metadata[82..84].copy_from_slice(&(content_type_bytes.len() as u16).to_le_bytes());
    metadata[84..84 + key_bytes.len()].copy_from_slice(key_bytes);
    metadata[84 + key_bytes.len()..].copy_from_slice(content_type_bytes);
    let crc = crc32_iso_hdlc(&metadata);
    metadata[12..16].copy_from_slice(&crc.to_le_bytes());
    Ok(metadata)
}

fn crc32_iso_hdlc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xedb8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

fn random_object_id() -> Result<[u8; 16], StoreError> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| io_error("generate object identifier", error))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes)
}

fn random_hex_id() -> Result<String, StoreError> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .map_err(|error| io_error("generate upload identifier", error))?;
    Ok(hex(&bytes))
}

fn is_uuid_v4(bytes: &[u8; 16]) -> bool {
    bytes[6] >> 4 == 4 && bytes[8] >> 6 == 2
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let value = hex(bytes);
    format!("{}-{}-{}-{}-{}", &value[..8], &value[8..12], &value[12..16], &value[16..20], &value[20..])
}

fn key_digest(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    hex(&digest)
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn is_object_filename(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".obj") else {
        return false;
    };
    stem.len() == 64 && stem.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn resolve_range(range: ReadRange, total: u64) -> Result<Option<ReadSpan>, StoreError> {
    let span = match range {
        ReadRange::Closed { start, end } => {
            if end < start {
                return Err(StoreError::new(StoreErrorKind::InvalidRequest, "Range end precedes range start"));
            }
            if start >= total {
                return Ok(None);
            }
            let end = end.min(total - 1);
            let size = end.checked_sub(start).and_then(|size| size.checked_add(1)).ok_or_else(|| {
                StoreError::new(StoreErrorKind::Internal, "Range size overflow")
            })?;
            ReadSpan::new(start, size)
        }
        ReadRange::From { start } => {
            if start >= total {
                return Ok(None);
            }
            ReadSpan::new(start, total - start)
        }
        ReadRange::Suffix { length } => {
            if length == 0 || total == 0 {
                return Ok(None);
            }
            let size = length.min(total);
            ReadSpan::new(total - size, size)
        }
    };
    Ok(Some(span))
}

fn emit_event(
    journal_path: &Path,
    event: &str,
    key: Option<&str>,
    file: Option<&str>,
    reason: &str,
    object_id: Option<&str>,
) {
    let mut record = Map::new();
    record.insert("time".to_string(), Value::String(rfc3339_now()));
    record.insert("event".to_string(), Value::String(event.to_string()));
    if let Some(key) = key {
        record.insert("key".to_string(), Value::String(key.to_string()));
    }
    if let Some(file) = file {
        record.insert("file".to_string(), Value::String(file.to_string()));
    }
    record.insert("reason".to_string(), Value::String(reason.to_string()));
    if let Some(object_id) = object_id {
        record.insert("object_id".to_string(), Value::String(object_id.to_string()));
    }
    let line = serde_json::to_string(&Value::Object(record)).unwrap_or_else(|_| {
        json!({"time": rfc3339_now(), "event": event, "reason": reason}).to_string()
    });
    eprintln!("{line}");
    match OpenOptions::new().create(true).append(true).open(journal_path) {
        Ok(mut journal) => {
            if let Err(error) = writeln!(journal, "{line}") {
                eprintln!("failed to append storage event journal {}: {error}", journal_path.display());
            }
        }
        Err(error) => eprintln!("failed to open storage event journal {}: {error}", journal_path.display()),
    }
}

fn rfc3339_now() -> String {
    let duration = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = duration.as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = second_of_day / 3_600;
    let minute = (second_of_day % 3_600) / 60;
    let second = second_of_day % 60;
    format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{:03}Z",
        duration.subsec_millis()
    )
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

fn unavailable(detail: impl Into<String>) -> StoreError {
    StoreError::new(StoreErrorKind::Unavailable, detail)
}

fn io_error(action: &str, error: std::io::Error) -> StoreError {
    unavailable(format!("Could not {action}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static ROOT_ID: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let id = ROOT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("journey-fs-store-{}-{id}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn config(&self) -> FilesystemStoreConfig {
            FilesystemStoreConfig::new(&self.0)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn content_type(value: &str) -> ContentType {
        ContentType::try_from_header(&HeaderValue::from_str(value).unwrap()).unwrap()
    }

    async fn put_bytes(store: &FilesystemStore, key: &str, content_type_value: &str, bytes: &[u8], condition: PutCondition) {
        let mut context = store
            .put_context(Key::new(key).unwrap(), content_type(content_type_value), condition)
            .await
            .unwrap();
        context.append(&Bytes::copy_from_slice(bytes)).await.unwrap();
        store.put(context).await.unwrap();
    }

    async fn read_bytes(mut read: ReadObject<FilesystemObjectReader>) -> Vec<u8> {
        let mut buffer = BytesMut::zeroed(3);
        let mut result = Vec::new();
        loop {
            let count = read.object_mut().read(&mut buffer).await.unwrap();
            if count == 0 {
                break;
            }
            result.extend_from_slice(&buffer[..count]);
        }
        result
    }

    fn found(result: GetResult<FilesystemObjectReader>) -> ReadObject<FilesystemObjectReader> {
        match result {
            GetResult::Found(read) => read,
            GetResult::Unsatisfiable { .. } => panic!("expected a readable span"),
        }
    }

    #[test]
    fn key_and_content_type_limits_count_utf8_bytes() {
        assert!(Key::new(&"é".repeat(512)).is_ok());
        assert!(Key::new(&"é".repeat(513)).is_err());
        assert!(ContentType::try_from_header(&HeaderValue::from_bytes(&[b'a'; 128]).unwrap()).is_ok());
        assert!(ContentType::try_from_header(&HeaderValue::from_bytes(&[b'a'; 129]).unwrap()).is_err());
    }

    #[test]
    fn crc_uses_iso_hdlc_parameters() {
        assert_eq!(crc32_iso_hdlc(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn metadata_bounds_match_minimum_and_maximum_v1_headers() {
        assert_eq!(metadata_length(&Key::new("k").unwrap(), &content_type("x")).unwrap(), 86);
        let largest_key = Key::new(&"k".repeat(1_024)).unwrap();
        let largest_content_type = ContentType::try_from_header(&HeaderValue::from_bytes(&[b'x'; 128]).unwrap()).unwrap();
        assert_eq!(metadata_length(&largest_key, &largest_content_type).unwrap(), 1_236);
    }

    #[tokio::test]
    async fn upload_header_range_snapshot_and_restart_round_trip() {
        let root = TestRoot::new();
        fs::create_dir_all(root.0.join("part")).unwrap();
        fs::write(root.0.join("part/stale.part"), b"incomplete").unwrap();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        assert_eq!(fs::read_dir(root.0.join("part")).unwrap().count(), 0);
        put_bytes(&store, "photos/猫", "image/jpeg; q=1", b"0123456789", PutCondition::Unconditional).await;

        let key = Key::new("photos/猫").unwrap();
        let captured = found(
            store
                .get(&key, Some(ReadRange::Closed { start: 2, end: 5 }))
                .await
                .unwrap(),
        );
        let path = store.object_path(&key);
        let file_bytes = fs::read(&path).unwrap();
        let metadata_len = u16::from_le_bytes([file_bytes[10], file_bytes[11]]) as usize;
        assert_eq!(&file_bytes[2..10], b"OBJSTORE");
        assert_eq!(metadata_len, 84 + key.as_str().len() + "image/jpeg; q=1".len());
        assert_eq!(file_bytes.len(), metadata_len + 10);
        let stored_crc = u32::from_le_bytes(file_bytes[12..16].try_into().unwrap());
        let mut crc_input = file_bytes[..metadata_len].to_vec();
        crc_input[12..16].fill(0);
        assert_eq!(stored_crc, crc32_iso_hdlc(&crc_input));
        assert_eq!(&file_bytes[metadata_len..], b"0123456789");

        put_bytes(&store, "photos/猫", "text/plain", b"replacement", PutCondition::Unconditional).await;
        assert_eq!(read_bytes(captured).await, b"2345");
        drop(store);

        let restarted = FilesystemStore::open(root.config()).await.unwrap();
        assert_eq!(restarted.stat(&key).await.unwrap().payload_length(), 11);
        assert_eq!(
            read_bytes(found(restarted.get(&key, None).await.unwrap())).await,
            b"replacement"
        );
    }

    #[tokio::test]
    async fn conditions_are_checked_at_publication_and_failed_uploads_are_removed() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        let key = Key::new("present").unwrap();
        put_bytes(&store, "present", "text/plain", b"original", PutCondition::Unconditional).await;

        let mut rejected = store
            .put_context(key.clone(), content_type("application/json"), PutCondition::CreateOnly)
            .await
            .unwrap();
        rejected.append(&Bytes::from_static(b"wrong")).await.unwrap();
        assert_eq!(store.put(rejected).await.unwrap_err().kind(), StoreErrorKind::PreconditionFailed);
        assert_eq!(fs::read_dir(root.0.join("part")).unwrap().count(), 0);
        assert_eq!(read_bytes(found(store.get(&key, None).await.unwrap())).await, b"original");

        let first = store
            .put_context(Key::new("new").unwrap(), content_type("text/plain"), PutCondition::CreateOnly)
            .await
            .unwrap();
        let second = store
            .put_context(Key::new("new").unwrap(), content_type("text/plain"), PutCondition::CreateOnly)
            .await
            .unwrap();
        let (first, second) = tokio::join!(store.put(first), store.put(second));
        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(
            [first, second]
                .iter()
                .filter(|result| matches!(result, Err(error) if error.kind() == StoreErrorKind::PreconditionFailed))
                .count(),
            1
        );
        assert_eq!(fs::read_dir(root.0.join("part")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn keyed_corruption_is_hidden_from_list_and_can_be_repaired() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        put_bytes(&store, "damaged", "text/plain", b"bytes", PutCondition::Unconditional).await;
        let path = store.object_path(&Key::new("damaged").unwrap());
        drop(store);
        let mut bytes = fs::read(&path).unwrap();
        bytes[12] ^= 0x80;
        fs::write(&path, bytes).unwrap();

        let store = FilesystemStore::open(root.config()).await.unwrap();
        let key = Key::new("damaged").unwrap();
        assert_eq!(store.stat(&key).await.unwrap_err().kind(), StoreErrorKind::Corrupt);
        assert_eq!(store.get(&key, None).await.unwrap_err().kind(), StoreErrorKind::Corrupt);
        let list = store
            .list(ListRequest::new("", None, NonZeroUsize::new(10).unwrap()))
            .await
            .unwrap();
        assert!(list.objects().is_empty());

        put_bytes(&store, "damaged", "text/plain", b"repaired", PutCondition::ReplaceOnly).await;
        assert_eq!(read_bytes(found(store.get(&key, None).await.unwrap())).await, b"repaired");
        let journal = fs::read_to_string(root.0.join("journal")).unwrap();
        assert!(journal.lines().any(|line| line.contains("corrupt_file_skipped")));
        assert!(journal.lines().all(|line| serde_json::from_str::<Value>(line).is_ok()));
    }

    #[tokio::test]
    async fn unknown_version_is_skipped_without_a_recoverable_key() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        put_bytes(&store, "future", "text/plain", b"payload", PutCondition::Unconditional).await;
        let path = store.object_path(&Key::new("future").unwrap());
        drop(store);
        let mut bytes = fs::read(&path).unwrap();
        bytes[0..2].copy_from_slice(&2_u16.to_le_bytes());
        fs::write(path, bytes).unwrap();

        let store = FilesystemStore::open(root.config()).await.unwrap();
        assert_eq!(store.stat(&Key::new("future").unwrap()).await.unwrap_err().kind(), StoreErrorKind::NotFound);
        let journal = fs::read_to_string(root.0.join("journal")).unwrap();
        assert!(journal.lines().any(|line| line.contains("unsupported object format version 2")));
    }

    #[tokio::test]
    async fn shortened_published_file_becomes_keyed_corruption_on_restart() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        put_bytes(&store, "short", "text/plain", b"payload", PutCondition::Unconditional).await;
        let path = store.object_path(&Key::new("short").unwrap());
        drop(store);
        let mut bytes = fs::read(&path).unwrap();
        bytes.pop();
        fs::write(path, bytes).unwrap();

        let store = FilesystemStore::open(root.config()).await.unwrap();
        assert_eq!(store.stat(&Key::new("short").unwrap()).await.unwrap_err().kind(), StoreErrorKind::Corrupt);
    }

    #[tokio::test]
    async fn unindexed_file_at_derived_path_is_preserved_and_reported_as_corrupt() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        let key = Key::new("occupied").unwrap();
        let path = store.object_path(&key);
        fs::write(&path, b"unkeyable external file").unwrap();
        let mut context = store
            .put_context(key, content_type("text/plain"), PutCondition::ReplaceOnly)
            .await
            .unwrap();
        context.append(&Bytes::from_static(b"replacement")).await.unwrap();
        assert_eq!(store.put(context).await.unwrap_err().kind(), StoreErrorKind::Corrupt);
        assert_eq!(fs::read(path).unwrap(), b"unkeyable external file");
        assert_eq!(fs::read_dir(root.0.join("part")).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn deleting_keyed_corruption_removes_file_and_journals_the_event() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        put_bytes(&store, "delete-damaged", "text/plain", b"bytes", PutCondition::Unconditional).await;
        let path = store.object_path(&Key::new("delete-damaged").unwrap());
        drop(store);
        let mut bytes = fs::read(&path).unwrap();
        bytes[12] ^= 1;
        fs::write(&path, bytes).unwrap();

        let store = FilesystemStore::open(root.config()).await.unwrap();
        store.delete(&Key::new("delete-damaged").unwrap()).await.unwrap();
        assert!(!path.exists());
        let journal = fs::read_to_string(root.0.join("journal")).unwrap();
        assert!(journal.lines().any(|line| line.contains("corrupt_object_deleted")));
    }

    #[tokio::test]
    async fn largest_valid_key_and_content_type_fit_the_published_header() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        let key_text = "k".repeat(1_024);
        let content_type_value = "x".repeat(128);
        put_bytes(&store, &key_text, &content_type_value, b"", PutCondition::Unconditional).await;
        let path = store.object_path(&Key::new(&key_text).unwrap());
        assert_eq!(fs::metadata(path).unwrap().len(), 1_236);
    }

    #[tokio::test]
    async fn delete_is_idempotent_and_removes_published_files() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        let key = Key::new("remove-me").unwrap();
        put_bytes(&store, "remove-me", "text/plain", b"payload", PutCondition::Unconditional).await;
        assert!(store.object_path(&key).exists());
        store.delete(&key).await.unwrap();
        store.delete(&key).await.unwrap();
        assert!(!store.object_path(&key).exists());
        assert_eq!(store.stat(&key).await.unwrap_err().kind(), StoreErrorKind::NotFound);
    }

    #[tokio::test]
    async fn storage_root_has_one_active_owner() {
        let root = TestRoot::new();
        let store = FilesystemStore::open(root.config()).await.unwrap();
        let error = FilesystemStore::open(root.config()).await.unwrap_err();
        assert_eq!(error.kind(), StoreErrorKind::Unavailable);
        drop(store);
        assert!(FilesystemStore::open(root.config()).await.is_ok());
    }
}
