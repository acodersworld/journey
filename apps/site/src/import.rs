use std::{
    collections::BTreeMap,
    error::Error,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use crate::{db::{Database, NewBlock, NewPost}, storage::StorageClient};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    posts: Vec<ManifestPost>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestPost {
    title: String,
    published_at: String,
    summary: String,
    blocks: Vec<ManifestBlock>,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ManifestBlock {
    Paragraph {
        text: String,
    },
    Heading {
        text: String,
        level: u8,
    },
    Image {
        path: String,
        #[serde(default)]
        alt: Option<String>,
        #[serde(default)]
        caption: Option<String>,
    },
    Video {
        path: String,
        #[serde(default)]
        caption: Option<String>,
    },
}

struct MediaAsset {
    content_type: String,
}

struct PreparedPost {
    title: String,
    published_at: String,
    summary: String,
    blocks: Vec<PreparedBlock>,
}

enum PreparedBlock {
    Ready(NewBlock),
    Media {
        kind: String,
        path: PathBuf,
        content_type: String,
        alt: Option<String>,
        caption: Option<String>,
    },
}

pub struct PreparedImport {
    posts: Vec<PreparedPost>,
    assets: BTreeMap<PathBuf, MediaAsset>,
}

pub async fn prepare_manifest(manifest_path: &Path) -> AppResult<PreparedImport> {
    let manifest_path = tokio::fs::canonicalize(manifest_path).await?;
    let manifest_dir = manifest_path
        .parent()
        .ok_or("manifest has no parent directory")?
        .to_path_buf();
    let manifest_bytes = tokio::fs::read(&manifest_path).await?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;

    let mut assets = BTreeMap::<PathBuf, MediaAsset>::new();
    let mut posts = Vec::with_capacity(manifest.posts.len());
    for post in manifest.posts {
        validate_post(&post)?;
        let mut blocks = Vec::with_capacity(post.blocks.len());
        for block in post.blocks {
            match block {
                ManifestBlock::Paragraph { text } => blocks.push(PreparedBlock::Ready(NewBlock {
                    kind: "paragraph".to_owned(),
                    text: Some(text),
                    level: None,
                    storage_key: None,
                    content_type: None,
                    alt: None,
                    caption: None,
                })),
                ManifestBlock::Heading { text, level } => blocks.push(PreparedBlock::Ready(NewBlock {
                    kind: "heading".to_owned(),
                    text: Some(text),
                    level: Some(i64::from(level)),
                    storage_key: None,
                    content_type: None,
                    alt: None,
                    caption: None,
                })),
                ManifestBlock::Image { path, alt, caption } => {
                    let (content_type, source_path) =
                        validate_media_path(&manifest_dir, &path).await?;
                    assets.entry(source_path.clone()).or_insert(MediaAsset {
                        content_type: content_type.clone(),
                    });
                    blocks.push(PreparedBlock::Media {
                        kind: "image".to_owned(),
                        path: source_path,
                        content_type,
                        alt,
                        caption,
                    });
                }
                ManifestBlock::Video { path, caption } => {
                    let (content_type, source_path) =
                        validate_media_path(&manifest_dir, &path).await?;
                    assets.entry(source_path.clone()).or_insert(MediaAsset {
                        content_type: content_type.clone(),
                    });
                    blocks.push(PreparedBlock::Media {
                        kind: "video".to_owned(),
                        path: source_path,
                        content_type,
                        alt: None,
                        caption,
                    });
                }
            }
        }
        posts.push(PreparedPost {
            title: post.title,
            published_at: post.published_at,
            summary: post.summary,
            blocks,
        });
    }

    Ok(PreparedImport { posts, assets })
}

pub async fn apply_import<S: StorageClient>(
    prepared: PreparedImport,
    database: &Database,
    storage: &S,
) -> AppResult<()> {
    println!(
        "validated {} posts and {} unique media files",
        prepared.posts.len(),
        prepared.assets.len()
    );
    let mut storage_keys = BTreeMap::new();
    for (path, asset) in prepared.assets {
        let storage_key = storage.put_file(&asset.content_type, &path).await?;
        println!("uploaded {storage_key}");
        storage_keys.insert(path, storage_key);
    }

    database
        .replace_posts(resolve_posts(prepared.posts, &storage_keys)?)
        .await
        .map_err(std::io::Error::other)?;
    println!("replaced the published post set");
    Ok(())
}

fn validate_post(post: &ManifestPost) -> AppResult<()> {
    if post.title.trim().is_empty() {
        return Err("post title must not be empty".into());
    }
    validate_publication_date(&post.published_at)?;
    for block in &post.blocks {
        match block {
            ManifestBlock::Heading { level, .. } if !(1..=6).contains(level) => {
                return Err(format!("heading level must be between 1 and 6, got {level}").into());
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_publication_date(value: &str) -> AppResult<()> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        return Err(format!("publication date must use YYYY-MM-DD: {value:?}").into());
    }
    let year = value[0..4].parse::<u32>()?;
    let month = value[5..7].parse::<u32>()?;
    let day = value[8..10].parse::<u32>()?;
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => return Err(format!("invalid publication date: {value:?}").into()),
    };
    if day == 0 || day > month_days {
        return Err(format!("invalid publication date: {value:?}").into());
    }
    Ok(())
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
        "mp4" => Ok("video/mp4".to_owned()),
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
            let blocks = post
                .blocks
                .into_iter()
                .map(|block| match block {
                    PreparedBlock::Ready(block) => Ok(block),
                    PreparedBlock::Media {
                        kind,
                        path,
                        content_type,
                        alt,
                        caption,
                    } => {
                        let storage_key = storage_keys.get(&path).cloned().ok_or_else(|| {
                            format!("media asset was not uploaded: {}", path.display())
                        })?;
                        Ok(NewBlock {
                            kind,
                            text: None,
                            level: None,
                            storage_key: Some(storage_key),
                            content_type: Some(content_type),
                            alt,
                            caption,
                        })
                    }
                })
                .collect::<AppResult<Vec<_>>>()?;
            Ok(NewPost {
                title: post.title,
                published_at: post.published_at,
                summary: post.summary,
                blocks,
            })
        })
        .collect()
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
    use crate::storage::StorageResponse;

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
                serde_json::json!({"type": "image", "path": path})
            })
            .collect::<Vec<_>>();
        let manifest = serde_json::json!({
            "posts": [{
                "title": "Imported post",
                "published_at": "2026-01-02",
                "summary": "Imported summary",
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
                title: "Previous post".to_owned(),
                published_at: "2025-12-31".to_owned(),
                summary: "Still available after a failed import".to_owned(),
                blocks: Vec::new(),
            }])
            .await
            .unwrap();
        database
    }

    #[tokio::test]
    async fn repeated_media_path_uploads_once_and_resolves_all_blocks() {
        let directory = TestDirectory::new();
        let database = database_with_old_post(&directory).await;
        let manifest = make_manifest(&directory, &["photo.png", "photo.png"]);
        let prepared = prepare_manifest(&manifest).await.unwrap();
        let key = format!("media/{}", "a".repeat(64));
        let storage = FakeStorage::new(vec![Ok(key.clone())]);

        apply_import(prepared, &database, &storage).await.unwrap();

        assert_eq!(storage.uploads.lock().unwrap().len(), 1);
        let post = database.all_summaries().await.unwrap().remove(0);
        for position in 0..2 {
            let media = database.media_reference(post.id, position).await.unwrap().unwrap();
            assert_eq!(media.storage_key, key);
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
        let post = database.all_summaries().await.unwrap().remove(0);
        for position in 0..2 {
            let media = database.media_reference(post.id, position).await.unwrap().unwrap();
            assert_eq!(media.storage_key, key);
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
