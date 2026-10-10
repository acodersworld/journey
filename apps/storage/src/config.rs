use serde::Deserialize;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub storage: StorageSettings,
    #[serde(default)]
    pub management: ManagementSettings,
    #[serde(default)]
    pub site_connection: SiteConnectionSettings,
    #[serde(default)]
    pub logging: journey_logging::LoggingSettings,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    pub object_dir: PathBuf,
    pub initial_stream_window_size: u32,
    pub initial_connection_window_size: u32,
    pub thumbnail_time_ms: u64,
    pub range_get_summary_interval_seconds: u64,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagementSettings {
    pub bind: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SiteConnectionSettings {
    pub websocket_url: String,
    pub websocket_secret: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            storage: StorageSettings::default(),
            management: ManagementSettings::default(),
            site_connection: SiteConnectionSettings::default(),
            logging: journey_logging::LoggingSettings::default(),
        }
    }
}

impl Default for StorageSettings {
    fn default() -> Self {
        Self {
            object_dir: default_object_dir(),
            initial_stream_window_size: 32 * 1024 * 1024,
            initial_connection_window_size: 64 * 1024 * 1024,
            thumbnail_time_ms: 0,
            range_get_summary_interval_seconds: 10,
        }
    }
}

impl StorageSettings {
    pub fn validate_range_get_summary_interval(&self) -> std::io::Result<()> {
        if self.range_get_summary_interval_seconds == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "storage.range_get_summary_interval_seconds must be positive",
            ));
        }
        Ok(())
    }
}

impl Default for ManagementSettings {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8082".to_owned(),
            username: String::new(),
            password: String::new(),
        }
    }
}

impl Default for SiteConnectionSettings {
    fn default() -> Self {
        Self {
            websocket_url: "ws://journey-site:8081/internal/storage".to_owned(),
            websocket_secret: String::new(),
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
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        config.storage.object_dir = resolve_path(base, config.storage.object_dir);
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

fn default_object_dir() -> PathBuf {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return data_home.join("journey/storage/objects");
    }

    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|home| home.join(".local/share/journey/storage/objects"))
        .unwrap_or_else(|| PathBuf::from("journey-storage-data/objects"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_storage_configuration_gets_logging_and_summary_defaults() {
        let config: AppConfig = toml::from_str(
            "[storage]\nobject_dir = \"objects\"\n[management]\nusername = \"user\"\npassword = \"pass\"\n[site_connection]\nwebsocket_secret = \"secret\"\n",
        )
        .unwrap();
        assert_eq!(config.logging.level, "info");
        assert_eq!(config.logging.queue_capacity, 65_536);
        assert_eq!(config.storage.range_get_summary_interval_seconds, 10);
        assert!(config.storage.validate_range_get_summary_interval().is_ok());
    }

    #[test]
    fn range_summary_interval_must_be_positive() {
        let settings = StorageSettings { range_get_summary_interval_seconds: 0, ..StorageSettings::default() };
        assert!(settings.validate_range_get_summary_interval().is_err());
    }
}
