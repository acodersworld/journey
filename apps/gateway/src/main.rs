use std::{
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderValue, Method, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use bytes::Bytes;
use futures_util::{stream::Stream, StreamExt};
use journey_websocket::{
    accept_websocket_with_authorizer, connect_client, ClientSession, Config, Error,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{Mutex, Notify},
    time::timeout,
};

const MAX_UPLOAD_SIZE: u64 = 256 * 1024 * 1024;
const MAX_SMALL_RESPONSE_SIZE: usize = 64 * 1024;
const HOME_WAIT_TIMEOUT: Duration = Duration::from_secs(5);
const INNER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Journey real-world validation</title>
  <style>
    :root { color-scheme: light dark; font: 16px system-ui, sans-serif; }
    body { max-width: 52rem; margin: 2rem auto; padding: 0 1rem; }
    img, video { display: block; max-width: 100%; margin: 1rem 0; }
    video { background: #111; }
    button { margin-left: .5rem; }
    #status { white-space: pre-wrap; min-height: 3rem; }
  </style>
</head>
<body>
  <h1>Journey real-world validation</h1>
  <p>This page exercises the gateway, home agent, and streaming media path.</p>
  <h2>Image</h2>
  <img src="/media/image.jpg" alt="Validation fixture">
  <h2>Video</h2>
  <video controls preload="metadata" src="/media/video.mp4"></video>
  <h2>Raw upload</h2>
  <input id="file" type="file">
  <button id="upload" type="button">Upload</button>
  <p id="status" role="status"></p>
  <script>
    const fileInput = document.querySelector('#file');
    const button = document.querySelector('#upload');
    const status = document.querySelector('#status');
    const digest = async file => {
      const hash = await crypto.subtle.digest('SHA-256', await file.arrayBuffer());
      return [...new Uint8Array(hash)].map(byte => byte.toString(16).padStart(2, '0')).join('');
    };
    button.addEventListener('click', async () => {
      const file = fileInput.files[0];
      if (!file) { status.textContent = 'Choose a file first.'; return; }
      button.disabled = true;
      status.textContent = `Uploading ${file.size} bytes…`;
      try {
        const response = await fetch('/api/uploads', {
          method: 'POST',
          headers: { 'Content-Type': 'application/octet-stream' },
          body: file
        });
        const text = await response.text();
        if (!response.ok) throw new Error(`${response.status}: ${text}`);
        const result = JSON.parse(text);
        status.textContent = `Stored ${result.size} bytes\nSHA-256: ${result.sha256}`;
      } catch (error) {
        status.textContent = `Upload failed: ${error}`;
      } finally {
        button.disabled = false;
      }
    });
  </script>
</body>
</html>
"#;

#[derive(Clone)]
struct Credential {
    expected_basic: Vec<u8>,
}

impl Credential {
    fn from_environment(username: &str, password: &str) -> Result<Self, std::io::Error> {
        if username.contains(':') {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Basic Authentication usernames must not contain ':'",
            ));
        }
        Ok(Self {
            expected_basic: format!("{username}:{password}").into_bytes(),
        })
    }

    fn accepts(&self, value: Option<&HeaderValue>) -> bool {
        let Some(value) = value.and_then(|value| value.to_str().ok()) else {
            return false;
        };
        let Some((scheme, encoded)) = value.split_once(char::is_whitespace) else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("Basic") {
            return false;
        }
        let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
            return false;
        };
        self.expected_basic.ct_eq(&decoded).into()
    }
}

#[derive(Clone)]
struct GatewayAuth {
    site: Credential,
    home: Credential,
}

impl GatewayAuth {
    fn from_environment() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let site_username = std::env::var("JOURNEY_SITE_USERNAME")?;
        let site_password = std::env::var("JOURNEY_SITE_PASSWORD")?;
        let home_username = std::env::var("JOURNEY_HOME_USERNAME")?;
        let home_password = std::env::var("JOURNEY_HOME_PASSWORD")?;
        Ok(Self {
            site: Credential::from_environment(&site_username, &site_password)?,
            home: Credential::from_environment(&home_username, &home_password)?,
        })
    }
}

