use std::{fs::{self, OpenOptions}, io::{Read, Write}, os::unix::fs::OpenOptionsExt, path::{Path, PathBuf}, sync::{Arc, Mutex}, time::{Duration, SystemTime}};
use bytes::Bytes;
use sha2::{Digest, Sha256};
use crate::storage_interface::ReducedImage;

const MAGIC: &[u8; 8] = b"DERIV001";

#[derive(Clone, Debug)]
pub struct DerivativeCacheSettings {
    pub enabled: bool,
    pub max_bytes: u64,
    pub max_entry_bytes: u64,
    pub eviction_interval: Duration,
    pub eviction_target_percent: u8,
    pub access_timestamp_interval: Duration,
}

impl Default for DerivativeCacheSettings {
    fn default() -> Self {
        Self { enabled: true, max_bytes: 1024 * 1024 * 1024, max_entry_bytes: 64 * 1024 * 1024,
            eviction_interval: Duration::from_secs(3600), eviction_target_percent: 90,
            access_timestamp_interval: Duration::from_secs(60) }
    }
}

impl DerivativeCacheSettings {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_bytes == 0 || self.max_entry_bytes == 0 || self.max_entry_bytes > self.max_bytes {
            return Err("derivative_cache byte limits must be positive and max_entry_bytes must not exceed max_bytes".into());
        }
        if self.eviction_interval.is_zero() || self.access_timestamp_interval.is_zero() {
            return Err("derivative_cache intervals must be positive".into());
        }
        if std::time::Instant::now().checked_add(self.eviction_interval).is_none()
            || std::time::Instant::now().checked_add(self.access_timestamp_interval).is_none() {
            return Err("derivative_cache intervals are too large".into());
        }
        if !(1..=99).contains(&self.eviction_target_percent) {
            return Err("derivative_cache.eviction_target_percent must be between 1 and 99".into());
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct DerivativeCache {
    cache_dir: PathBuf,
    parts_dir: PathBuf,
    settings: DerivativeCacheSettings,
    occupied: Mutex<u64>,
}

impl DerivativeCache {
    pub(crate) fn open(root: &Path, settings: DerivativeCacheSettings) -> std::io::Result<Arc<Self>> {
        settings.validate().map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        let directory = root.join("derivative-cache");
        let cache_dir = directory.join("cache");
        let parts_dir = directory.join("parts");
        for path in [&directory, &cache_dir, &parts_dir] { ensure_dir(path)?; }
        for entry in fs::read_dir(&parts_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() { fs::remove_file(entry.path())?; }
        }
        let cache = Arc::new(Self { cache_dir, parts_dir, settings, occupied: Mutex::new(0) });
        cache.recount()?;
        if cache.occupied.lock().unwrap().to_owned() > cache.settings.max_bytes { cache.evict()?; }
        Ok(cache)
    }

    pub(crate) fn disabled(root: &Path, mut settings: DerivativeCacheSettings) -> Arc<Self> {
        settings.enabled = false;
        let directory = root.join("derivative-cache");
        Arc::new(Self {
            cache_dir: directory.join("cache"),
            parts_dir: directory.join("parts"),
            settings,
            occupied: Mutex::new(0),
        })
    }

    pub(crate) fn enabled(&self) -> bool { self.settings.enabled }
    pub(crate) fn interval(&self) -> Duration { self.settings.eviction_interval }
    pub(crate) fn identity(source: &[u8; 32], content_type: &str, request: &str) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"journey-derivative-v3\0");
        hash.update(source);
        hash.update(content_type.as_bytes());
        hash.update([0]);
        hash.update(request.as_bytes());
        hash.finalize().into()
    }

    fn path(&self, identity: &[u8; 32]) -> PathBuf { self.cache_dir.join(format!("{}.bin", hex(identity))) }

