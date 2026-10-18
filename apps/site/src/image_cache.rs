use bytes::Bytes;
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize, de::{self, Visitor}};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    fs::{self, OpenOptions},
    io,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::{AtomicU64, Ordering}},
    time::{Duration, SystemTime},
};
use tokio::{fs::File, io::{AsyncReadExt, AsyncWriteExt}, sync::Mutex as AsyncMutex};

use crate::{db::Database, storage::StorageBody};

const CACHE_MAGIC: &[u8; 8] = b"JICACHE1";
const CACHE_VERSION: u32 = 1;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const CACHE_CHUNK_BYTES: usize = 64 * 1024;
static NEXT_PART: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub directory: Option<PathBuf>,
    pub max_bytes: ByteLimit,
    pub max_entry_bytes: ByteLimit,
    pub max_concurrent_fills: usize,
    pub recent_post_count: usize,
    pub eviction_interval: String,
    pub eviction_target_percent: u8,
    pub access_timestamp_interval: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: None,
            max_bytes: ByteLimit(1024 * 1024 * 1024),
            max_entry_bytes: ByteLimit(64 * 1024 * 1024),
            max_concurrent_fills: 4,
            recent_post_count: 5,
            eviction_interval: "1h".to_owned(),
            eviction_target_percent: 90,
            access_timestamp_interval: "1m".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
#[derive(Default)]
pub struct ByteLimit(pub u64);


impl<'de> Deserialize<'de> for ByteLimit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where D: serde::Deserializer<'de> {
        struct ByteLimitVisitor;
        impl<'de> Visitor<'de> for ByteLimitVisitor {
            type Value = ByteLimit;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a positive byte count or a size string such as 64MiB")
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where E: de::Error {
                if value == 0 { return Err(E::custom("byte size must be positive")); }
                Ok(ByteLimit(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where E: de::Error {
                if value <= 0 { return Err(E::custom("byte size must be positive")); }
                Ok(ByteLimit(value as u64))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where E: de::Error {
                journey_performance::parse_size(value).map(ByteLimit).map_err(E::custom)
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where E: de::Error {
                self.visit_str(&value)
            }
        }
        deserializer.deserialize_any(ByteLimitVisitor)
    }
}

impl Settings {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_bytes.0 == 0 || self.max_entry_bytes.0 == 0 {
            return Err("image_cache byte limits must be positive".to_owned());
        }
        if self.max_entry_bytes.0 > self.max_bytes.0 {
            return Err("image_cache.max_entry_bytes must not exceed image_cache.max_bytes".to_owned());
        }
        if self.max_concurrent_fills == 0 {
            return Err("image_cache.max_concurrent_fills must be positive".to_owned());
        }
        if !(1..=99).contains(&self.eviction_target_percent) {
            return Err("image_cache.eviction_target_percent must be between 1 and 99".to_owned());
        }
        parse_interval(&self.eviction_interval, "image_cache.eviction_interval")?;
        parse_interval(&self.access_timestamp_interval, "image_cache.access_timestamp_interval")?;
        Ok(())
    }

    fn runtime(&self) -> Result<RuntimeSettings, String> {
        self.validate()?;
        Ok(RuntimeSettings {
            directory: self.directory.clone().ok_or("image cache directory was not resolved")?,
            max_bytes: self.max_bytes.0,
            max_entry_bytes: self.max_entry_bytes.0,
            max_concurrent_fills: self.max_concurrent_fills,
            recent_post_count: self.recent_post_count,
            eviction_interval: parse_interval(&self.eviction_interval, "image_cache.eviction_interval")?,
            eviction_target_percent: self.eviction_target_percent,
            access_timestamp_interval: parse_interval(&self.access_timestamp_interval, "image_cache.access_timestamp_interval")?,
        })
    }
}

fn parse_interval(value: &str, name: &str) -> Result<Duration, String> {
    let digits_end = value.bytes().take_while(u8::is_ascii_digit).count();
    let (digits, suffix) = value.split_at(digits_end);
    let number = digits.parse::<u64>().ok().filter(|number| *number > 0)
        .ok_or_else(|| format!("{name} must be a positive whole number followed by s, m, h, or d"))?;
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        _ => return Err(format!("{name} must end in s, m, h, or d")),
    };
    let seconds = number.checked_mul(multiplier)
        .ok_or_else(|| format!("{name} is too large"))?;
    let duration = Duration::from_secs(seconds);
    if std::time::Instant::now().checked_add(duration).is_none() {
        return Err(format!("{name} is too large"));
    }
    Ok(duration)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "representation", rename_all = "snake_case")]
enum Representation {
    Original,
    QualityImage { quality: u8 },
    Thumbnail,
    ReducedImage { max_edge: u32, max_bytes: Option<u64>, video_thumbnail: bool },
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Identity {
    storage_key: String,
    representation: Representation,
}

impl Identity {
    pub fn original(storage_key: &str) -> Self {
        Self { storage_key: storage_key.to_owned(), representation: Representation::Original }
    }

    pub fn thumbnail(storage_key: &str) -> Self {
        Self { storage_key: storage_key.to_owned(), representation: Representation::Thumbnail }
    }

    pub fn quality_image(storage_key: &str, quality: crate::storage::ImageQuality) -> Self {
        Self { storage_key: storage_key.to_owned(), representation: Representation::QualityImage { quality: quality.value() } }
    }