struct ConnectedSession {
    id: u64,
    session: ClientSession,
}

struct HomeConnection {
    next_id: AtomicU64,
    session: Mutex<Option<ConnectedSession>>,
    changed: Notify,
}

impl HomeConnection {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
            changed: Notify::new(),
        }
    }

    async fn replace(&self, session: ClientSession) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        *self.session.lock().await = Some(ConnectedSession { id, session });
        self.changed.notify_waiters();
    }

    async fn current(&self) -> Option<ConnectedSession> {
        self.session.lock().await.as_ref().map(|connected| ConnectedSession {
            id: connected.id,
            session: connected.session.clone(),
        })
    }

    async fn clear_if(&self, id: u64) {
        let mut session = self.session.lock().await;
        if session.as_ref().is_some_and(|connected| connected.id == id) {
            *session = None;
            self.changed.notify_waiters();
        }
    }
}

#[derive(Clone)]
struct GatewayState {
    home: Arc<HomeConnection>,
    auth: Arc<GatewayAuth>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind_address = std::env::var("GATEWAY_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let websocket_bind_address = std::env::var("GATEWAY_WEBSOCKET_BIND")
        .unwrap_or_else(|_| "0.0.0.0:9000".to_owned());
    let auth = Arc::new(GatewayAuth::from_environment()?);
    let home = Arc::new(HomeConnection::new());
    let websocket_listener = TcpListener::bind(&websocket_bind_address).await?;
    let websocket_home = Arc::clone(&home);
    let websocket_auth = Arc::clone(&auth);
    tokio::spawn(async move {
        accept_reconnections(websocket_listener, websocket_home, websocket_auth).await;
    });

    let state = GatewayState { home, auth };
    let app = Router::new()
        .route("/", get(index))
        .route("/media/image.jpg", get(image))
        .route("/media/video.mp4", get(video))
        .route("/api/uploads", post(upload))
        .route("/health", get(health))
        .fallback(not_found)
        .with_state(state);
    let listener = TcpListener::bind(&bind_address).await?;

    println!("gateway HTTP listener on {bind_address}");
    println!("gateway WebSocket listener on {websocket_bind_address}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"journey-validation\"")],
        "unauthorized\n",
    )
        .into_response()
}

fn require_site(request: &Request<Body>, state: &GatewayState) -> Result<(), Response> {
    if state
        .auth
        .site
        .accepts(request.headers().get(header::AUTHORIZATION))
    {
        Ok(())
    } else {
        Err(unauthorized())
    }
}

fn reject_home() -> journey_websocket::tungstenite::handshake::server::ErrorResponse {
    http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::WWW_AUTHENTICATE, "Basic realm=\"journey-home\"")
        .body(Some("unauthorized\n".to_owned()))
        .expect("valid WebSocket rejection response")
}

fn authorize_home(
    request: &journey_websocket::tungstenite::handshake::server::Request,
    credential: &Credential,
) -> Result<(), journey_websocket::tungstenite::handshake::server::ErrorResponse> {
    if credential.accepts(request.headers().get(header::AUTHORIZATION)) {
        Ok(())
    } else {
        Err(reject_home())
    }
}

async fn index(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    if let Err(response) = require_site(&request, &state) {
        return response;
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::CONTENT_LENGTH, INDEX_HTML.len())
        .body(Body::from(INDEX_HTML))
        .expect("valid static page response")
}

async fn not_found(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    if let Err(response) = require_site(&request, &state) {
        return response;
    }
    (StatusCode::NOT_FOUND, "not found\n").into_response()
}

async fn image(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    media(State(state), request, "/fixtures/image.jpg", "image/jpeg").await
}

async fn video(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    media(State(state), request, "/fixtures/video.mp4", "video/mp4").await
}

