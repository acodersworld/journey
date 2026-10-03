use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    auth,
    db::{AccountRole, Database, ImportedAccount, NewBlock, NewPost},
    storage::StorageClient,
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    #[serde(default)]
    users: Vec<ManifestUser>,
    posts: Vec<ManifestPost>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestUser {
    username: String,
    password: String,
    #[serde(default)]
    role: AccountRole,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPost {
    author: String,
    title: String,
    published_at: String,
    summary: String,
    #[serde(default)]
    tags: Vec<String>,
    blocks: Vec<ManifestBlock>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestBlock {
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    alt: Option<String>,
    #[serde(default)]
    blocks: Vec<ManifestBlock>,
}

struct MediaAsset {
    content_type: String,
}

struct PreparedPost {
    author_username: String,
    title: String,
    published_at: i64,
    summary: String,
    tags: Vec<String>,
    blocks: Vec<PreparedBlock>,
}

struct PreparedBlock {
    header: Option<String>,
    body: Option<String>,
    media: Option<PreparedMedia>,
    children: Vec<PreparedBlock>,
}

struct PreparedMedia {
    path: PathBuf,
    content_type: String,
    alt: Option<String>,
}

pub struct PreparedImport {
    accounts: Vec<ImportedAccount>,
    posts: Vec<PreparedPost>,
    assets: BTreeMap<PathBuf, MediaAsset>,
}

#[derive(Serialize, Deserialize)]
pub struct ImportResult {
    pub posts_replaced: usize,
    pub imported_accounts: usize,
    pub unique_media_files: usize,
    pub accounts_added: usize,
}

pub async fn prepare_manifest(manifest_path: &Path) -> AppResult<PreparedImport> {
    let manifest_path = tokio::fs::canonicalize(manifest_path).await?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or("manifest has no parent directory")?
        .to_path_buf();
    let manifest_bytes = tokio::fs::read(&manifest_path).await?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

    let mut accounts = Vec::with_capacity(manifest.users.len());
    let mut usernames = HashSet::new();
    for user in manifest.users {
        auth::validate_username(&user.username)?;
        if user.password.is_empty() {
            return Err(format!("password for imported user {:?} must not be empty", user.username).into());
        }
        if !usernames.insert(user.username.to_ascii_lowercase()) {
            return Err(format!("duplicate imported username: {:?}", user.username).into());
        }
        accounts.push(ImportedAccount {
            username: user.username,
            role: user.role,
            password_hash: auth::hash_password(&user.password).map_err(std::io::Error::other)?,
        });
    }

    let mut assets = BTreeMap::<PathBuf, MediaAsset>::new();
    let mut posts = Vec::with_capacity(manifest.posts.len());
    for post in manifest.posts {
        let published_at = validate_post(&post)?;
        let mut blocks = Vec::with_capacity(post.blocks.len());
        for mut block in post.blocks {
            let mut children = Vec::with_capacity(block.blocks.len());
            for child in std::mem::take(&mut block.blocks) {
                children.push(prepare_block(child, &manifest_dir, &mut assets).await?);
            }
            let media = prepare_media(block.path, block.alt, &manifest_dir, &mut assets).await?;
            blocks.push(PreparedBlock {
                header: block.header,
                body: block.body,
                media,
                children,
            });
        }
        posts.push(PreparedPost {
            author_username: post.author,
            title: post.title,
            published_at,
            summary: post.summary,
            tags: post.tags,
            blocks,
        });
    }

    Ok(PreparedImport { accounts, posts, assets })
}

pub async fn apply_import<S: StorageClient>(
    prepared: PreparedImport,
    database: &Database,
    storage: &S,
) -> AppResult<ImportResult> {
    database.initialize().await.map_err(std::io::Error::other)?;
    let posts_replaced = prepared.posts.len();
    let imported_accounts = prepared.accounts.len();
    let unique_media_files = prepared.assets.len();
    println!(
        "validated {} posts, {} imported accounts, and {} unique media files",
        prepared.posts.len(),
        prepared.accounts.len(),
        prepared.assets.len()
    );
    let mut storage_keys = BTreeMap::new();
    for (path, asset) in prepared.assets {
        let storage_key = storage.put_file(&asset.content_type, &path).await?;
        println!("uploaded {storage_key}");
        storage_keys.insert(path, storage_key);
    }

    let added_accounts = database
        .replace_posts_and_add_accounts(
            resolve_posts(prepared.posts, &storage_keys)?,
            prepared.accounts,
        )
        .await
        .map_err(std::io::Error::other)?;
    println!("added {added_accounts} new account(s); existing accounts and sessions were kept");
    println!("replaced the published post set");
    Ok(ImportResult {
        posts_replaced,
        imported_accounts,
        unique_media_files,
        accounts_added: added_accounts,
    })
}

fn validate_post(post: &ManifestPost) -> AppResult<i64> {
    auth::validate_username(&post.author)?;
    if post.title.trim().is_empty() {
        return Err("post title must not be empty".into());
    }
    let published_at = parse_publication_timestamp(&post.published_at)?;
    for block in &post.blocks {
        validate_block(block, false)?;
        for child in &block.blocks {
            validate_block(child, true)?;
        }
    }
    Ok(published_at)
}

fn parse_publication_timestamp(value: &str) -> AppResult<i64> {
    let has_explicit_offset = value.ends_with('Z')
        || value.ends_with('z')
        || value.rfind(|character| matches!(character, '+' | '-')).is_some_and(|offset_start| {
            offset_start > 10
                && matches!(value.len() - offset_start, 3 | 5 | 6)
                && value[offset_start + 1..]
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| index == 2 && value.len() - offset_start == 6
                        || byte.is_ascii_digit())
        });
    if !value.contains('T') || !has_explicit_offset {
        return Err("published_at must be an ISO 8601 timestamp with an explicit offset or Z".into());
    }
    let timestamp = value
        .parse::<jiff::Timestamp>()
        .map_err(|error| format!("published_at is not a valid ISO 8601 timestamp: {error}"))?;
    Ok(timestamp.as_second())
}

fn validate_block(block: &ManifestBlock, nested: bool) -> AppResult<()> {
    if nested && !block.blocks.is_empty() {
        return Err("post blocks may only be nested one level deep".into());
    }
    if block.header.is_none()
        && block.body.is_none()
        && block.path.is_none()
        && block.blocks.is_empty()
    {
        return Err("each post block must have a header, body, media path, or child block".into());
    }
    if block.alt.is_some() && block.path.is_none() {
        return Err("alt text requires a media path".into());
    }
    Ok(())
}

async fn prepare_block(
    block: ManifestBlock,
    manifest_dir: &Path,
    assets: &mut BTreeMap<PathBuf, MediaAsset>,
) -> AppResult<PreparedBlock> {
    let media = prepare_media(block.path, block.alt, manifest_dir, assets).await?;
    Ok(PreparedBlock {
        header: block.header,
        body: block.body,
        media,
        children: Vec::new(),
    })
}

async fn prepare_media(
    path: Option<String>,
    alt: Option<String>,
    manifest_dir: &Path,
    assets: &mut BTreeMap<PathBuf, MediaAsset>,
) -> AppResult<Option<PreparedMedia>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let (content_type, source_path) = validate_media_path(manifest_dir, &path).await?;
    assets.entry(source_path.clone()).or_insert(MediaAsset {
        content_type: content_type.clone(),
    });
    Ok(Some(PreparedMedia {
        path: source_path,
        content_type,
        alt,
    }))
}

