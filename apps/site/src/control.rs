use std::{
    error::Error,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};

use crate::{
    backup,
    db::Database,
    import::{self, ImportResult},
    performance,
    shutdown,
    storage::StorageClient,
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Serialize, Deserialize)]
struct ControlRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance: Option<performance::Request>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    performance_urls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_key: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum ControlResponse {
    Backup { backup: bool },
    Import { result: ImportResult },
    Performance { performance: performance::Result },
    PerformanceUrls { performance_urls: PerformanceUrls },
    ImageCacheCleared { image_cache_cleared: usize },
    Error { error: String },
}

#[derive(Deserialize)]
struct ControlResponseFields {
    #[serde(default)]
    backup: Option<bool>,
    #[serde(default)]
    result: Option<ImportResult>,
    #[serde(default)]
    performance: Option<performance::Result>,
    #[serde(default)]
    performance_urls: Option<PerformanceUrls>,
    #[serde(default)]
    image_cache_cleared: Option<usize>,
    #[serde(default)]
    error: Option<String>,
}

impl<'de> Deserialize<'de> for ControlResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        let fields = ControlResponseFields::deserialize(deserializer)?;
        match (
            fields.backup,
            fields.result,
            fields.performance,
            fields.performance_urls,
            fields.image_cache_cleared,
            fields.error,
        ) {
            (Some(backup), None, None, None, None, None) => Ok(Self::Backup { backup }),
            (None, Some(result), None, None, None, None) => Ok(Self::Import { result }),
            (None, None, Some(performance), None, None, None) => Ok(Self::Performance { performance }),
            (None, None, None, Some(performance_urls), None, None) => Ok(Self::PerformanceUrls { performance_urls }),
            (None, None, None, None, Some(image_cache_cleared), None) => Ok(Self::ImageCacheCleared { image_cache_cleared }),
            (None, None, None, None, None, Some(error)) => Ok(Self::Error { error }),
            _ => Err(D::Error::custom("control response must contain exactly one result")),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PerformanceUrls {
    pub site: String,
    pub storage: String,
}

#[derive(Clone)]
pub struct PerformanceSettings {
    pub enabled: bool,
    pub transport: String,
    pub initial_stream_window_size: u32,
    pub initial_connection_window_size: u32,
    pub token: Option<String>,
    pub origin: String,
}

impl PerformanceSettings {
    fn urls(&self) -> Result<PerformanceUrls, &'static str> {
        if !self.enabled {
            return Err("performance diagnostics are disabled");
        }
        let token = self.token.as_deref().ok_or("performance diagnostics are disabled")?;
        let origin = self.origin.trim_end_matches('/');
        Ok(PerformanceUrls {
            site: format!("{origin}/perftest/{token}/site"),
            storage: format!("{origin}/perftest/{token}/storage"),
        })
    }
}

pub async fn bind(socket_path: &Path) -> AppResult<UnixListener> {
    let parent = socket_path
        .parent()
        .ok_or("control socket path has no parent directory")?;
    tokio::fs::create_dir_all(parent).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).await?;
    }

    match tokio::fs::symlink_metadata(&socket_path).await {
        Ok(metadata) => {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                return Err(format!("control socket path exists and is not a socket: {}", socket_path.display()).into());
            }
            if UnixStream::connect(&socket_path).await.is_ok() {
                return Err(format!("another journey-site process is using {}", socket_path.display()).into());
            }
            tokio::fs::remove_file(&socket_path).await?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    let listener = UnixListener::bind(&socket_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = tokio::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).await {
            drop(listener);
            let _ = tokio::fs::remove_file(socket_path).await;
            return Err(error.into());
        }
    }
    Ok(listener)
}

pub struct BoundSocketPath {
    path: PathBuf,
    bound: bool,
}

impl BoundSocketPath {
    pub fn new(path: PathBuf) -> Self {
        Self { path, bound: true }
    }

    pub async fn remove(&mut self) -> std::io::Result<()> {
        if !self.bound {
            return Ok(());
        }
        match tokio::fs::remove_file(&self.path).await {
            Ok(()) => self.bound = false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => self.bound = false,
            Err(error) => return Err(error),
        }
        Ok(())
    }
}