async fn media(
    State(state): State<GatewayState>,
    request: Request<Body>,
    path: &str,
    fallback_content_type: &str,
) -> Response {
    if let Err(response) = require_site(&request, &state) {
        return response;
    }
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n").into_response();
    }
    let range = request.headers().get(header::RANGE).cloned();
    let (id, response_future, _request_body) = match send_inner_request(
        &state,
        method.clone(),
        path,
        range.as_ref(),
        None,
        true,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return home_failure(&state, error).await,
    };
    let response = match timeout(INNER_RESPONSE_TIMEOUT, response_future).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            state.home.clear_if(id).await;
            return home_failure(&state, error.to_string()).await;
        }
        Err(_) => {
            state.home.clear_if(id).await;
            return (StatusCode::GATEWAY_TIMEOUT, "home response timed out\n").into_response();
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body();
    let mut builder = Response::builder().status(status);
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        header::ACCEPT_RANGES,
        header::CONTENT_RANGE,
        header::ETAG,
    ] {
        if let Some(value) = headers.get(&name) {
            builder = builder.header(name, value.clone());
        }
    }
    if headers.get(header::CONTENT_TYPE).is_none() {
        builder = builder.header(header::CONTENT_TYPE, fallback_content_type);
    }
    if method == Method::HEAD {
        return builder
            .body(Body::empty())
            .expect("valid HEAD response");
    }
    builder
        .body(Body::from_stream(stream_body(body)))
        .expect("valid media response")
}

async fn health(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    if let Err(response) = require_site(&request, &state) {
        return response;
    }
    let (id, response_future, _request_body) = match send_inner_request(
        &state,
        Method::GET,
        "/health",
        None,
        None,
        true,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return home_failure(&state, error).await,
    };
    let response = match timeout(INNER_RESPONSE_TIMEOUT, response_future).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            state.home.clear_if(id).await;
            return home_failure(&state, error.to_string()).await;
        }
        Err(_) => {
            state.home.clear_if(id).await;
            return (StatusCode::GATEWAY_TIMEOUT, "home response timed out\n").into_response();
        }
    };
    let status = response.status();
    let body = match collect_body(response.into_body(), MAX_SMALL_RESPONSE_SIZE).await {
        Ok(body) => body,
        Err(error) => return home_failure(&state, error).await,
    };
    if status == StatusCode::OK && body.as_slice() == b"healthy\n" {
        (StatusCode::OK, "ok\n").into_response()
    } else {
        (StatusCode::BAD_GATEWAY, "home health check failed\n").into_response()
    }
}