    pub fn reduced_image(
        storage_key: &str,
        max_edge: u32,
        max_bytes: Option<u64>,
        video_thumbnail: bool,
    ) -> Self {
        Self {
            storage_key: storage_key.to_owned(),
            representation: Representation::ReducedImage { max_edge, max_bytes, video_thumbnail },
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct EntryMetadata {
    version: u32,
    identity: Identity,
    content_type: String,
    payload_length: u64,
}

struct RuntimeSettings {
    directory: PathBuf,
    max_bytes: u64,
    max_entry_bytes: u64,
    max_concurrent_fills: usize,
    recent_post_count: usize,
    eviction_interval: Duration,
    eviction_target_percent: u8,
    access_timestamp_interval: Duration,
}

struct Accounting {
    completed_bytes: u64,
    reserved_bytes: u64,
    active_fills: usize,
    global_generation: u64,
    key_generations: HashMap<String, u64>,
}

pub struct ImageCache {
    cache_directory: PathBuf,
    parts_directory: PathBuf,
    settings: RuntimeSettings,
    accounting: Mutex<Accounting>,
    operation_gate: AsyncMutex<()>,
    _process_lock: fs::File,
}

pub struct CacheHit {
    pub content_type: String,
    pub payload_length: u64,
    pub body: StorageBody,
}

#[derive(Clone, Copy)]
pub struct FillGeneration {
    global: u64,
    key: u64,
}

impl ImageCache {
    pub async fn open(settings: &Settings) -> Result<Arc<Self>, Box<dyn Error + Send + Sync>> {
        let settings = settings.runtime().map_err(io::Error::other)?;
        let directory = settings.directory.clone();
        let cache_directory = directory.join("cache");
        let parts_directory = directory.join("parts");
        prepare_cache_directory(&directory).await?;
        let lock_path = directory.join(".lock");
        let process_lock = acquire_process_lock(&lock_path)?;
        tokio::fs::create_dir_all(&cache_directory).await?;
        tokio::fs::create_dir_all(&parts_directory).await?;
        set_private_directory(&cache_directory).await?;
        set_private_directory(&parts_directory).await?;
        clear_abandoned_parts(&parts_directory).await?;
        let (completed_bytes, completed_entries) = scan_occupancy(&cache_directory).await?;
        log::info!(
            "image_cache_started directory={:?} completed_entries={completed_entries} completed_bytes={completed_bytes} max_bytes={}",
            directory,
            settings.max_bytes,
        );
        Ok(Arc::new(Self {
            cache_directory,
            parts_directory,
            accounting: Mutex::new(Accounting {
                completed_bytes,
                reserved_bytes: 0,
                active_fills: 0,
                global_generation: 0,
                key_generations: HashMap::new(),
            }),
            settings,
            operation_gate: AsyncMutex::new(()),
            _process_lock: process_lock,
        }))
    }

    pub fn eviction_interval(&self) -> Duration { self.settings.eviction_interval }

    pub fn startup_exceeds_limit(&self) -> bool {
        let accounting = self.accounting();
        accounting.completed_bytes > self.settings.max_bytes
    }

    pub fn fill_generation(&self, identity: &Identity) -> FillGeneration {
        let accounting = self.accounting();
        let key = accounting.key_generations.get(&identity.storage_key).copied().unwrap_or(0);
        FillGeneration { global: accounting.global_generation, key }
    }

    pub async fn lookup(&self, identity: &Identity, head: bool) -> Option<CacheHit> {
        let path = self.entry_path(identity);
        let mut file = match File::open(&path).await {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
            Err(error) => {
                log::warn!("image_cache_open_failed path={:?}: {error}", path);
                return None;
            }
        };
        let parsed = read_header(&mut file).await;
        let (metadata, header_length) = match parsed {
            Ok(value) => value,
            Err(error) => {
                log::warn!("image_cache_entry_invalid path={:?}: {error}", path);
                drop(file);
                self.remove_invalid_entry(&path).await;
                return None;
            }
        };
        let file_length = match file.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                log::warn!("image_cache_metadata_failed path={:?}: {error}", path);
                return None;
            }
        };
        let valid = metadata.version == CACHE_VERSION
            && metadata.identity == *identity
            && metadata.payload_length > 0
            && is_image_content_type(&metadata.content_type)
            && (!matches!(identity.representation, Representation::QualityImage { .. }) || metadata.content_type == "image/jpeg")
            && HeaderValueCheck::valid(&metadata.content_type)
            && header_length.checked_add(metadata.payload_length) == Some(file_length);
        if !valid {
            log::warn!("image_cache_entry_invalid path={:?}: metadata did not match the requested identity or file length", path);
            drop(file);
            self.remove_invalid_entry(&path).await;
            return None;
        }

        self.touch_if_due(path.clone(), file.metadata().await.ok().and_then(|metadata| metadata.modified().ok())).await;
        let body: StorageBody = if head {
            Box::pin(stream::empty())
        } else {
            let body = stream::try_unfold(file, |mut file| async move {
                let mut buffer = vec![0; CACHE_CHUNK_BYTES];
                let count = file.read(&mut buffer).await?;
                if count == 0 {
                    Ok(None)
                } else {
                    buffer.truncate(count);
                    Ok(Some((Bytes::from(buffer), file)))
                }
            });
            Box::pin(body)
        };
        log::debug!("image_cache_hit key={:?} representation={:?}", identity.storage_key, identity.representation);
        Some(CacheHit {
            content_type: metadata.content_type,
            payload_length: metadata.payload_length,
            body,
        })
    }

    pub async fn cache_body(
        self: &Arc<Self>,
        identity: Identity,
        generation: FillGeneration,
        status: http::StatusCode,
        content_type: Option<&http::HeaderValue>,
        content_length: Option<u64>,
        body: StorageBody,
    ) -> StorageBody {
        if status != http::StatusCode::OK {
            return body;
        }
        let Some(content_type) = content_type.and_then(|value| value.to_str().ok()).map(str::to_owned) else {
            return body;
        };
        let Some(content_length) = content_length.filter(|length| *length > 0 && *length <= self.settings.max_entry_bytes) else {
            return body;
        };
        if !is_image_content_type(&content_type) || !HeaderValueCheck::valid(&content_type) {
            return body;
        }
        let part_path = self.parts_directory.join(format!(
            "part-{}-{}",
            std::process::id(),
            NEXT_PART.fetch_add(1, Ordering::Relaxed),
        ));
        let reservation = match self.reserve(&identity, generation, content_length, part_path.clone()) {
            Some(reservation) => reservation,
            None => {
                log::debug!("image_cache_fill_bypassed key={:?} reason=capacity_or_concurrency", identity.storage_key);
                return body;
            }
        };
        let mut file_options = tokio::fs::OpenOptions::new();
        file_options.write(true).create_new(true);
        #[cfg(unix)]
        file_options.mode(0o600);
        let mut file = match file_options.open(&part_path).await {
            Ok(file) => file,
            Err(error) => {
                log::warn!("image_cache_part_create_failed path={:?}: {error}", part_path);
                return body;
            }
        };
        let metadata = EntryMetadata {
            version: CACHE_VERSION,
            identity: identity.clone(),
            content_type,
            payload_length: content_length,
        };
        let metadata_bytes = match serde_json::to_vec(&metadata) {
            Ok(bytes) if bytes.len() <= MAX_METADATA_BYTES => bytes,
            Ok(_) => {
                log::warn!("image_cache_metadata_too_large key={:?}", identity.storage_key);
                return body;
            }
            Err(error) => {
                log::warn!("image_cache_metadata_encode_failed key={:?}: {error}", identity.storage_key);
                return body;
            }
        };
        if let Err(error) = async {
            file.write_all(CACHE_MAGIC).await?;
            file.write_all(&(metadata_bytes.len() as u32).to_le_bytes()).await?;
            file.write_all(&metadata_bytes).await
        }.await {
            log::warn!("image_cache_part_header_write_failed path={:?}: {error}", part_path);
            return body;
        }
        log::debug!("image_cache_fill_started key={:?} bytes={content_length}", identity.storage_key);
        let state = FillState {
            upstream: body,
            file: Some(file),
            reservation: Some(reservation),
            expected: content_length,
            written: 0,
            terminated: false,
        };
        Box::pin(stream::unfold(state, |mut state| async move {
            if state.terminated {
                return None;
            }
            match state.upstream.next().await {
                Some(Ok(bytes)) => {
                    state.written = state.written.saturating_add(bytes.len() as u64);
                    if state.file.is_some() && state.written <= state.expected {
                        let result = state.file.as_mut().expect("cache fill file exists").write_all(&bytes).await;
                        if let Err(error) = result {
                            log::warn!("image_cache_part_write_failed: {error}");
                            state.abandon();
                        } else if state.written == state.expected {
                            // HTTP/1 servers may stop polling the body after the declared
                            // Content-Length has been sent, without polling once more for EOF.
                            state.finish().await;
                        }
                    } else if state.file.is_some() {
                        log::warn!("image_cache_fill_length_exceeded expected={} actual={}", state.expected, state.written);
                        state.abandon();
                    }
                    Some((Ok(bytes), state))
                }
                Some(Err(error)) => {
                    log::warn!("image_cache_upstream_stream_failed: {error}");
                    state.abandon();
                    state.terminated = true;
                    Some((Err(error), state))
                }
                None => {
                    state.finish().await;
                    None
                }
            }
        }))
    }

    pub async fn clear(&self, storage_key: Option<&str>) -> io::Result<usize> {
        let _gate = self.operation_gate.lock().await;
        {
            let mut accounting = self.accounting();
            match storage_key {
                Some(key) => {
                    let generation = accounting.key_generations.entry(key.to_owned()).or_insert(0);
                    *generation = generation.wrapping_add(1);
                }
                None => {
                    accounting.global_generation = accounting.global_generation.wrapping_add(1);
                    accounting.key_generations.clear();
                }
            }
        }
        let mut removed_entries = 0;
        let mut removed_bytes = 0_u64;
        let mut entries = tokio::fs::read_dir(&self.cache_directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let is_match = match storage_key {
                None => true,
                Some(key) => read_any_metadata(&path).await
                    .ok()
                    .is_some_and(|metadata| metadata.identity.storage_key == key),
            };
            if !is_match { continue; }
            if let Some(bytes) = self.remove_completed_entry(&path).await? {
                removed_entries += 1;
                removed_bytes = removed_bytes.saturating_add(bytes);
            }
        }
        log::info!("image_cache_cleared key={:?} entries={removed_entries} bytes={removed_bytes}", storage_key);
        Ok(removed_entries)
    }

    pub async fn evict(&self, database: &Database, force: bool) -> Result<(), Box<dyn Error + Send + Sync>> {
        let recent_keys = if self.settings.recent_post_count == 0 {
            Vec::new()
        } else {
            database.recent_published_media_keys(self.settings.recent_post_count).await
                .map_err(io::Error::other)?
        };
        let recent_keys: std::collections::HashSet<String> = recent_keys.into_iter().collect();
        let _gate = self.operation_gate.lock().await;
        let entries = scan_entries(&self.cache_directory).await?;
        let completed_bytes = entries.iter().fold(0_u64, |total, entry| total.saturating_add(entry.accounted_bytes));
        {
            let mut accounting = self.accounting();
            accounting.completed_bytes = completed_bytes;
        }
        let mut total_bytes = completed_bytes.saturating_add(self.accounting().reserved_bytes);
        let target_bytes = ((self.settings.max_bytes as u128) * self.settings.eviction_target_percent as u128 / 100) as u64;
        if !force && total_bytes <= target_bytes {
            return Ok(());
        }
        let before = total_bytes;
        let mut candidates = entries;
        candidates.sort_by(|left, right| {
            let left_recent = left.storage_key.as_ref().is_some_and(|key| recent_keys.contains(key));
            let right_recent = right.storage_key.as_ref().is_some_and(|key| recent_keys.contains(key));
            left_recent.cmp(&right_recent).then_with(|| left.modified.cmp(&right.modified))
        });
        let mut removed_entries = 0;
        let mut removed_bytes = 0_u64;
        for entry in candidates {
            if total_bytes <= target_bytes { break; }
            match tokio::fs::remove_file(&entry.path).await {
                Ok(()) => {
                    removed_entries += 1;
                    removed_bytes = removed_bytes.saturating_add(entry.accounted_bytes);
                    total_bytes = total_bytes.saturating_sub(entry.accounted_bytes);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => log::warn!("image_cache_eviction_failed path={:?}: {error}", entry.path),
            }
        }
        {
            let mut accounting = self.accounting();
            accounting.completed_bytes = total_bytes.saturating_sub(accounting.reserved_bytes);
        }
        log::info!("image_cache_eviction_completed before_bytes={before} after_bytes={total_bytes} target_bytes={target_bytes} removed_entries={removed_entries} removed_bytes={removed_bytes}");
        Ok(())
    }

    fn entry_path(&self, identity: &Identity) -> PathBuf {
        self.cache_directory.join(identity_filename(identity))
    }

    fn reserve(self: &Arc<Self>, identity: &Identity, generation: FillGeneration, bytes: u64, part_path: PathBuf) -> Option<Reservation> {
        let mut accounting = self.accounting();
        if accounting.active_fills >= self.settings.max_concurrent_fills
            || accounting.global_generation != generation.global
            || accounting.key_generations.get(&identity.storage_key).copied().unwrap_or(0) != generation.key
            || accounting.completed_bytes
                .saturating_add(accounting.reserved_bytes)
                .saturating_add(bytes) > self.settings.max_bytes
        {
            return None;
        }
        accounting.active_fills += 1;
        accounting.reserved_bytes = accounting.reserved_bytes.saturating_add(bytes);
        Some(Reservation {
            cache: Arc::clone(self),
            identity: identity.clone(),
            expected: bytes,
            part_path,
            global_generation: generation.global,
            key_generation: generation.key,
            complete: false,
        })
    }

    async fn remove_invalid_entry(&self, path: &Path) {
        let _gate = self.operation_gate.lock().await;
        if let Err(error) = self.remove_completed_entry(path).await
            && error.kind() != io::ErrorKind::NotFound {
                log::warn!("image_cache_invalid_entry_remove_failed path={:?}: {error}", path);
            }
    }

    async fn remove_completed_entry(&self, path: &Path) -> io::Result<Option<u64>> {
        let accounted = match tokio::fs::metadata(path).await {
            Ok(file_metadata) => read_any_metadata(path).await.ok()
                .filter(|metadata| metadata.version == CACHE_VERSION
                    && metadata.payload_length > 0
                    && 12_u64.saturating_add(metadata.payload_length) <= file_metadata.len())
                .map(|metadata| metadata.payload_length)
                .unwrap_or(file_metadata.len()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        tokio::fs::remove_file(path).await?;
        let mut accounting = self.accounting();
        accounting.completed_bytes = accounting.completed_bytes.saturating_sub(accounted);
        Ok(Some(accounted))
    }

    async fn touch_if_due(&self, path: PathBuf, modified: Option<SystemTime>) {
        let due = modified.and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|elapsed| elapsed >= self.settings.access_timestamp_interval);
        if !due { return; }
        let result = tokio::task::spawn_blocking(move || {
            let file = fs::OpenOptions::new().write(true).open(path)?;
            file.set_times(fs::FileTimes::new().set_modified(SystemTime::now()))
        }).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("image_cache_access_timestamp_failed: {error}"),
            Err(error) => log::warn!("image_cache_access_timestamp_task_failed: {error}"),
        }
    }

    fn accounting(&self) -> std::sync::MutexGuard<'_, Accounting> {
        self.accounting.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

struct Reservation {
    cache: Arc<ImageCache>,
    identity: Identity,
    expected: u64,
    part_path: PathBuf,
    global_generation: u64,
    key_generation: u64,
    complete: bool,
}

impl Reservation {
    async fn publish(mut self) -> io::Result<()> {
        let cache = Arc::clone(&self.cache);
        let _gate = cache.operation_gate.lock().await;
        let generation_matches = {
            let accounting = cache.accounting();
            accounting.global_generation == self.global_generation
                && accounting.key_generations.get(&self.identity.storage_key).copied().unwrap_or(0) == self.key_generation
        };
        if !generation_matches {
            log::debug!("image_cache_fill_discarded_after_clear key={:?}", self.identity.storage_key);
            return Ok(());
        }
        let target = cache.entry_path(&self.identity);
        let existing_valid = match File::open(&target).await {
            Ok(mut existing) => {
                match read_header(&mut existing).await {
                    Ok((metadata, header_length)) => {
                        let file_length = existing.metadata().await?.len();
                        metadata.version == CACHE_VERSION
                            && metadata.identity == self.identity
                            && metadata.payload_length > 0
                            && is_image_content_type(&metadata.content_type)
                            && HeaderValueCheck::valid(&metadata.content_type)
                            && header_length.checked_add(metadata.payload_length) == Some(file_length)
                    }
                    Err(_) => false,
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error),
        };
        if existing_valid {
            let _ = tokio::fs::remove_file(&self.part_path).await;
            self.finish_accounting(false);
            log::debug!("image_cache_duplicate_fill_discarded key={:?}", self.identity.storage_key);
            return Ok(());
        }
        if tokio::fs::metadata(&target).await.is_ok() {
            cache.remove_completed_entry(&target).await?;
        }
        tokio::fs::hard_link(&self.part_path, &target).await?;
        if let Err(error) = tokio::fs::remove_file(&self.part_path).await {
            log::warn!("image_cache_published_part_cleanup_failed path={:?}: {error}", self.part_path);
        }
        self.finish_accounting(true);
        log::debug!("image_cache_fill_published key={:?} bytes={}", self.identity.storage_key, self.expected);
        Ok(())
    }

    fn finish_accounting(&mut self, published: bool) {
        if self.complete { return; }
        let mut accounting = self.cache.accounting();
        accounting.reserved_bytes = accounting.reserved_bytes.saturating_sub(self.expected);
        accounting.active_fills = accounting.active_fills.saturating_sub(1);
        if published {
            accounting.completed_bytes = accounting.completed_bytes.saturating_add(self.expected);
        }
        self.complete = true;
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.complete {
            let mut accounting = self.cache.accounting();
            accounting.reserved_bytes = accounting.reserved_bytes.saturating_sub(self.expected);
            accounting.active_fills = accounting.active_fills.saturating_sub(1);
        }
        let _ = fs::remove_file(&self.part_path);
    }
}

struct FillState {
    upstream: StorageBody,
    file: Option<File>,
    reservation: Option<Reservation>,
    expected: u64,
    written: u64,
    terminated: bool,
}

impl FillState {
    fn abandon(&mut self) {
        self.file.take();
        self.reservation.take();
    }

    async fn finish(&mut self) {
        if self.written != self.expected {
            log::warn!("image_cache_fill_length_mismatch expected={} actual={}", self.expected, self.written);
            self.abandon();
            return;
        }
        let Some(mut file) = self.file.take() else { return; };
        if let Err(error) = file.flush().await {
            log::warn!("image_cache_part_flush_failed: {error}");
            self.reservation.take();
            return;
        }
        if let Err(error) = file.sync_all().await {
            log::warn!("image_cache_part_sync_failed: {error}");
            self.reservation.take();
            return;
        }
        drop(file);
        if let Some(reservation) = self.reservation.take()
            && let Err(error) = reservation.publish().await {
                log::warn!("image_cache_publish_failed: {error}");
            }
    }
}

fn acquire_process_lock(path: &Path) -> io::Result<fs::File> {
    let file = OpenOptions::new().read(true).write(true).truncate(true).create(true).mode(0o600).open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let result = unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&file), libc::LOCK_EX | libc::LOCK_NB) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            let directory = path.parent().unwrap_or(Path::new("."));
            return Err(io::Error::other(format!("another journey-site process is using image cache directory {}", directory.display())));
        }
        return Err(error);
    }
    Ok(file)
}

async fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    tokio::fs::set_permissions(path, fs::Permissions::from_mode(0o700)).await?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

async fn prepare_cache_directory(path: &Path) -> io::Result<()> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => {
            let metadata = tokio::fs::metadata(path).await?;
            if !metadata.is_dir() {
                return Err(io::Error::new(io::ErrorKind::InvalidInput, "image cache path must be a directory"));
            }
            #[cfg(unix)]
            if metadata.permissions().mode() & 0o777 != 0o700 {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, format!(
                    "existing image cache directory must have permissions 0700: {}",
                    path.display(),
                )));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            tokio::fs::create_dir_all(path).await?;
            set_private_directory(path).await?;
        }
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn clear_abandoned_parts(parts_directory: &Path) -> io::Result<()> {
    let mut entries = tokio::fs::read_dir(parts_directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = tokio::fs::symlink_metadata(&path).await?;
        if metadata.is_dir() {
            tokio::fs::remove_dir_all(&path).await?;
        } else {
            tokio::fs::remove_file(&path).await?;
        }
    }
    Ok(())
}