async fn validate_media_path(
    manifest_dir: &Path,
    relative_path: &str,
) -> AppResult<(String, PathBuf)> {
    let relative_path = Path::new(relative_path);
    if relative_path.as_os_str().is_empty() || relative_path.is_absolute() {
        return Err("media paths must be non-empty and relative to the manifest".into());
    }
    let content_type = content_type_for_path(relative_path)?;
    let media_path = tokio::fs::canonicalize(manifest_dir.join(relative_path)).await?;
    if !media_path.starts_with(manifest_dir) {
        return Err(format!(
            "media path escapes the manifest directory: {}",
            relative_path.display()
        )
        .into());
    }
    if !tokio::fs::metadata(&media_path).await?.is_file() {
        return Err(format!("media path is not a regular file: {}", relative_path.display()).into());
    }
    Ok((content_type, media_path))
}

fn content_type_for_path(path: &Path) -> AppResult<String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| format!("media file has no supported extension: {}", path.display()))?
        .to_ascii_lowercase();
    match extension.as_str() {
        "jpg" | "jpeg" => Ok("image/jpeg".to_owned()),
        "png" => Ok("image/png".to_owned()),
        "webp" => Ok("image/webp".to_owned()),
        "gif" => Ok("image/gif".to_owned()),
        "heic" => Ok("image/heic".to_owned()),
        "heif" => Ok("image/heif".to_owned()),
        "mp4" => Ok("video/mp4".to_owned()),
        "mov" => Ok("video/quicktime".to_owned()),
        _ => Err(format!("unsupported media extension: .{extension}").into()),
    }
}

