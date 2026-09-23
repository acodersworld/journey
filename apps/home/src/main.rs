use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use futures_util::future::poll_fn;
use h2::RecvStream;
use http::{header, Method, Request, Response, StatusCode, Version};
use journey_websocket::{
    connect_websocket_with_connector, server_session, tungstenite::ClientRequestBuilder, Config,
    Connector, ServerSession,
};
use rustls::{ClientConfig, RootCertStore};
use rustls_native_certs::load_native_certs;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::{
    fs::{self, File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    task::JoinSet,
    time::sleep,
};

const MAX_UPLOAD_SIZE: u64 = 256 * 1024 * 1024;
const FILE_CHUNK_SIZE: usize = 64 * 1024;
const TRANSFER_LOG_INTERVAL: u64 = 100;
static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
struct Fixture {
    path: PathBuf,
    content_type: &'static str,
    length: u64,
    etag: String,
}

#[derive(Clone)]
struct HomeState {
    image: Fixture,
    video: Fixture,
    uploads: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let image_path = fixture_path("image.jpg")?;
    let video_path = fixture_path("video.mp4")?;
    let uploads = PathBuf::from(
        std::env::var("HOME_UPLOADS_DIR").unwrap_or_else(|_| "/var/lib/journey/uploads".to_owned()),
    );
    fs::create_dir_all(&uploads).await?;
    let state = Arc::new(HomeState {
        image: load_fixture(image_path, "image/jpeg").await?,
        video: load_fixture(video_path, "video/mp4").await?,
        uploads,
    });
    let websocket_url = std::env::var("HOME_WEBSOCKET_URL")
        .unwrap_or_else(|_| "ws://gateway:9000".to_owned());
    let connector = build_connector(std::env::var("HOME_CA_CERT").ok())?;
    let home_username = std::env::var("JOURNEY_HOME_USERNAME")?;
    let home_password = std::env::var("JOURNEY_HOME_PASSWORD")?;
    println!("home agent connecting to gateway");

    loop {
        let request = websocket_request(&websocket_url, &home_username, &home_password)?;
        let config = Config::default();
        match connect_websocket_with_connector(request, config, connector.clone()).await {
            Ok((websocket, _)) => {
                println!("home WebSocket connected to gateway");
                match server_session(websocket, config).await {
                    Ok(session) => run_server_session(session, Arc::clone(&state)).await,
                    Err(error) => eprintln!("home HTTP/2 session failed: {error}"),
                }
            }
            Err(error) => eprintln!("home WebSocket connection failed: {error}"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn fixture_path(name: &str) -> Result<PathBuf, std::io::Error> {
    let directory = std::env::var("HOME_FIXTURES_DIR")
        .unwrap_or_else(|_| "/var/lib/journey/fixtures".to_owned());
    Ok(Path::new(&directory).join(name))
}

async fn load_fixture(path: PathBuf, content_type: &'static str) -> Result<Fixture, std::io::Error> {
    let metadata = fs::metadata(&path).await?;
    let mut file = File::open(&path).await?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; FILE_CHUNK_SIZE];
    loop {
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    let etag = format!("\"{:x}\"", digest.finalize());
    Ok(Fixture {
        path,
        content_type,
        length: metadata.len(),
        etag,
    })
}

fn websocket_request(
    url: &str,
    username: &str,
    password: &str,
) -> Result<ClientRequestBuilder, Box<dyn std::error::Error + Send + Sync>> {
    let encoded = STANDARD.encode(format!("{username}:{password}"));
    let uri = url.parse()?;
    Ok(ClientRequestBuilder::new(uri).with_header(
        "Authorization",
        format!("Basic {encoded}"),
    ))
}

fn build_connector(
    ca_path: Option<String>,
) -> Result<Option<Connector>, Box<dyn std::error::Error + Send + Sync>> {
    let Some(ca_path) = ca_path else {
        return Ok(None);
    };
    let mut roots = RootCertStore::empty();
    let native = load_native_certs();
    for certificate in native.certs {
        roots.add(certificate)?;
    }
    if !native.errors.is_empty() {
        eprintln!("some native root certificates could not be loaded");
    }
    let pem = std::fs::read(ca_path)?;
    let mut pem_slice = pem.as_slice();
    let mut found = false;
    for certificate in rustls_pemfile::certs(&mut pem_slice) {
        roots.add(certificate?)?;
        found = true;
    }
    if !found {
        return Err("HOME_CA_CERT did not contain a PEM certificate".into());
    }
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Some(Connector::Rustls(Arc::new(config))))
}

async fn run_server_session(mut session: ServerSession, state: Arc<HomeState>) {
    let mut handlers = JoinSet::new();
    loop {
        match session.accept().await {
            Ok(Some((request, respond))) => {
                let state = Arc::clone(&state);
                handlers.spawn(async move {
                    if let Err(error) = respond_to(request, respond, state).await {
                        eprintln!("home request failed: {error}");
                    }
                });
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("home HTTP/2 session failed: {error}");
                break;
            }
        }
    }
    while let Some(result) = handlers.join_next().await {
        if let Err(error) = result {
            eprintln!("home request task failed: {error}");
        }
    }
    if let Err(error) = session.wait().await {
        eprintln!("home WebSocket session failed: {error}");
    }
}

async fn respond_to(
    request: Request<RecvStream>,
    respond: h2::server::SendResponse<Bytes>,
    state: Arc<HomeState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let target = request.uri().path_and_query().map(|value| value.as_str());
    match (request.method(), target) {
        (&Method::GET | &Method::HEAD, Some("/fixtures/image.jpg")) => {
            serve_fixture(request, respond, &state.image).await
        }
        (&Method::GET | &Method::HEAD, Some("/fixtures/video.mp4")) => {
            serve_fixture(request, respond, &state.video).await
        }
        (&Method::GET, Some("/health")) => send_text(respond, StatusCode::OK, "healthy\n").await,
        (&Method::PUT, Some("/uploads")) => upload(request, respond, &state.uploads).await,
        _ => send_text(respond, StatusCode::NOT_FOUND, "not found\n").await,
    }
}

async fn send_text(
    mut respond: h2::server::SendResponse<Bytes>,
    status: StatusCode,
    body: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bytes = Bytes::copy_from_slice(body.as_bytes());
    let response = Response::builder()
        .version(Version::HTTP_2)
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(())?;
    if bytes.is_empty() {
        respond.send_response(response, true)?;
    } else {
        let mut stream = respond.send_response(response, false)?;
        stream.send_data(bytes, true)?;
    }
    Ok(())
}

enum RequestedRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
    Malformed,
}

fn requested_range(value: Option<&http::HeaderValue>, length: u64) -> RequestedRange {
    let Some(value) = value else {
        return RequestedRange::Full;
    };
    let Ok(value) = value.to_str() else {
        return RequestedRange::Malformed;
    };
    let Some(value) = value.strip_prefix("bytes=") else {
        return RequestedRange::Malformed;
    };
    if value.contains(',') {
        return RequestedRange::Malformed;
    }
    let Some((start, end)) = value.split_once('-') else {
        return RequestedRange::Malformed;
    };
    if length == 0 {
        return RequestedRange::Unsatisfiable;
    }
    if start.is_empty() {
        let Ok(suffix) = end.parse::<u64>() else {
            return RequestedRange::Malformed;
        };
        if suffix == 0 {
            return RequestedRange::Unsatisfiable;
        }
        return RequestedRange::Partial {
            start: length.saturating_sub(suffix),
            end: length - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return RequestedRange::Malformed;
    };
    if start >= length {
        return RequestedRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return RequestedRange::Malformed;
        };
        end.min(length - 1)
    };
    if start > end {
        RequestedRange::Unsatisfiable
    } else {
        RequestedRange::Partial { start, end }
    }
}

async fn serve_fixture(
    request: Request<RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    fixture: &Fixture,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let requested = requested_range(request.headers().get(header::RANGE), fixture.length);
    let (status, start, length, content_range) = match requested {
        RequestedRange::Full => (StatusCode::OK, 0, fixture.length, None),
        RequestedRange::Partial { start, end } => (
            StatusCode::PARTIAL_CONTENT,
            start,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{}", fixture.length)),
        ),
        RequestedRange::Unsatisfiable => {
            let response = Response::builder()
                .version(Version::HTTP_2)
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", fixture.length))
                .header(header::CONTENT_LENGTH, 0)
                .body(())?;
            respond.send_response(response, true)?;
            return Ok(());
        }
        RequestedRange::Malformed => {
            send_text(respond, StatusCode::BAD_REQUEST, "malformed Range\n").await?;
            return Ok(());
        }
    };
    let mut builder = Response::builder()
        .version(Version::HTTP_2)
        .status(status)
        .header(header::CONTENT_TYPE, fixture.content_type)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, fixture.etag.as_str());
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    let response = builder.body(())?;
    println!(
        "home response: path={} method={} type={} size={} bytes range_start={}",
        request.uri().path(),
        request.method(),
        fixture.content_type,
        length,
        start,
    );
    if request.method() == Method::HEAD || length == 0 {
        respond.send_response(response, true)?;
        return Ok(());
    }
    let mut stream = respond.send_response(response, false)?;
    stream_fixture(
        &mut stream,
        &fixture.path,
        start,
        length,
        fixture.content_type,
    )
    .await
}

async fn stream_fixture(
    stream: &mut h2::SendStream<Bytes>,
    path: &Path,
    start: u64,
    length: u64,
    content_type: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut file = File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(start)).await?;
    let mut remaining = length;
    let mut chunks = 0_u64;
    let mut sent = 0_u64;
    while remaining > 0 {
        stream.reserve_capacity(FILE_CHUNK_SIZE.min(remaining as usize));
        let capacity = poll_fn(|context| stream.poll_capacity(context))
            .await
            .ok_or("fixture stream closed")??;
        let read_size = capacity.min(FILE_CHUNK_SIZE).min(remaining as usize);
        let mut buffer = vec![0_u8; read_size];
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            return Err("fixture ended before its advertised length".into());
        }
        remaining -= count as u64;
        sent += count as u64;
        chunks += 1;
        stream.send_data(Bytes::from(buffer[..count].to_vec()), remaining == 0)?;
        if chunks % TRANSFER_LOG_INTERVAL == 0 || remaining == 0 {
            println!(
                "home upload: type={} message=HTTP/2 DATA chunk={} size={} bytes total={}/{} bytes",
                content_type,
                chunks,
                count,
                sent,
                length,
            );
        }
    }
    Ok(())
}