fn identity_filename(identity: &Identity) -> String {
    let bytes = serde_json::to_vec(identity).expect("cache identity serialization is infallible");
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn read_header(file: &mut File) -> io::Result<(EntryMetadata, u64)> {
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic).await?;
    if &magic != CACHE_MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "cache entry magic is invalid"));
    }
    let mut length = [0_u8; 4];
    file.read_exact(&mut length).await?;
    let metadata_length = u32::from_le_bytes(length) as usize;
    if metadata_length == 0 || metadata_length > MAX_METADATA_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "cache entry metadata length is invalid"));
    }
    let mut bytes = vec![0; metadata_length];
    file.read_exact(&mut bytes).await?;
    let metadata = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((metadata, 12 + metadata_length as u64))
}

async fn read_any_metadata(path: &Path) -> io::Result<EntryMetadata> {
    let mut file = File::open(path).await?;
    read_header(&mut file).await.map(|(metadata, _)| metadata)
}

struct ScannedEntry {
    path: PathBuf,
    storage_key: Option<String>,
    modified: Option<SystemTime>,
    accounted_bytes: u64,
}

async fn scan_occupancy(directory: &Path) -> io::Result<(u64, usize)> {
    let entries = scan_entries(directory).await?;
    let bytes = entries.iter().fold(0_u64, |total, entry| total.saturating_add(entry.accounted_bytes));
    Ok((bytes, entries.len()))
}

