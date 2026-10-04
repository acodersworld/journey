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
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSettings {
    pub object_dir: PathBuf,
    pub initial_stream_window_size: u32,
    pub initial_connection_window_size: u32,
    pub thumbnail_time_ms: u64,
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
        }
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
