use std::{error::Error, fs, path::{Path, PathBuf}, process::Command, sync::Arc, time::Duration};

use futures_util::StreamExt;
use http::StatusCode;
use rand::RngCore;
use rusqlite::{backup::Backup, Connection, OpenFlags};
use tokio::{io::AsyncWriteExt, sync::Mutex};

use crate::storage::StorageClient;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;
pub const KEY: &str = "backups/site/latest.sqlite3.zst";

#[derive(Clone, Default)]
pub struct Jobs(Arc<Mutex<()>>);

struct Temporary(PathBuf);

impl Temporary {
    fn near(path: &Path) -> Result<Self> {
        let parent = path.parent().ok_or("path needs a parent directory")?;
        let mut random = [0_u8; 16];
        rand::thread_rng().fill_bytes(&mut random);
        Ok(Self(parent.join(format!(".journey-backup-{}", hex(&random)))))
    }
}

impl Drop for Temporary {
    fn drop(&mut self) { let _ = fs::remove_file(&self.0); }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate(path: &Path) -> Result<()> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" { return Err(format!("SQLite integrity check failed: {integrity}").into()); }
    let mut foreign_keys = db.prepare("PRAGMA foreign_key_check")?;
    if foreign_keys.query([])?.next()?.is_some() { return Err("SQLite foreign key check failed".into()); }
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != crate::db::CURRENT_SCHEMA_VERSION { return Err(format!("unsupported site schema version {version}; expected {}", crate::db::CURRENT_SCHEMA_VERSION).into()); }
    Ok(())
}

fn zstd(input: &Path, output: &Path, decompress: bool) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(output)?;
    let mut command = Command::new("zstd");
    command.arg("-q").arg("-f");
    if decompress { command.arg("-d"); } else { command.arg("-3"); }
    let status = command.arg(input).arg("-o").arg(output).status()?;
    if !status.success() { return Err(format!("zstd failed with {status}").into()); }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(output, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub async fn create<S: StorageClient>(jobs: &Jobs, database: &Path, storage: &S) -> Result<()> {
    let _guard = jobs.0.lock().await;
    log::info!("site_backup_started key={KEY}");
    let source = database.to_path_buf();
    let compressed = tokio::task::spawn_blocking(move || -> Result<Temporary> {
        let snapshot = Temporary::near(&source)?;
        let compressed = Temporary::near(&source)?;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&snapshot.0)?;
        let live = Connection::open_with_flags(&source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut target = Connection::open(&snapshot.0)?;
        Backup::new(&live, &mut target)?.run_to_completion(100, Duration::from_millis(50), None)?;
        drop(target);
        validate(&snapshot.0)?;
        zstd(&snapshot.0, &compressed.0, false)?;
        Ok(compressed)
    }).await??;
    storage.put_file_at(KEY, "application/zstd", &compressed.0).await?;
    log::info!("site_backup_completed key={KEY}");
    Ok(())
}

pub async fn restore_check<S: StorageClient>(jobs: &Jobs, storage: &S, output: &Path) -> Result<()> {
    let _guard = jobs.0.lock().await;
    if !output.is_absolute() { return Err("output path must be absolute and visible to the site container".into()); }
    if output.exists() { return Err("output file already exists".into()); }
    let compressed = Temporary::near(output)?;
    let mut response = storage.get(KEY, None, false, false).await?;
    if response.status != StatusCode::OK { return Err(format!("backup GET failed with HTTP {}", response.status).into()); }
    let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&compressed.0).await?;
    while let Some(chunk) = response.body.next().await { file.write_all(&chunk?).await?; }
    file.flush().await?;
    drop(file);
    verify_file(&compressed.0, output).await
}

pub async fn verify_file(input: &Path, output: &Path) -> Result<()> {
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        if output.exists() { return Err("output file already exists".into()); }
        let staged = Temporary::near(&output)?;
        zstd(&input, &staged.0, true)?;
        let staged_database = crate::db::Database::new(staged.0.clone());
        tokio::runtime::Handle::current().block_on(staged_database.initialize())
            .map_err(std::io::Error::other)?;
        validate(&staged.0)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged.0, fs::Permissions::from_mode(0o600))?;
        fs::hard_link(&staged.0, &output)?;
        Ok(())
    }).await??;
    Ok(())
}