async fn scan_entries(directory: &Path) -> io::Result<Vec<ScannedEntry>> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    let mut scanned = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(metadata) if metadata.file_type().is_symlink() => {
                tokio::fs::remove_file(&path).await?;
                continue;
            }
            Ok(metadata) if metadata.is_dir() => {
                log::warn!("image_cache_unexpected_directory_removed path={:?}", path);
                tokio::fs::remove_dir_all(&path).await?;
                continue;
            }
            Ok(_) => {
                tokio::fs::remove_file(&path).await?;
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = read_any_metadata(&path).await.ok();
        let accounted_bytes = match &metadata {
            Some(metadata) if metadata.version == CACHE_VERSION
                && metadata.payload_length > 0
                && 12_u64.saturating_add(metadata.payload_length) <= file_metadata.len() => metadata.payload_length,
            _ => file_metadata.len(),
        };
        scanned.push(ScannedEntry {
            path,
            storage_key: metadata.map(|metadata| metadata.identity.storage_key),
            modified: file_metadata.modified().ok(),
            accounted_bytes,
        });
    }
    Ok(scanned)
}

fn is_image_content_type(content_type: &str) -> bool {
    let media_type = content_type.split(';').next().map(str::trim);
    let Some((kind, subtype)) = media_type.and_then(|value| value.split_once('/')) else {
        return false;
    };
    kind.eq_ignore_ascii_case("image")
        && !subtype.contains('/')
        && !subtype.is_empty()
        && subtype.bytes().all(is_mime_token_byte)
}