struct TemporaryUpload {
    path: PathBuf,
    committed: bool,
}

impl Drop for TemporaryUpload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn upload(
    request: Request<RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
    directory: &Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let expected_length = match request.headers().get(header::CONTENT_LENGTH) {
        Some(value) => match value.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
            Some(length) if length <= MAX_UPLOAD_SIZE => Some(length),
            Some(_) => {
                send_text(respond, StatusCode::PAYLOAD_TOO_LARGE, "upload exceeds 256 MiB\n").await?;
                return Ok(());
            }
            None => {
                send_text(respond, StatusCode::BAD_REQUEST, "invalid Content-Length\n").await?;
                return Ok(());
            }
        },
        None => None,
    };
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(".upload-{}-{counter}.part", std::process::id()));
    let mut temporary = TemporaryUpload {
        path: path.clone(),
        committed: false,
    };
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
    {
        Ok(file) => file,
        Err(error) => {
            send_text(respond, StatusCode::INTERNAL_SERVER_ERROR, "upload staging failed\n").await?;
            return Err(error.into());
        }
    };
    let mut body = request.into_body();
    let mut digest = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = body.data().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                send_text(respond, StatusCode::BAD_REQUEST, "upload body failed\n").await?;
                return Err(error.into());
            }
        };
        size = match size.checked_add(chunk.len() as u64) {
            Some(size) if size <= MAX_UPLOAD_SIZE => size,
            _ => {
                send_text(respond, StatusCode::PAYLOAD_TOO_LARGE, "upload exceeds 256 MiB\n").await?;
                return Ok(());
            }
        };
        file.write_all(&chunk).await?;
        body.flow_control().release_capacity(chunk.len())?;
        digest.update(&chunk);
    }
    if expected_length.is_some_and(|expected| expected != size) {
        send_text(respond, StatusCode::BAD_REQUEST, "Content-Length did not match body\n").await?;
        return Ok(());
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);
    let digest = format!("{:x}", digest.finalize());
    let destination = directory.join(&digest);
    if fs::metadata(&destination).await.is_err() {
        fs::rename(&path, &destination).await?;
    } else {
        fs::remove_file(&path).await?;
    }
    temporary.committed = true;
    let response_body = serde_json::to_vec(&json!({ "sha256": digest, "size": size }))?;
    let response = Response::builder()
        .version(Version::HTTP_2)
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::CONTENT_LENGTH, response_body.len())
        .body(())?;
    let mut response_stream = respond.send_response(response, false)?;
    response_stream.send_data(Bytes::from(response_body), true)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_cover_complete_open_ended_and_suffix_forms() {
        assert!(matches!(requested_range(None, 100), RequestedRange::Full));
        assert!(matches!(
            requested_range(Some(&"bytes=0-9".parse().unwrap()), 100),
            RequestedRange::Partial { start: 0, end: 9 }
        ));
        assert!(matches!(
            requested_range(Some(&"bytes=90-".parse().unwrap()), 100),
            RequestedRange::Partial { start: 90, end: 99 }
        ));
        assert!(matches!(
            requested_range(Some(&"bytes=-10".parse().unwrap()), 100),
            RequestedRange::Partial { start: 90, end: 99 }
        ));
    }

    #[test]
    fn ranges_reject_multiple_and_unsatisfiable_requests() {
        assert!(matches!(
            requested_range(Some(&"bytes=0-1,2-3".parse().unwrap()), 100),
            RequestedRange::Malformed
        ));
        assert!(matches!(
            requested_range(Some(&"bytes=100-".parse().unwrap()), 100),
            RequestedRange::Unsatisfiable
        ));
    }
}
