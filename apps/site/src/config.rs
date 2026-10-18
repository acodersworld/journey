use serde::Deserialize;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use crate::control;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
#[derive(Default)]
pub struct AppConfig {
    #[serde(default)]
    pub backup: BackupSettings,
    #[serde(default)]
    pub diagnostics: DiagnosticsSettings,
    #[serde(default)]
    pub image_cache: crate::image_cache::Settings,
    #[serde(default)]
    pub site: SiteSettings,
    #[serde(default)]
    pub storage: StorageSettings,
    #[serde(default)]
    pub logging: journey_logging::LoggingSettings,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupSettings { pub daily_utc: String }

impl Default for BackupSettings {
    fn default() -> Self { Self { daily_utc: "03:00".to_owned() } }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiagnosticsSettings {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SiteSettings {
    pub bind: String,
    pub database_path: PathBuf,
    pub public_origin: Option<String>,
    pub control_socket: PathBuf,
    pub allow_insecure_lan_http: bool,
    pub session_ttl_seconds: i64,
    pub whatsapp_preview_image_ttl_seconds: i64,
    pub max_media_upload_bytes: u64,
    pub image_quality: u8,
    pub mosaic_max_tiles: usize,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    pub transport: String,
    pub h2c_address: String,
    pub websocket_bind: String,
    pub websocket_secret: String,
    pub initial_window_size: u32,
    pub initial_connection_window_size: u32,
}


impl Default for SiteSettings {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".to_owned(),
            database_path: PathBuf::from("journey-site.sqlite3"),
            public_origin: None,
            control_socket: control::default_socket_path(),
            allow_insecure_lan_http: false,
            session_ttl_seconds: 7 * 24 * 60 * 60,
            whatsapp_preview_image_ttl_seconds: 10,
            max_media_upload_bytes: crate::web::DEFAULT_MAX_MEDIA_UPLOAD_BYTES,
            image_quality: 80,
            mosaic_max_tiles: 8,
        }
    }
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            transport: "h2c".to_owned(),
            h2c_address: "127.0.0.1:8081".to_owned(),
            websocket_bind: "127.0.0.1:8081".to_owned(),
            websocket_secret: String::new(),
            initial_window_size: 512 * 1024,
            initial_connection_window_size: 4 * 1024 * 1024,
        }
    }
}

impl AppConfig {
    pub fn load(config_path: Option<PathBuf>) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let path = config_path
            .or_else(|| std::env::var_os("JOURNEY_CONFIG").map(PathBuf::from))
            .ok_or("configuration file required; pass --config or set JOURNEY_CONFIG")?;
        let contents = fs::read_to_string(&path).map_err(|error| {
            format!("could not read TOML configuration file {}: {error}", path.display())
        })?;
        let mut config: Self = toml::from_str(&contents).map_err(|error| {
            format!("could not parse TOML configuration file {}: {error}", path.display())
        })?;
        crate::backup::parse_daily(&config.backup.daily_utc)?;
        config.image_cache.validate()?;
        crate::storage::ImageQuality::new(config.site.image_quality)?;
        if config.site.mosaic_max_tiles == 0 {
            return Err("site.mosaic_max_tiles must be a positive integer".into());
        }
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        config.site.database_path = resolve_path(base, config.site.database_path);
        config.site.control_socket = resolve_path(base, config.site.control_socket);
        config.image_cache.directory = Some(match config.image_cache.directory.take() {
            Some(directory) => resolve_path(base, directory),
            None => config.site.database_path.parent().unwrap_or(base).join("image-cache"),
        });
        Ok(config)
    }
}

fn resolve_path(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_CONFIG_TEST: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn legacy_site_configuration_gets_logging_defaults() {
        let config: AppConfig = toml::from_str(
            "[site]\nbind = \"127.0.0.1:8080\"\n[storage]\ntransport = \"h2c\"\n",
        )
        .unwrap();
        assert_eq!(config.logging.level, "info");
        assert!(!config.diagnostics.enabled);
        assert_eq!(config.logging.queue_capacity, 65_536);
        assert_eq!(config.logging.slow_enqueue_warning_ms, 100);
        assert_eq!(config.site.image_quality, 80);
        assert_eq!(config.site.mosaic_max_tiles, 8);
    }

    #[test]
    fn image_quality_configuration_validates_range() {
        let directory = std::env::temp_dir().join(format!("journey-config-quality-{}-{}", std::process::id(), NEXT_CONFIG_TEST.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("site.toml");
        for value in [1, 80, 100] {
            fs::write(&path, format!("[site]\nimage_quality = {value}\n")).unwrap();
            assert_eq!(AppConfig::load(Some(path.clone())).unwrap().site.image_quality, value);
        }
        for value in [0, 101, 256] {
            fs::write(&path, format!("[site]\nimage_quality = {value}\n")).unwrap();
            assert!(AppConfig::load(Some(path.clone())).is_err());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn mosaic_tile_limit_requires_a_positive_integer() {
        let directory = std::env::temp_dir().join(format!("journey-config-mosaic-{}-{}", std::process::id(), NEXT_CONFIG_TEST.fetch_add(1, Ordering::Relaxed)));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("site.toml");
        fs::write(&path, "[site]\nmosaic_max_tiles = 3\n").unwrap();
        assert_eq!(AppConfig::load(Some(path.clone())).unwrap().site.mosaic_max_tiles, 3);
        for invalid in ["0", "-1", "1.5", "\"eight\""] {
            fs::write(&path, format!("[site]\nmosaic_max_tiles = {invalid}\n")).unwrap();
            assert!(AppConfig::load(Some(path.clone())).is_err());
        }
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn image_cache_directory_defaults_beside_database_and_resolves_explicit_paths() {
        let directory = std::env::temp_dir().join(format!(
            "journey-config-image-cache-{}-{}",
            std::process::id(),
            NEXT_CONFIG_TEST.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&directory).unwrap();
        let config_path = directory.join("site.toml");
        fs::write(&config_path, "[site]\ndatabase_path = \"data/site.sqlite3\"\n").unwrap();
        let config = AppConfig::load(Some(config_path.clone())).unwrap();
        assert_eq!(config.site.database_path, directory.join("data/site.sqlite3"));
        assert_eq!(config.image_cache.directory, Some(directory.join("data/image-cache")));

        fs::write(
            &config_path,
            "[image_cache]\nenabled = true\ndirectory = \"./cache-data\"\n",
        ).unwrap();
        let config = AppConfig::load(Some(config_path)).unwrap();
        assert_eq!(config.image_cache.directory, Some(directory.join("cache-data")));
        fs::remove_dir_all(directory).unwrap();
    }
}
