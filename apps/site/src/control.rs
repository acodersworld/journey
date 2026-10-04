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
    db::Database,
    import::{self, ImportResult},
    shutdown,
    storage::StorageClient,
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Serialize, Deserialize)]
struct ControlRequest {
    manifest_path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct ControlResponse {
    result: Option<ImportResult>,
    error: Option<String>,
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
            result = handle(stream, &database, &storage) => {
                if let Err(error) = result {
                    eprintln!("site control request failed: {error}");
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
        manifest_path: manifest_path.to_path_buf(),
    })?;
    stream.write_all(&request).await?;
    stream.write_all(b"\n").await?;
    let mut response_line = String::new();
    BufReader::new(stream).read_line(&mut response_line).await?;
    if response_line.is_empty() {
        return Err("running site closed the control connection without a result".into());
    }
    let response: ControlResponse = serde_json::from_str(&response_line)?;
    match (response.result, response.error) {
        (Some(result), None) => Ok(result),
        (None, Some(error)) => Err(error.into()),
        _ => Err("running site returned an invalid control response".into()),
    }
}

async fn handle<S: StorageClient>(
    stream: UnixStream,
    database: &Database,
    storage: &S,
) -> AppResult<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let result: AppResult<ImportResult> = async {
        let request = serde_json::from_str::<ControlRequest>(&request_line)?;
        if !request.manifest_path.is_absolute() {
            return Err("manifest path from the control socket must be absolute".into());
        }
        let prepared = import::prepare_manifest(&request.manifest_path).await?;
        import::apply_import(prepared, database, storage).await
    }
    .await;
    let response = match result {
        Ok(result) => ControlResponse { result: Some(result), error: None },
        Err(error) => ControlResponse { result: None, error: Some(error.to_string()) },
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