fn resolve_posts(
    posts: Vec<PreparedPost>,
    storage_keys: &BTreeMap<PathBuf, String>,
) -> AppResult<Vec<NewPost>> {
    posts
        .into_iter()
        .map(|post| {
            let blocks = resolve_blocks(post.blocks, storage_keys)?;
            Ok(NewPost {
                author_username: post.author_username,
                title: post.title,
                published_at: Some(post.published_at),
                summary: post.summary,
                tags: post.tags,
                blocks,
            })
        })
        .collect()
}

fn resolve_blocks(
    blocks: Vec<PreparedBlock>,
    storage_keys: &BTreeMap<PathBuf, String>,
) -> AppResult<Vec<NewBlock>> {
    blocks
        .into_iter()
        .map(|block| resolve_root_block(block, storage_keys))
        .collect()
}

fn resolve_root_block(
    block: PreparedBlock,
    storage_keys: &BTreeMap<PathBuf, String>,
) -> AppResult<NewBlock> {
    let mut children = Vec::new();
    if let Some(media) = block.media {
        children.push(resolve_media_item(media, None, None, storage_keys)?);
    }
    let mut body_parts = Vec::new();
    if let Some(body) = block.body {
        body_parts.push(body);
    }
    for child in block.children {
        collect_import_content(child, storage_keys, &mut children, &mut body_parts)?;
    }
    Ok(NewBlock {
        id: None,
        header: block.header,
        body: (!body_parts.is_empty()).then(|| body_parts.join("\n\n")),
        storage_key: None,
        content_type: None,
        alt: None,
        children,
    })
}

fn collect_import_content(
    block: PreparedBlock,
    storage_keys: &BTreeMap<PathBuf, String>,
    gallery: &mut Vec<NewBlock>,
    body_parts: &mut Vec<String>,
) -> AppResult<()> {
    if let Some(media) = block.media {
        gallery.push(resolve_media_item(media, block.header, block.body, storage_keys)?);
    } else {
        if let Some(header) = block.header {
            body_parts.push(header);
        }
        if let Some(body) = block.body {
            body_parts.push(body);
        }
    }
    for child in block.children {
        collect_import_content(child, storage_keys, gallery, body_parts)?;
    }
    Ok(())
}