impl Drop for BoundSocketPath {
    fn drop(&mut self) {
        if self.bound {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub async fn serve<S: StorageClient>(
    listener: UnixListener,
    database: Database,
    storage: S,
    backup_jobs: backup::Jobs,
    image_cache: Option<std::sync::Arc<crate::image_cache::ImageCache>>,
    performance_settings: PerformanceSettings,
    mut shutdown: shutdown::Receiver,
) -> std::io::Result<()> {
    loop {
        let (stream, _) = tokio::select! {
            result = listener.accept() => result?,
            result = shutdown::requested(&mut shutdown) => {
                return result.map_err(std::io::Error::other);
            }
        };
        tokio::select! {
            biased;
            result = handle(stream, &database, &storage, &backup_jobs, image_cache.as_ref(), &performance_settings) => {
                if let Err(error) = result {
                    log::error!("site control request failed: {error}");
                }
            }
            result = shutdown::requested(&mut shutdown) => {
                return result.map_err(std::io::Error::other);
            }
        }
    }
}

pub async fn request_import(manifest_path: &Path, socket_path: &Path) -> AppResult<ImportResult> {
    if !manifest_path.is_absolute() {
        return Err("--via-running-site requires an absolute container-visible manifest path".into());
    }
    let mut stream = UnixStream::connect(&socket_path).await.map_err(|error| {
        std::io::Error::other(format!("could not connect to running site at {}: {error}", socket_path.display()))
    })?;
    let request = serde_json::to_vec(&ControlRequest {
        command: Some("import".to_owned()), output_path: None,
        manifest_path: Some(manifest_path.to_path_buf()),
        performance: None,
        performance_urls: None,
        cache_key: None,
    })?;
    stream.write_all(&request).await?;
    stream.write_all(b"\n").await?;
    let mut response_line = String::new();
    BufReader::new(stream).read_line(&mut response_line).await?;
    if response_line.is_empty() {
        return Err("running site closed the control connection without a result".into());
    }
    let response: ControlResponse = serde_json::from_str(&response_line)?;
    match response {
        ControlResponse::Import { result } => Ok(result),
        ControlResponse::Error { error } => Err(error.into()),
        _ => Err("running site returned an invalid control response".into()),
    }
}

pub async fn request_backup_create(socket_path: &Path) -> AppResult<()> {
    request_backup(socket_path, "backup_create", None).await
}

pub async fn request_backup_restore(socket_path: &Path, output: &Path) -> AppResult<()> {
    if !output.is_absolute() { return Err("restore output must be an absolute container-visible path".into()); }
    request_backup(socket_path, "backup_restore", Some(output)).await
}

async fn request_backup(socket_path: &Path, command: &str, output: Option<&Path>) -> AppResult<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(&serde_json::to_vec(&ControlRequest {
        command: Some(command.to_owned()), output_path: output.map(Path::to_path_buf),
        manifest_path: None, performance: None, performance_urls: None, cache_key: None,
    })?).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() { return Err("running site closed the control connection without a result".into()); }
    let response: ControlResponse = serde_json::from_str(&line)?;
    match response {
        ControlResponse::Backup { backup: true } => Ok(()),
        ControlResponse::Error { error } => Err(error.into()),
        _ => Err("running site returned an invalid backup response".into()),
    }
}

pub async fn request_performance(
    request: performance::Request,
    socket_path: &Path,
) -> AppResult<performance::Result> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(&serde_json::to_vec(&ControlRequest {
        command: Some("performance".to_owned()), output_path: None,
        manifest_path: None, performance: Some(request), performance_urls: None,
        cache_key: None,
    })?).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        return Err("running site closed the control connection without a result".into());
    }
    let response: ControlResponse = serde_json::from_str(&line)?;
    match response {
        ControlResponse::Performance { performance } => Ok(performance),
        ControlResponse::Error { error } => Err(error.into()),
        _ => Err("running site returned an invalid performance response".into()),
    }
}

pub async fn request_performance_urls(socket_path: &Path) -> AppResult<PerformanceUrls> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(&serde_json::to_vec(&ControlRequest {
        command: Some("performance_urls".to_owned()), output_path: None,
        manifest_path: None, performance: None, performance_urls: Some(true),
        cache_key: None,
    })?).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        return Err("running site closed the control connection without a result".into());
    }
    let response: ControlResponse = serde_json::from_str(&line)?;
    match response {
        ControlResponse::PerformanceUrls { performance_urls } => Ok(performance_urls),
        ControlResponse::Error { error } => Err(error.into()),
        _ => Err("running site returned an invalid performance URL response".into()),
    }
}

pub async fn request_image_cache_clear(storage_key: Option<&str>, socket_path: &Path) -> AppResult<usize> {
    let mut stream = UnixStream::connect(socket_path).await?;
    let request = ControlRequest {
        command: Some("image_cache_clear".to_owned()),
        output_path: None,
        manifest_path: None,
        performance: None,
        performance_urls: None,
        cache_key: storage_key.map(str::to_owned),
    };
    stream.write_all(&serde_json::to_vec(&request)?).await?;
    stream.write_all(b"\n").await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    if line.is_empty() {
        return Err("running site closed the control connection without a result".into());
    }
    let response: ControlResponse = serde_json::from_str(&line)?;
    match response {
        ControlResponse::ImageCacheCleared { image_cache_cleared } => Ok(image_cache_cleared),
        ControlResponse::Error { error } => Err(error.into()),
        _ => Err("running site returned an invalid image cache response".into()),
    }
}