fn is_mime_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

struct HeaderValueCheck;

impl HeaderValueCheck {
    fn valid(value: &str) -> bool { http::HeaderValue::from_str(value).is_ok() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use std::{
        os::unix::fs::PermissionsExt,
        time::{Duration, UNIX_EPOCH},
    };

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "journey-image-cache-test-{}-{}",
                std::process::id(),
                NEXT_PART.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn test_settings(directory: &Path) -> Settings {
        Settings {
            enabled: true,
            directory: Some(directory.join("cache-root")),
            max_bytes: ByteLimit(1024),
            max_entry_bytes: ByteLimit(1024),
            max_concurrent_fills: 4,
            recent_post_count: 5,
            eviction_interval: "1h".to_owned(),
            eviction_target_percent: 90,
            access_timestamp_interval: "1m".to_owned(),
        }
    }

    fn upstream(bytes: &'static [u8]) -> StorageBody {
        Box::pin(stream::iter([Ok::<_, io::Error>(Bytes::from_static(bytes))]))
    }

    async fn collect_body(mut body: StorageBody) -> io::Result<Vec<u8>> {
        let mut collected = Vec::new();
        while let Some(chunk) = body.next().await {
            collected.extend_from_slice(&chunk?);
        }
        Ok(collected)
    }

    async fn entry_count(directory: &Path) -> usize {
        let mut entries = tokio::fs::read_dir(directory).await.unwrap();
        let mut count = 0;
        while entries.next_entry().await.unwrap().is_some() { count += 1; }
        count
    }

    #[test]
    fn interval_parser_requires_supported_positive_suffixes() {
        assert_eq!(parse_interval("3m", "test").unwrap(), Duration::from_secs(180));
        assert_eq!(parse_interval("2D", "test").unwrap(), Duration::from_secs(172800));
        for value in ["1", "0s", "-1h", "1.5m", "1w", "18446744073709551615s", "18446744073709551615d"] {
            assert!(parse_interval(value, "test").is_err(), "{value}");
        }
    }

    #[test]
    fn configuration_parses_byte_sizes_and_rejects_invalid_limits_and_intervals() {
        let settings: Settings = toml::from_str(
            "max_bytes = \"3MiB\"\nmax_entry_bytes = \"1KB\"\neviction_interval = \"2D\"\naccess_timestamp_interval = \"30S\"\n",
        ).unwrap();
        assert_eq!(settings.max_bytes.0, 3 * 1024 * 1024);
        assert_eq!(settings.max_entry_bytes.0, 1000);
        settings.validate().unwrap();
        assert_eq!(Settings::default().max_bytes.0, 1024 * 1024 * 1024);
        let integer_settings: Settings = toml::from_str("max_bytes = 5\nmax_entry_bytes = 5\n").unwrap();
        assert_eq!(integer_settings.max_bytes.0, 5);

        for input in [
            "max_bytes = 0",
            "max_bytes = \"1TB\"",
            "max_bytes = \"1.5M\"",
            "max_bytes = \"18446744073709551615G\"",
            "eviction_interval = 60",
        ] {
            assert!(toml::from_str::<Settings>(input).is_err(), "{input}");
        }
        for interval in ["60", "0s", "-1h", "1.5m", "1w", "18446744073709551615s", "18446744073709551615d"] {
            let settings = Settings { eviction_interval: interval.to_owned(), ..Settings::default() };
            assert!(settings.validate().is_err(), "{interval}");
        }
        let invalid_limits = Settings { max_bytes: ByteLimit(10), max_entry_bytes: ByteLimit(11), ..Settings::default() };
        assert!(invalid_limits.validate().is_err());
        let invalid_concurrency = Settings { max_concurrent_fills: 0, ..Settings::default() };
        assert!(invalid_concurrency.validate().is_err());
        let invalid_target = Settings { eviction_target_percent: 100, ..Settings::default() };
        assert!(invalid_target.validate().is_err());
    }

    #[tokio::test]
    async fn complete_fill_is_streamed_and_survives_restart() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let identity = Identity::original("media/complete");
        {
            let cache = ImageCache::open(&settings).await.unwrap();
            assert_eq!(fs::metadata(&cache.cache_directory).unwrap().permissions().mode() & 0o777, 0o700);
            let generation = cache.fill_generation(&identity);
            let body = cache.cache_body(
                identity.clone(),
                generation,
                http::StatusCode::OK,
                Some(&http::HeaderValue::from_static("image/png")),
                Some(4),
                upstream(b"data"),
            ).await;
            assert_eq!(collect_body(body).await.unwrap(), b"data");
            let hit = cache.lookup(&identity, false).await.unwrap();
            assert_eq!(hit.content_type, "image/png");
            assert_eq!(hit.payload_length, 4);
            assert_eq!(collect_body(hit.body).await.unwrap(), b"data");
            let head = cache.lookup(&identity, true).await.unwrap();
            assert_eq!(head.payload_length, 4);
            assert!(collect_body(head.body).await.unwrap().is_empty());
            assert_eq!(Arc::strong_count(&cache), 1);
        }
        let reopened = ImageCache::open(&settings).await.unwrap();
        let hit = reopened.lookup(&identity, false).await.unwrap();
        assert_eq!(collect_body(hit.body).await.unwrap(), b"data");
        let quality = Identity::quality_image("media/complete", crate::storage::ImageQuality::new(80).unwrap());
        assert!(reopened.lookup(&quality, false).await.is_none());
    }