pub fn parse_daily(value: &str) -> std::result::Result<u32, String> {
    let bytes = value.as_bytes();
    if bytes.len() != 5 || bytes[2] != b':' || !bytes.iter().enumerate().all(|(i, b)| i == 2 || b.is_ascii_digit()) {
        return Err("backup.daily_utc must be HH:MM in 24-hour UTC time".to_owned());
    }
    let hour: u32 = value[..2].parse().unwrap();
    let minute: u32 = value[3..].parse().unwrap();
    if hour > 23 || minute > 59 { return Err("backup.daily_utc must be HH:MM in 24-hour UTC time".to_owned()); }
    Ok(hour * 3600 + minute * 60)
}

pub fn next_daily(now: u64, second_of_day: u64) -> u64 {
    let today = (now / 86_400) * 86_400 + second_of_day;
    if today <= now { today + 86_400 } else { today }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use bytes::Bytes;
    use futures_util::stream;
    use crate::storage::{StorageBody, StorageResponse};

    #[derive(Clone, Default)]
    struct MemoryStorage {
        object: Arc<Mutex<Option<Vec<u8>>>>,
        fail: Arc<Mutex<bool>>,
    }

    impl StorageClient for MemoryStorage {
        async fn put_file(&self, _content_type: &str, _path: &Path) -> std::result::Result<String, String> {
            Err("unused".to_owned())
        }

        async fn put_stream(&self, _content_type: &str, _content_length: Option<u64>, _body: StorageBody, _max_bytes: u64)
            -> std::result::Result<(String, u64), String> { Err("unused".to_owned()) }

        async fn put_file_at(&self, key: &str, content_type: &str, path: &Path) -> std::result::Result<(), String> {
            assert_eq!(key, KEY);
            assert_eq!(content_type, "application/zstd");
            if *self.fail.lock().await { return Err("simulated upload failure".to_owned()); }
            let bytes = tokio::fs::read(path).await.map_err(|error| error.to_string())?;
            *self.object.lock().await = Some(bytes);
            Ok(())
        }

        async fn get(&self, key: &str, _range: Option<&str>, _head: bool, _thumbnail: bool)
            -> std::result::Result<StorageResponse, String> {
            assert_eq!(key, KEY);
            let object = self.object.lock().await.clone().ok_or("missing backup")?;
            Ok(StorageResponse {
                status: StatusCode::OK,
                headers: http::HeaderMap::new(),
                body: Box::pin(stream::once(async move { Ok(Bytes::from(object)) })),
            })
        }
    }

    #[test]
    fn daily_time_is_strict() {
        assert_eq!(parse_daily("03:00").unwrap(), 10_800);
        for value in ["3:00", "24:00", "12:60", "12:0x", "12:00Z"] {
            assert!(parse_daily(value).is_err(), "{value}");
        }
    }

    #[test]
    fn daily_schedule_skips_elapsed_runs() {
        assert_eq!(next_daily(10_000, 10_800), 10_800);
        assert_eq!(next_daily(10_800, 10_800), 97_200);
        assert_eq!(next_daily(200_000, 10_800), 270_000);
    }

    #[tokio::test]
    async fn verify_rejects_invalid_input_and_keeps_existing_output() {
        let directory = std::env::temp_dir().join(format!("journey-backup-test-{}", hex(&rand::random::<[u8; 16]>())));
        fs::create_dir(&directory).unwrap();
        let input = directory.join("bad.zst");
        let output = directory.join("restored.sqlite3");
        fs::write(&input, b"invalid").unwrap();
        fs::write(&output, b"existing").unwrap();
        assert!(verify_file(&input, &output).await.is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing");
        fs::remove_file(&output).unwrap();
        assert!(verify_file(&input, &output).await.is_err());
        assert!(!output.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn verify_accepts_current_schema_and_rejects_wrong_schema() {
        let directory = std::env::temp_dir().join(format!("journey-backup-test-{}", hex(&rand::random::<[u8; 16]>())));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.sqlite3");
        let compressed = directory.join("source.zst");
        let output = directory.join("restored.sqlite3");
        crate::db::Database::new(source.clone()).initialize().await.unwrap();
        let db = Connection::open(&source).unwrap();
        db.execute_batch("CREATE TABLE sample (value TEXT); INSERT INTO sample VALUES ('before');").unwrap();
        drop(db);
        zstd(&source, &compressed, false).unwrap();
        verify_file(&compressed, &output).await.unwrap();
        assert_eq!(fs::metadata(&output).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(Connection::open(&output).unwrap().query_row("SELECT value FROM sample", [], |row| row.get::<_, String>(0)).unwrap(), "before");
        fs::remove_file(&output).unwrap();
        let db = Connection::open(&source).unwrap();
        db.pragma_update(None, "user_version", 9).unwrap();
        drop(db);
        fs::remove_file(&compressed).unwrap();
        zstd(&source, &compressed, false).unwrap();
        assert!(verify_file(&compressed, &output).await.is_err());
        assert!(!output.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn verify_upgrades_a_version_ten_backup_on_the_restored_file() {
        let directory = std::env::temp_dir().join(format!("journey-backup-test-{}", hex(&rand::random::<[u8; 16]>())));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("source.sqlite3");
        let compressed = directory.join("source.zst");
        let output = directory.join("restored.sqlite3");
        crate::db::Database::new(source.clone()).initialize().await.unwrap();
        let db = Connection::open(&source).unwrap();
        db.execute_batch("CREATE TABLE sample (value TEXT); INSERT INTO sample VALUES ('kept'); \
            DROP TABLE share_sessions; DROP TABLE share_link_posts; DROP TABLE share_links; \
            CREATE TABLE share_links (id TEXT PRIMARY KEY, post_id INTEGER NOT NULL REFERENCES posts(id) ON DELETE CASCADE, \
            token_digest TEXT NOT NULL, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, revoked_at INTEGER); \
            CREATE TABLE share_sessions (token_digest TEXT PRIMARY KEY, share_link_id TEXT NOT NULL REFERENCES share_links(id) ON DELETE CASCADE, \
            created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL); PRAGMA user_version = 10;").unwrap();
        drop(db);
        zstd(&source, &compressed, false).unwrap();
        verify_file(&compressed, &output).await.unwrap();
        let restored = Connection::open(&output).unwrap();
        assert_eq!(restored.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), crate::db::CURRENT_SCHEMA_VERSION);
        assert_eq!(restored.query_row("SELECT value FROM sample", [], |row| row.get::<_, String>(0)).unwrap(), "kept");
        let original = Connection::open(&source).unwrap();
        assert_eq!(original.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0)).unwrap(), 10);
        drop(original);
        drop(restored);
        let _ = fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn backups_replace_one_key_and_failed_upload_preserves_previous_snapshot() {
        let directory = std::env::temp_dir().join(format!("journey-backup-test-{}", hex(&rand::random::<[u8; 16]>())));
        fs::create_dir(&directory).unwrap();
        let source = directory.join("live.sqlite3");
        crate::db::Database::new(source.clone()).initialize().await.unwrap();
        let db = Connection::open(&source).unwrap();
        db.execute_batch("CREATE TABLE sample (value TEXT); INSERT INTO sample VALUES ('first');").unwrap();
        drop(db);
        let storage = MemoryStorage::default();
        let jobs = Jobs::default();
        create(&jobs, &source, &storage).await.unwrap();
        let first = storage.object.lock().await.clone().unwrap();
        let db = Connection::open(&source).unwrap();
        db.execute("UPDATE sample SET value = 'second'", []).unwrap();
        drop(db);
        create(&jobs, &source, &storage).await.unwrap();
        let second = storage.object.lock().await.clone().unwrap();
        assert_ne!(first, second);
        *storage.fail.lock().await = true;
        assert!(create(&jobs, &source, &storage).await.is_err());
        assert_eq!(storage.object.lock().await.as_ref().unwrap(), &second);
        let output = directory.join("restored.sqlite3");
        restore_check(&jobs, &storage, &output).await.unwrap();
        let value: String = Connection::open(&output).unwrap().query_row("SELECT value FROM sample", [], |row| row.get(0)).unwrap();
        assert_eq!(value, "second");
        let _ = fs::remove_dir_all(directory);
    }
}