async fn upload(State(state): State<GatewayState>, request: Request<Body>) -> Response {
    if let Err(response) = require_site(&request, &state) {
        return response;
    }
    let content_length = match request.headers().get(header::CONTENT_LENGTH) {
        Some(value) => match value.to_str().ok().and_then(|value| value.parse::<u64>().ok()) {
            Some(length) if length <= MAX_UPLOAD_SIZE => Some(length),
            Some(_) => return (StatusCode::PAYLOAD_TOO_LARGE, "upload exceeds 256 MiB\n").into_response(),
            None => return (StatusCode::BAD_REQUEST, "invalid Content-Length\n").into_response(),
        },
        None => None,
    };
    let (id, response_future, mut send_stream) = match send_inner_request(
        &state,
        Method::PUT,
        "/uploads",
        None,
        content_length,
        false,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return home_failure(&state, error).await,
    };
    let mut body = request.into_body().into_data_stream();
    let mut observed = 0_u64;
    let mut capacity = match reserve_upload_capacity(&mut send_stream).await {
        Ok(capacity) => capacity,
        Err(error) => {
            state.home.clear_if(id).await;
            return home_failure(&state, error.to_string()).await;
        }
    };
    while let Some(result) = body.next().await {
        let chunk = match result {
            Ok(chunk) => chunk,
            Err(_error) => {
                send_stream.send_reset(h2::Reason::CANCEL);
                return (StatusCode::BAD_REQUEST, "upload body disconnected\n").into_response();
            }
        };
        observed = match observed.checked_add(chunk.len() as u64) {
            Some(value) => value,
            None => MAX_UPLOAD_SIZE + 1,
        };
        if observed > MAX_UPLOAD_SIZE {
            send_stream.send_reset(h2::Reason::CANCEL);
            return (StatusCode::PAYLOAD_TOO_LARGE, "upload exceeds 256 MiB\n").into_response();
        }
        if let Err(error) = send_chunk(&mut send_stream, chunk, &mut capacity).await {
            state.home.clear_if(id).await;
            return home_failure(&state, error.to_string()).await;
        }
        if capacity == 0 {
            capacity = match reserve_upload_capacity(&mut send_stream).await {
                Ok(capacity) => capacity,
                Err(error) => {
                    state.home.clear_if(id).await;
                    return home_failure(&state, error.to_string()).await;
                }
            };
        }
    }
    if let Some(expected) = content_length {
        if expected != observed {
            send_stream.send_reset(h2::Reason::CANCEL);
            return (StatusCode::BAD_REQUEST, "Content-Length did not match body\n").into_response();
        }
    }
    if let Err(error) = send_stream.send_data(Bytes::new(), true) {
        state.home.clear_if(id).await;
        return home_failure(&state, error.to_string()).await;
    }
    let response = match timeout(INNER_RESPONSE_TIMEOUT, response_future).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            state.home.clear_if(id).await;
            return home_failure(&state, error.to_string()).await;
        }
        Err(_) => {
            state.home.clear_if(id).await;
            return (StatusCode::GATEWAY_TIMEOUT, "home response timed out\n").into_response();
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let body = match collect_body(response.into_body(), MAX_SMALL_RESPONSE_SIZE).await {
        Ok(body) => body,
        Err(error) => return home_failure(&state, error).await,
    };
    let mut builder = Response::builder().status(status);
    if let Some(value) = headers.get(header::CONTENT_TYPE) {
        builder = builder.header(header::CONTENT_TYPE, value.clone());
    }
    builder
        .header(header::CONTENT_LENGTH, body.len())
        .body(Body::from(body))
        .expect("valid upload response")
}

async fn send_inner_request(
    state: &GatewayState,
    method: Method,
    path: &str,
    range: Option<&HeaderValue>,
    content_length: Option<u64>,
    end_of_stream: bool,
) -> Result<(u64, h2::client::ResponseFuture, h2::SendStream<Bytes>), String> {
    let connected = wait_for_home(&state.home).await?;
    let sender = connected.session.sender();
    let mut request = http::Request::builder()
        .version(http::Version::HTTP_2)
        .method(method)
        .uri(format!("https://home.internal{path}"))
        .header(header::HOST, "home.internal")
        .body(())
        .map_err(|error| error.to_string())?;
    if let Some(range) = range {
        request.headers_mut().insert(header::RANGE, range.clone());
    }
    if let Some(content_length) = content_length {
        request.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&content_length.to_string()).map_err(|error| error.to_string())?,
        );
    }
    if !end_of_stream {
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    let (response, body) = match sender.send_request(request, end_of_stream).await {
        Ok(result) => result,
        Err(error) => {
            state.home.clear_if(connected.id).await;
            return Err(error.to_string());
        }
    };
    Ok((connected.id, response, body))
}

async fn wait_for_home(home: &HomeConnection) -> Result<ConnectedSession, String> {
    loop {
        if let Some(session) = home.current().await {
            return Ok(session);
        }
        let notified = home.changed.notified();
        if timeout(HOME_WAIT_TIMEOUT, notified).await.is_err() {
            return Err("home connection is unavailable".to_owned());
        }
    }
}

async fn home_failure(_state: &GatewayState, error: String) -> Response {
    eprintln!("gateway home request failed: {error}");
    (StatusCode::BAD_GATEWAY, "home connection failed\n").into_response()
}