    #[tokio::test]
    async fn complete_fill_publishes_when_consumer_stops_at_content_length() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let cache = ImageCache::open(&settings).await.unwrap();
        let identity = Identity::original("media/content-length-complete");
        let mut body = cache.cache_body(
            identity.clone(),
            cache.fill_generation(&identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;

        let mut received = Vec::new();
        while received.len() < 4 {
            received.extend_from_slice(&body.next().await.unwrap().unwrap());
        }
        assert_eq!(received, b"data");
        drop(body);

        let hit = cache.lookup(&identity, false).await.unwrap();
        assert_eq!(collect_body(hit.body).await.unwrap(), b"data");
    }

    #[tokio::test]
    async fn startup_clears_abandoned_parts_and_enforces_single_process_ownership() {
        let directory = TestDirectory::new();
        let cache_directory = directory.0.join("cache-root");
        let parts_directory = cache_directory.join("parts");
        fs::create_dir_all(&parts_directory).unwrap();
        fs::set_permissions(&cache_directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&parts_directory, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(parts_directory.join("interrupted-part"), b"partial").unwrap();
        let mut settings = test_settings(&directory.0);
        settings.directory = Some(cache_directory);
        let cache = ImageCache::open(&settings).await.unwrap();
        assert_eq!(entry_count(&parts_directory).await, 0);
        assert!(ImageCache::open(&settings).await.is_err());
        drop(cache);
        assert!(ImageCache::open(&settings).await.is_ok());
    }

    #[tokio::test]
    async fn truncation_and_cancellation_never_publish_parts() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let cache = ImageCache::open(&settings).await.unwrap();
        let truncated_identity = Identity::original("media/truncated");
        let generation = cache.fill_generation(&truncated_identity);
        let truncated = cache.cache_body(
            truncated_identity.clone(),
            generation,
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(8),
            upstream(b"short"),
        ).await;
        assert_eq!(collect_body(truncated).await.unwrap(), b"short");
        assert!(cache.lookup(&truncated_identity, false).await.is_none());

        let cancelled_identity = Identity::original("media/cancelled");
        let generation = cache.fill_generation(&cancelled_identity);
        let mut cancelled = cache.cache_body(
            cancelled_identity.clone(),
            generation,
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(8),
            Box::pin(stream::iter([
                Ok::<_, io::Error>(Bytes::from_static(b"part")),
                Ok::<_, io::Error>(Bytes::from_static(b"tail")),
            ])),
        ).await;
        assert_eq!(cancelled.next().await.unwrap().unwrap(), Bytes::from_static(b"part"));
        drop(cancelled);
        assert!(cache.lookup(&cancelled_identity, false).await.is_none());
        assert_eq!(entry_count(&cache.parts_directory).await, 0);
    }

    #[tokio::test]
    async fn non_200_non_image_unknown_length_and_oversized_responses_bypass() {
        let directory = TestDirectory::new();
        let mut settings = test_settings(&directory.0);
        settings.max_entry_bytes = ByteLimit(4);
        let cache = ImageCache::open(&settings).await.unwrap();
        let cases = [
            ("partial", http::StatusCode::PARTIAL_CONTENT, Some("image/jpeg"), Some(4)),
            ("text", http::StatusCode::OK, Some("text/plain"), Some(4)),
            ("oversized", http::StatusCode::OK, Some("image/jpeg"), Some(5)),
            ("unknown", http::StatusCode::OK, Some("image/jpeg"), None),
        ];
        for (key, status, content_type, content_length) in cases {
            let identity = Identity::original(&format!("media/{key}"));
            let header = content_type.map(http::HeaderValue::from_static);
            let body = cache.cache_body(
                identity.clone(),
                cache.fill_generation(&identity),
                status,
                header.as_ref(),
                content_length,
                upstream(b"data"),
            ).await;
            assert_eq!(collect_body(body).await.unwrap(), b"data");
            assert!(cache.lookup(&identity, false).await.is_none(), "{key}");
        }
        assert_eq!(entry_count(&cache.parts_directory).await, 0);
    }

    #[tokio::test]
    async fn concurrent_duplicate_fills_publish_one_entry() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let cache = ImageCache::open(&settings).await.unwrap();
        let identity = Identity::original("media/duplicate");
        let first = cache.cache_body(
            identity.clone(),
            cache.fill_generation(&identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;
        let second = cache.cache_body(
            identity.clone(),
            cache.fill_generation(&identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;
        let (first, second) = tokio::join!(collect_body(first), collect_body(second));
        assert_eq!(first.unwrap(), b"data");
        assert_eq!(second.unwrap(), b"data");
        assert_eq!(entry_count(&cache.cache_directory).await, 1);
        let hit = cache.lookup(&identity, false).await.unwrap();
        assert_eq!(collect_body(hit.body).await.unwrap(), b"data");
    }

    #[tokio::test]
    async fn invalid_entries_are_removed_and_access_time_updates_are_throttled() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let cache = ImageCache::open(&settings).await.unwrap();
        let invalid_identity = Identity::original("media/invalid");
        let invalid_path = cache.entry_path(&invalid_identity);
        tokio::fs::write(&invalid_path, b"invalid").await.unwrap();
        assert!(cache.lookup(&invalid_identity, false).await.is_none());
        assert!(!invalid_path.exists());

        let identity = Identity::original("media/touched");
        let body = cache.cache_body(
            identity.clone(),
            cache.fill_generation(&identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;
        collect_body(body).await.unwrap();
        let path = cache.entry_path(&identity);
        fs::OpenOptions::new().write(true).open(&path).unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(100))).unwrap();
        let hit = cache.lookup(&identity, true).await.unwrap();
        drop(hit);
        let first_modified = fs::metadata(&path).unwrap().modified().unwrap();
        let hit = cache.lookup(&identity, true).await.unwrap();
        drop(hit);
        let second_modified = fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(first_modified, second_modified);
    }

    #[tokio::test]
    async fn clear_invalidates_a_fill_started_before_the_clear() {
        let directory = TestDirectory::new();
        let settings = test_settings(&directory.0);
        let cache = ImageCache::open(&settings).await.unwrap();
        let identity = Identity::original("media/cleared");
        let generation = cache.fill_generation(&identity);
        let body = cache.cache_body(
            identity.clone(),
            generation,
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;
        assert_eq!(cache.clear(Some("media/cleared")).await.unwrap(), 0);
        assert_eq!(collect_body(body).await.unwrap(), b"data");
        assert!(cache.lookup(&identity, false).await.is_none());
        assert_eq!(entry_count(&cache.parts_directory).await, 0);
    }

    #[tokio::test]
    async fn full_capacity_and_busy_fill_slots_bypass_caching() {
        let directory = TestDirectory::new();
        let mut settings = test_settings(&directory.0);
        settings.max_bytes = ByteLimit(8);
        settings.max_entry_bytes = ByteLimit(4);
        settings.max_concurrent_fills = 1;
        let cache = ImageCache::open(&settings).await.unwrap();
        let first_identity = Identity::original("media/first");
        let first_generation = cache.fill_generation(&first_identity);
        let first = cache.cache_body(
            first_identity.clone(),
            first_generation,
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"data"),
        ).await;
        let second_identity = Identity::original("media/second");
        let second_generation = cache.fill_generation(&second_identity);
        let second = cache.cache_body(
            second_identity.clone(),
            second_generation,
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"more"),
        ).await;
        assert_eq!(collect_body(second).await.unwrap(), b"more");
        assert_eq!(collect_body(first).await.unwrap(), b"data");

        let third_identity = Identity::original("media/third");
        let third = cache.cache_body(
            third_identity.clone(),
            cache.fill_generation(&third_identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"else"),
        ).await;
        assert_eq!(collect_body(third).await.unwrap(), b"else");

        let fourth_identity = Identity::original("media/fourth");
        let fourth = cache.cache_body(
            fourth_identity.clone(),
            cache.fill_generation(&fourth_identity),
            http::StatusCode::OK,
            Some(&http::HeaderValue::from_static("image/jpeg")),
            Some(4),
            upstream(b"last"),
        ).await;
        assert_eq!(collect_body(fourth).await.unwrap(), b"last");
        assert!(cache.lookup(&first_identity, false).await.is_some());
        assert!(cache.lookup(&second_identity, false).await.is_none());
        assert!(cache.lookup(&third_identity, false).await.is_some());
        assert!(cache.lookup(&fourth_identity, false).await.is_none());
    }

    #[tokio::test]
    async fn eviction_removes_old_ordinary_entries_before_recent_post_media() {
        let directory = TestDirectory::new();
        let database = Database::new(directory.0.join("site.sqlite3"));
        database.initialize().await.unwrap();
        let connection = rusqlite::Connection::open(database.path()).unwrap();
        connection.execute_batch(
            "INSERT INTO users (id, username, role, password_hash, created_at) VALUES (1, 'admin', 'admin', 'x', 1);\
             INSERT INTO posts (id, author_id, title, published_at, summary, published) VALUES (1, 1, 'recent', 10, '', 1);\
             INSERT INTO media_assets (storage_key, content_type, size_bytes) VALUES ('media/recent', 'image/jpeg', 4);\
             INSERT INTO post_blocks (id, post_id, parent_id, position) VALUES (1, 1, NULL, 0);\
             INSERT INTO post_blocks (id, post_id, parent_id, position, storage_key) VALUES (2, 1, 1, 0, 'media/recent');",
        ).unwrap();
        drop(connection);

        let mut settings = test_settings(&directory.0);
        settings.max_bytes = ByteLimit(12);
        settings.max_entry_bytes = ByteLimit(4);
        settings.eviction_target_percent = 50;
        settings.recent_post_count = 1;
        let cache = ImageCache::open(&settings).await.unwrap();
        let identities = [
            Identity::original("media/old"),
            Identity::original("media/new"),
            Identity::original("media/recent"),
        ];
        for identity in &identities {
            let body = cache.cache_body(
                identity.clone(),
                cache.fill_generation(identity),
                http::StatusCode::OK,
                Some(&http::HeaderValue::from_static("image/jpeg")),
                Some(4),
                upstream(b"data"),
            ).await;
            collect_body(body).await.unwrap();
        }
        for (identity, second) in identities.iter().zip([10, 20, 1]) {
            let file = fs::OpenOptions::new().write(true).open(cache.entry_path(identity)).unwrap();
            file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(second))).unwrap();
        }
        cache.evict(&database, false).await.unwrap();
        assert!(cache.lookup(&identities[0], false).await.is_none());
        assert!(cache.lookup(&identities[1], false).await.is_none());
        assert!(cache.lookup(&identities[2], false).await.is_some());
    }

    #[test]
    fn identities_distinguish_all_representations_and_options() {
        assert_ne!(identity_filename(&Identity::original("media/a")), identity_filename(&Identity::thumbnail("media/a")));
        let quality_80 = Identity::quality_image("media/a", crate::storage::ImageQuality::new(80).unwrap());
        let quality_81 = Identity::quality_image("media/a", crate::storage::ImageQuality::new(81).unwrap());
        assert_ne!(identity_filename(&Identity::original("media/a")), identity_filename(&quality_80));
        assert_ne!(identity_filename(&quality_80), identity_filename(&quality_81));
        assert_ne!(
            identity_filename(&Identity::reduced_image("media/a", 1200, Some(500), false)),
            identity_filename(&Identity::reduced_image("media/a", 1200, Some(500), true)),
        );
        assert_ne!(
            identity_filename(&Identity::reduced_image("media/a", 1200, Some(500), false)),
            identity_filename(&Identity::reduced_image("media/a", 800, Some(500), false)),
        );
    }
}