async fn handle<S: StorageClient>(
    stream: UnixStream,
    database: &Database,
    storage: &S,
    backup_jobs: &backup::Jobs,
    image_cache: Option<&std::sync::Arc<crate::image_cache::ImageCache>>,
    performance_settings: &PerformanceSettings,
) -> AppResult<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let result: AppResult<ControlResponse> = async {
        let request = serde_json::from_str::<ControlRequest>(&request_line)?;
        if let Some(command) = request.command.as_deref() {
            match command {
                "backup_create" => {
                    if request.output_path.is_some() || request.manifest_path.is_some() || request.performance.is_some() || request.performance_urls.is_some() { return Err("invalid backup request".into()); }
                    backup::create(backup_jobs, database.path(), storage).await.map_err(|error| format!("backup create key={} failed: {error}", backup::KEY))?;
                    return Ok(ControlResponse::Backup { backup: true });
                }
                "backup_restore" => {
                    let output = request.output_path.as_deref().ok_or("missing restore output path")?;
                    if request.manifest_path.is_some() || request.performance.is_some() || request.performance_urls.is_some() { return Err("invalid backup request".into()); }
                    backup::restore_check(backup_jobs, storage, output).await.map_err(|error| format!("backup restore-check key={} failed: {error}", backup::KEY))?;
                    return Ok(ControlResponse::Backup { backup: true });
                }
                "import" | "performance" | "performance_urls" => {}
                "image_cache_clear" => {
                    if request.output_path.is_some() || request.manifest_path.is_some() || request.performance.is_some() || request.performance_urls.is_some() {
                        return Err("invalid image cache clear request".into());
                    }
                    let cache = image_cache.ok_or("image cache is disabled")?;
                    let removed = cache.clear(request.cache_key.as_deref()).await?;
                    return Ok(ControlResponse::ImageCacheCleared { image_cache_cleared: removed });
                }
                _ => return Err("unknown control command".into()),
            }
        }
        if request.performance_urls == Some(true) {
            if request.manifest_path.is_some() || request.performance.is_some() {
                return Err("control request must select one command".into());
            }
            let urls = performance_settings.urls()?;
            return Ok(ControlResponse::PerformanceUrls { performance_urls: urls });
        }
        if let Some(performance_request) = request.performance {
            if request.manifest_path.is_some() {
                return Err("control request must select one command".into());
            }
            if !performance_settings.enabled {
                return Err("performance diagnostics are disabled".into());
            }
            let result = performance::run(storage.clone(), performance_request,
                performance_settings.transport.clone(),
                performance_settings.initial_stream_window_size,
                performance_settings.initial_connection_window_size).await?;
            return Ok(ControlResponse::Performance { performance: result });
        }
        let manifest_path = request.manifest_path.ok_or("control request is missing a command")?;
        if !manifest_path.is_absolute() {
            return Err("manifest path from the control socket must be absolute".into());
        }
        let prepared = import::prepare_manifest(&manifest_path).await?;
        let result = import::apply_import(prepared, database, storage).await?;
        Ok(ControlResponse::Import { result })
    }
    .await;
    let response = match result {
        Ok(result) => result,
        Err(error) => ControlResponse::Error { error: error.to_string() },
    };
    let mut stream = reader.into_inner();
    stream.write_all(&serde_json::to_vec(&response)?).await?;
    stream.write_all(b"\n").await?;
    Ok(())
}

pub fn default_socket_path() -> PathBuf {
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        return runtime_dir.join("journey-site/control.sock");
    }

    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir()
        .join(format!("journey-site-{uid}"))
        .join("control.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_import_request_format_remains_valid() {
        let request: ControlRequest = serde_json::from_str(r#"{"manifest_path":"/tmp/import.json"}"#).unwrap();
        assert_eq!(request.manifest_path.as_deref(), Some(Path::new("/tmp/import.json")));
        assert!(request.performance.is_none());
        let request = ControlRequest { command: None, output_path: None, manifest_path: Some(PathBuf::from("/tmp/import.json")), performance: None, performance_urls: None, cache_key: None };
        assert_eq!(serde_json::to_string(&request).unwrap(), r#"{"manifest_path":"/tmp/import.json"}"#);
    }

    #[test]
    fn control_responses_keep_flat_wire_format_and_reject_ambiguous_shapes() {
        let response = ControlResponse::ImageCacheCleared { image_cache_cleared: 0 };
        let json = serde_json::to_string(&response).unwrap();
        assert_eq!(json, r#"{"image_cache_cleared":0}"#);
        assert!(matches!(
            serde_json::from_str::<ControlResponse>(&json).unwrap(),
            ControlResponse::ImageCacheCleared { image_cache_cleared: 0 }
        ));

        let response: ControlResponse = serde_json::from_str(r#"{"error":"cache is disabled"}"#).unwrap();
        assert!(matches!(response, ControlResponse::Error { error } if error == "cache is disabled"));
        assert!(serde_json::from_str::<ControlResponse>(r#"{"backup":true,"error":"failed"}"#).is_err());
        assert!(serde_json::from_str::<ControlResponse>("{}").is_err());
    }

    #[test]
    fn capability_urls_require_enabled_diagnostics() {
        let mut settings = PerformanceSettings {
            enabled: false, transport: "websocket".to_owned(),
            initial_stream_window_size: 1, initial_connection_window_size: 1,
            token: None, origin: "https://example.test".to_owned(),
        };
        assert!(settings.urls().is_err());
        settings.enabled = true;
        settings.token = Some("random-token".to_owned());
        assert_eq!(settings.urls().unwrap().site, "https://example.test/perftest/random-token/site");
    }
}