async fn reserve_upload_capacity(send: &mut h2::SendStream<Bytes>) -> Result<usize, Error> {
    send.reserve_capacity(64 * 1024);
    let capacity = futures_util::future::poll_fn(|context| send.poll_capacity(context))
        .await
        .ok_or_else(|| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "inner upload stream closed",
            ))
        })??;
    Ok(capacity)
}

async fn send_chunk(
    send: &mut h2::SendStream<Bytes>,
    mut chunk: Bytes,
    capacity: &mut usize,
) -> Result<(), Error> {
    while !chunk.is_empty() {
        if *capacity == 0 {
            *capacity = reserve_upload_capacity(send).await?;
        }
        let amount = (*capacity).min(chunk.len());
        send.send_data(chunk.split_to(amount), false)?;
        *capacity -= amount;
    }
    Ok(())
}

async fn collect_body(mut body: h2::RecvStream, limit: usize) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        if result.len().saturating_add(chunk.len()) > limit {
            return Err("home response exceeded the bounded response limit".to_owned());
        }
        body.flow_control()
            .release_capacity(chunk.len())
            .map_err(|error| error.to_string())?;
        result.extend_from_slice(&chunk);
    }
    Ok(result)
}

struct H2BodyStream {
    body: Option<h2::RecvStream>,
    pending_release: usize,
}

impl H2BodyStream {
    fn new(body: h2::RecvStream) -> Self {
        Self {
            body: Some(body),
            pending_release: 0,
        }
    }
}

impl Stream for H2BodyStream {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        if this.pending_release != 0 {
            let pending_release = std::mem::take(&mut this.pending_release);
            let release_result = this
                .body
                .as_mut()
                .expect("body remains while capacity is pending")
                .flow_control()
                .release_capacity(pending_release);
            if let Err(error) = release_result {
                this.body = None;
                return Poll::Ready(Some(Err(std::io::Error::other(error.to_string()))));
            }
        }
        let result = match this.body.as_mut() {
            Some(body) => body.poll_data(context),
            None => return Poll::Ready(None),
        };
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(chunk))) => {
                this.pending_release = chunk.len();
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.body = None;
                Poll::Ready(Some(Err(std::io::Error::other(error.to_string()))))
            }
            Poll::Ready(None) => {
                this.body = None;
                Poll::Ready(None)
            }
        }
    }
}

fn stream_body(body: h2::RecvStream) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    H2BodyStream::new(body)
}

async fn accept_reconnections(
    listener: TcpListener,
    home: Arc<HomeConnection>,
    auth: Arc<GatewayAuth>,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("gateway WebSocket accept failed: {error}");
                continue;
            }
        };
        let credential = auth.home.clone();
        match accept_websocket_with_authorizer(stream, Config::default(), move |request| {
            authorize_home(request, &credential)
        })
        .await
        {
            Ok(websocket) => match connect_client(websocket, Config::default()).await {
                Ok(session) => {
                    home.replace(session).await;
                    println!("home WebSocket connection established");
                }
                Err(error) => eprintln!("home HTTP/2 connection from {peer} failed: {error}"),
            },
            Err(error) => eprintln!("home WebSocket handshake from {peer} failed: {error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_credentials_are_compared_as_one_constant_time_value() {
        let credential = Credential::from_environment("site", "secret").expect("credential");
        let value = format!("Basic {}", STANDARD.encode("site:secret"));
        let header = HeaderValue::from_str(&value).expect("header");
        assert!(credential.accepts(Some(&header)));
        let wrong = HeaderValue::from_static("Basic c2l0ZTp3cm9uZw==");
        assert!(!credential.accepts(Some(&wrong)));
    }

    #[test]
    fn static_page_uses_a_raw_upload_body() {
        assert!(INDEX_HTML.contains("body: file"));
        assert!(!INDEX_HTML.contains("FormData"));
    }
}