    pub(crate) fn read(&self, identity: &[u8; 32]) -> Option<ReducedImage> {
        if !self.enabled() { return None; }
        let path = self.path(identity);
        let mut file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(error) => { log::warn!("derivative cache read failed: {error}"); return None; }
        };
        let result = (|| -> std::io::Result<ReducedImage> {
            let length = file.metadata()?.len();
            if length > self.settings.max_entry_bytes || length < 49 { return Err(std::io::Error::from(std::io::ErrorKind::InvalidData)); }
            let mut header = [0; 49];
            file.read_exact(&mut header)?;
            if &header[..8] != MAGIC || &header[8..40] != identity { return Err(std::io::Error::from(std::io::ErrorKind::InvalidData)); }
            let payload_len = u64::from_le_bytes(header[40..48].try_into().unwrap());
            let content_type: &'static str = match header[48] { 1 => "image/jpeg", 2 => "image/png", 3 => "image/webp", _ => return Err(std::io::Error::from(std::io::ErrorKind::InvalidData)) };
            if payload_len == 0 || payload_len.checked_add(49) != Some(length) { return Err(std::io::Error::from(std::io::ErrorKind::InvalidData)); }
            let mut bytes = vec![0; payload_len as usize];
            file.read_exact(&mut bytes)?;
            Ok(ReducedImage::new(Bytes::from(bytes), content_type))
        })();
        match result {
            Ok(image) => {
                if file.metadata().and_then(|metadata| metadata.modified()).ok()
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|elapsed| elapsed >= self.settings.access_timestamp_interval) &&
                        let Err(error) = file.set_modified(SystemTime::now()) {
                    log::warn!("derivative cache access timestamp failed: {error}");
                }
                Some(image)
            }
            Err(error) => {
                log::warn!("derivative cache entry invalid {}: {error}", path.display());
                let mut occupied = self.occupied.lock().unwrap();
                if fs::remove_file(&path).is_ok() && let Ok(metadata) = file.metadata() {
                    *occupied = occupied.saturating_sub(metadata.len());
                }
                None
            }
        }
    }

    pub(crate) fn write(&self, identity: &[u8; 32], image: &ReducedImage) {
        if !self.enabled() { return; }
        if let Err(error) = self.write_inner(identity, image) { log::warn!("derivative cache fill failed: {error}"); }
    }

    fn write_inner(&self, identity: &[u8; 32], image: &ReducedImage) -> std::io::Result<()> {
        let type_id = match image.content_type() { "image/jpeg" => 1, "image/png" => 2, "image/webp" => 3, _ => return Ok(()) };
        let length = image.bytes().len() as u64 + 49;
        if length > self.settings.max_entry_bytes { return Ok(()); }
        let mut occupied = self.occupied.lock().unwrap();
        if occupied.checked_add(length).is_none_or(|total| total > self.settings.max_bytes) { return Ok(()); }
        let result = (|| -> std::io::Result<bool> {
            let path = self.parts_dir.join(format!("{}-{}.part", hex(identity), unique_id()));
            let mut file = OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&path)?;
            let write_result = (|| -> std::io::Result<bool> {
                file.write_all(MAGIC)?;
                file.write_all(identity)?;
                file.write_all(&(image.bytes().len() as u64).to_le_bytes())?;
                file.write_all(&[type_id])?;
                file.write_all(image.bytes())?;
                file.sync_all()?;
                match fs::hard_link(&path, self.path(identity)) {
                    Ok(()) => Ok(true),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                    Err(error) => Err(error),
                }
            })();
            if let Err(error) = fs::remove_file(&path) { log::warn!("derivative cache part cleanup failed: {error}"); }
            write_result
        })();
        if matches!(result, Ok(true)) { *occupied += length; }
        result.map(|_| ())
    }

    fn recount(&self) -> std::io::Result<()> {
        let mut bytes = 0u64;
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if entry.file_type()?.is_file() { bytes = bytes.saturating_add(entry.metadata()?.len()); }
        }
        *self.occupied.lock().unwrap() = bytes;
        Ok(())
    }

    pub(crate) fn evict(&self) -> std::io::Result<()> {
        if !self.enabled() { return Ok(()); }
        let mut occupied = self.occupied.lock().unwrap();
        let mut entries = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.cache_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() { continue; }
            let metadata = entry.metadata()?;
            total = total.saturating_add(metadata.len());
            entries.push((metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH), metadata.len(), entry.path()));
        }
        let target = (self.settings.max_bytes as u128 * self.settings.eviction_target_percent as u128 / 100) as u64;
        if total > target {
            entries.sort_by_key(|entry| entry.0);
            for (_, length, path) in entries {
                if total <= target { break; }
                fs::remove_file(path)?;
                total = total.saturating_sub(length);
            }
        }
        *occupied = total;
        Ok(())
    }
}

fn ensure_dir(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(()),
        Ok(_) => Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "cache path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path),
        Err(error) => Err(error),
    }
}

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|byte| format!("{byte:02x}")).collect() }
fn unique_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("journey-derivative-test-{}", unique_id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Root { fn drop(&mut self) { fs::remove_dir_all(&self.0).unwrap(); } }
    fn image(bytes: &'static [u8]) -> ReducedImage { ReducedImage::new(Bytes::from_static(bytes), "image/jpeg") }

    #[test]
    fn cache_round_trip_corruption_and_restart_cleanup() {
        let root = Root::new();
        let cache = DerivativeCache::open(&root.0, DerivativeCacheSettings::default()).unwrap();
        let identity = DerivativeCache::identity(&[1; 32], "image/jpeg", "quality:80");
        cache.write(&identity, &image(b"payload"));
        assert_eq!(cache.read(&identity).unwrap().bytes().as_ref(), b"payload");
        fs::write(cache.parts_dir.join("abandoned.part"), b"part").unwrap();
        let entry = cache.path(&identity);
        fs::write(&entry, b"broken").unwrap();
        assert!(cache.read(&identity).is_none());
        cache.write(&identity, &image(b"repaired"));
        assert_eq!(cache.read(&identity).unwrap().bytes().as_ref(), b"repaired");
        drop(cache);
        let cache = DerivativeCache::open(&root.0, DerivativeCacheSettings::default()).unwrap();
        assert_eq!(fs::read_dir(&cache.parts_dir).unwrap().count(), 0);
        assert_eq!(cache.read(&identity).unwrap().bytes().as_ref(), b"repaired");
    }

    #[test]
    fn duplicate_publication_capacity_and_lru_eviction() {
        let root = Root::new();
        let settings = DerivativeCacheSettings { max_bytes: 120, max_entry_bytes: 80, eviction_target_percent: 50, ..DerivativeCacheSettings::default() };
        let cache = DerivativeCache::open(&root.0, settings).unwrap();
        let first = DerivativeCache::identity(&[1; 32], "image/jpeg", "one");
        let second = DerivativeCache::identity(&[2; 32], "image/jpeg", "two");
        cache.write(&first, &image(b"first"));
        cache.write(&first, &image(b"other"));
        assert_eq!(cache.read(&first).unwrap().bytes().as_ref(), b"first");
        cache.write(&second, &image(b"second"));
        assert_eq!(cache.read(&second).unwrap().bytes().as_ref(), b"second");
        let third = DerivativeCache::identity(&[3; 32], "image/jpeg", "three");
        cache.write(&third, &image(b"third"));
        assert!(cache.read(&third).is_none());
        cache.evict().unwrap();
        assert_eq!(fs::read_dir(&cache.cache_dir).unwrap().count(), 1);
        cache.write(&third, &image(b"third"));
        assert_eq!(cache.read(&third).unwrap().bytes().as_ref(), b"third");
    }
}