fn resolve_media_item(
    media: PreparedMedia,
    header: Option<String>,
    body: Option<String>,
    storage_keys: &BTreeMap<PathBuf, String>,
) -> AppResult<NewBlock> {
    let storage_key = storage_keys.get(&media.path).cloned().ok_or_else(|| {
        format!("media asset was not uploaded: {}", media.path.display())
    })?;
    Ok(NewBlock {
        id: None,
        header,
        body,
        storage_key: Some(storage_key),
        content_type: Some(media.content_type),
        alt: media.alt,
        children: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        fs,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
    };

    use super::*;
    use crate::storage::{StorageBody, StorageResponse};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct FakeStorage {
        uploads: Arc<Mutex<Vec<(PathBuf, String)>>>,
        responses: Arc<Mutex<VecDeque<Result<String, String>>>>,
    }

    impl FakeStorage {
        fn new(responses: Vec<Result<String, String>>) -> Self {
            Self {
                uploads: Arc::new(Mutex::new(Vec::new())),
                responses: Arc::new(Mutex::new(responses.into())),
            }
        }
    }

    impl StorageClient for FakeStorage {
        fn put_file(
            &self,
            content_type: &str,
            path: &Path,
        ) -> impl std::future::Future<Output = Result<String, String>> + Send {
            let content_type = content_type.to_owned();
            let path = path.to_path_buf();
            let uploads = Arc::clone(&self.uploads);
            let responses = Arc::clone(&self.responses);
            async move {
                uploads.lock().unwrap().push((path, content_type));
                responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Err("no stubbed upload response".to_owned()))
            }
        }

        fn put_stream(
            &self,
            _content_type: &str,
            _content_length: Option<u64>,
            _body: StorageBody,
            _max_bytes: u64,
        ) -> impl std::future::Future<Output = Result<(String, u64), String>> + Send {
            async { Err("stream uploads are unused in import tests".to_owned()) }
        }

        fn get(
            &self,
            _key: &str,
            _range: Option<&str>,
            _head: bool,
        ) -> impl std::future::Future<Output = Result<StorageResponse, String>> + Send {
            async { Err("GET is unused in import tests".to_owned()) }
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "journey-site-import-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn make_manifest(directory: &TestDirectory, media_paths: &[&str]) -> PathBuf {
        let blocks = media_paths
            .iter()
            .map(|path| {
                fs::write(directory.path().join(path), b"same media bytes").unwrap();
                serde_json::json!({"path": path})
            })
            .collect::<Vec<_>>();
        let manifest = serde_json::json!({
            "users": [{"username": "user", "password": "password", "role": "read"}],
            "posts": [{
                "author": "user",
                "title": "Imported post",
                "published_at": "2026-01-02T12:00:00Z",
                "summary": "Imported summary",
                "tags": ["Import test", "CASE-sensitive"],
                "blocks": blocks
            }]
        });
        let path = directory.path().join("posts.json");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        path
    }

    async fn database_with_old_post(directory: &TestDirectory) -> Database {
        let database = Database::new(directory.path().join("posts.sqlite3"));
        database
            .replace_posts(vec![NewPost {
                author_username: "test-author".to_owned(),
                title: "Previous post".to_owned(),
                published_at: Some("2025-12-31T12:00:00Z".parse::<jiff::Timestamp>().unwrap().as_second()),
                summary: "Still available after a failed import".to_owned(),
                tags: Vec::new(),
                blocks: Vec::new(),
            }])
            .await
            .unwrap();
        database
    }

    #[tokio::test]
    async fn example_manifest_has_tags_and_a_group_with_its_own_media() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("example/posts.json");
        let prepared = prepare_manifest(&manifest).await.unwrap();
        let post = &prepared.posts[0];
        assert_eq!(post.author_username, "user");
        assert_eq!(prepared.posts[1].author_username, "user2");
        assert_eq!(post.tags, vec!["alpine", "lake", "morning"]);
        assert_eq!(post.blocks.len(), 1);
        assert!(post.blocks[0].media.is_some());
        assert_eq!(post.blocks[0].children.len(), 2);
        assert!(post.blocks[0].children[0].media.is_none());
        assert!(post.blocks[0].children[1].media.is_some());
    }

    #[tokio::test]
    async fn manifest_rejects_blocks_nested_more_than_one_level() {
        let directory = TestDirectory::new();
        let manifest = serde_json::json!({
            "posts": [{
                "author": "user",
                "title": "Nested post",
                "published_at": "2026-01-02T12:00:00Z",
                "summary": "Nested summary",
                "blocks": [{
                    "header": "Group",
                    "blocks": [{
                        "body": "Child",
                        "blocks": [{"body": "Grandchild"}]
                    }]
                }]
            }]
        });
        let path = directory.path().join("posts.json");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let error = prepare_manifest(&path).await.err().unwrap();
        assert!(error.to_string().contains("one level deep"));
    }

    #[tokio::test]
    async fn repeated_media_path_uploads_once_and_resolves_all_blocks() {
        let directory = TestDirectory::new();
        let database = database_with_old_post(&directory).await;
        database
            .create_account("user".to_owned(), AccountRole::Read, "existing-hash".to_owned())
            .await
            .unwrap();
        database
            .create_draft(
                "test-author".to_owned(),
                "HTTP-created draft".to_owned(),
                String::new(),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
        let manifest = make_manifest(&directory, &["photo.png", "photo.png"]);
        let prepared = prepare_manifest(&manifest).await.unwrap();
        let key = format!("media/{}", "a".repeat(64));
        let storage = FakeStorage::new(vec![Ok(key.clone())]);

        apply_import(prepared, &database, &storage).await.unwrap();

        assert_eq!(storage.uploads.lock().unwrap().len(), 1);
        let summary = database.all_summaries().await.unwrap().remove(0);
        let post = database.post(summary.id).await.unwrap().unwrap();
        assert_eq!(post.author_username, "user");
        assert!(database.drafts(None).await.unwrap().is_empty());
        assert_eq!(post.tags, vec!["Import test", "CASE-sensitive"]);
        for block in &post.blocks {
            for placement in &block.children {
                let media = database.media_reference(summary.id, placement.id).await.unwrap().unwrap();
                assert_eq!(media.storage_key, key);
            }
        }
    }

    #[tokio::test]
    async fn distinct_media_paths_can_resolve_to_the_same_generated_key() {
        let directory = TestDirectory::new();
        let database = database_with_old_post(&directory).await;
        let manifest = make_manifest(&directory, &["first.png", "second.png"]);
        let prepared = prepare_manifest(&manifest).await.unwrap();
        let key = format!("media/{}", "b".repeat(64));
        let storage = FakeStorage::new(vec![Ok(key.clone()), Ok(key.clone())]);

        apply_import(prepared, &database, &storage).await.unwrap();

        assert_eq!(storage.uploads.lock().unwrap().len(), 2);
        let summary = database.all_summaries().await.unwrap().remove(0);
        let post = database.post(summary.id).await.unwrap().unwrap();
        for block in &post.blocks {
            for placement in &block.children {
                let media = database.media_reference(summary.id, placement.id).await.unwrap().unwrap();
                assert_eq!(media.storage_key, key);
            }
        }
    }

    #[tokio::test]
    async fn upload_or_object_name_error_keeps_the_previous_post_set() {
        for error in [
            "upload failed",
            "missing Object-Name",
            "duplicate Object-Name",
            "malformed Object-Name",
            "invalid Object-Name",
        ] {
            let directory = TestDirectory::new();
            let database = database_with_old_post(&directory).await;
            let manifest = make_manifest(&directory, &["photo.png"]);
            let prepared = prepare_manifest(&manifest).await.unwrap();
            let storage = FakeStorage::new(vec![Err(error.to_owned())]);

            assert!(apply_import(prepared, &database, &storage).await.is_err());

            let posts = database.all_summaries().await.unwrap();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].title, "Previous post");
        }
    }
}
