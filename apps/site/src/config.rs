use serde::Deserialize;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

use crate::control;

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    #[serde(default)]
    pub site: SiteSettings,
    #[serde(default)]
    pub storage: StorageSettings,
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
    pub max_media_upload_bytes: u64,
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

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            site: SiteSettings::default(),
            storage: StorageSettings::default(),
        }
    }
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
            max_media_upload_bytes: crate::web::DEFAULT_MAX_MEDIA_UPLOAD_BYTES,
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
        let base = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        config.site.database_path = resolve_path(base, config.site.database_path);
        config.site.control_socket = resolve_path(base, config.site.control_socket);
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
