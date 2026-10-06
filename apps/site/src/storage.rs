use std::{
    error::Error,
    future::Future,
    future::poll_fn,
    path::Path,
    pin::Pin,
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use bytes::Bytes;
use futures_util::{stream, Stream, StreamExt};
use h2::client::{self, SendRequest};
use http::{header, Method, Request, StatusCode, Version};
use journey_websocket::{connect_client, ClientSender, Config};
use tokio::{
    fs::File,
    io::AsyncReadExt,
    net::TcpStream,
    sync::Mutex,
    time::timeout,
};

const STORAGE_CHUNK_SIZE: usize = 64 * 1024 * 1024;
const MEDIA_KEY_PREFIX: &str = "media/";

pub type StorageBody = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;
pub const UPLOAD_LIMIT_ERROR: &str = "media file exceeds configured size limit";

pub struct StorageResponse {
    pub status: StatusCode,
    pub headers: http::HeaderMap,
    pub body: StorageBody,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageReductionOptions {
    max_edge: u32,
    max_bytes: Option<u64>,
}

impl ImageReductionOptions {
    pub fn new(
        max_edge: u32,
        max_bytes: Option<u64>,
    ) -> Result<Self, String> {
        if !(1..=2_048).contains(&max_edge) {
            return Err("Image dimensions must be between 1 and 2048 pixels".to_owned());
        }
        if max_bytes == Some(0) {
            return Err("Image byte limit must be positive".to_owned());
        }
        Ok(Self { max_edge, max_bytes })
    }
}

pub trait StorageClient: Clone + Send + Sync + 'static {
    fn put_file(
        &self,
        content_type: &str,
        path: &Path,
    ) -> impl Future<Output = Result<String, String>> + Send;

    fn put_stream(
        &self,
        content_type: &str,
        content_length: Option<u64>,
        body: StorageBody,
        max_bytes: u64,
    ) -> impl Future<Output = Result<(String, u64), String>> + Send;

    fn get(
        &self,
        key: &str,
        range: Option<&str>,
        head: bool,
        thumbnail: bool,
    ) -> impl Future<Output = Result<StorageResponse, String>> + Send;

    fn get_reduced_image(
        &self,
        key: &str,
        options: ImageReductionOptions,
        video_thumbnail: bool,
        head: bool,
    ) -> impl Future<Output = Result<StorageResponse, String>> + Send {
        async move {
            let _ = (key, options, video_thumbnail, head);
            Err("Reduced image requests are unavailable for this storage client".to_owned())
        }
    }
}

#[derive(Clone)]
pub struct H2cStorageClient {
    address: SocketAddr,
    initial_window_size: u32,
    initial_connection_window_size: u32,
    connection: Arc<Mutex<ConnectionState>>,
}

#[derive(Clone)]
pub struct WebSocketStorageClient {
    connection: Arc<Mutex<WebSocketConnectionState>>,
    initial_window_size: u32,
    initial_connection_window_size: u32,
}

#[derive(Clone)]
pub enum SiteStorageClient {
    H2c(H2cStorageClient),
    WebSocket(WebSocketStorageClient),
}

struct WebSocketConnectionState {
    generation: u64,
    sender: Option<ClientSender>,
}

struct ConnectionState {
    generation: u64,
    sender: Option<SendRequest<Bytes>>,
    driver: Option<tokio::task::JoinHandle<()>>,
}

impl H2cStorageClient {
    pub async fn connect(
        address: SocketAddr,
        initial_window_size: u32,
        initial_connection_window_size: u32,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if !address.ip().is_loopback() {
            return Err(format!("storage h2c address must be loopback: {address}").into());
        }
        let client = Self {
            address,
            initial_window_size,
            initial_connection_window_size,
            connection: Arc::new(Mutex::new(ConnectionState {
                generation: 0,
                sender: None,
                driver: None,
            })),
        };
        let mut state = client.connection.lock().await;
        client
            .connect_sender(&mut state)
            .await
            .map_err(std::io::Error::other)?;
        drop(state);
        Ok(client)
    }

    async fn connect_sender(&self, state: &mut ConnectionState) -> Result<(), String> {
        if let Some(driver) = state.driver.take() {
            driver.abort();
        }
        let mut builder = client::Builder::new();
        builder
            .initial_window_size(self.initial_window_size)
            .initial_connection_window_size(self.initial_connection_window_size);

        let stream = TcpStream::connect(self.address)
            .await
            .map_err(|error| error.to_string())?;
        let (sender, connection) = builder.handshake(stream)
            .await
            .map_err(|error| error.to_string())?;
        let generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| "storage h2c connection generation overflow".to_owned())?;
        state.generation = generation;
        state.sender = Some(sender);
        let shared_connection = Arc::clone(&self.connection);
        let driver = tokio::spawn(async move {
            if let Err(error) = connection.await {
                eprintln!("storage h2c connection ended: {error}");
            }
            let mut state = shared_connection.lock().await;
            if state.generation == generation {
                state.sender = None;
            }
        });
        state.driver = Some(driver);
        Ok(())
    }

    pub async fn close(&self) {
        let driver = {
            let mut state = self.connection.lock().await;
            state.sender = None;
            state.driver.take()
        };
        if let Some(driver) = driver {
            driver.abort();
            let _ = driver.await;
        }
    }

    async fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), String> {
        let mut state = self.connection.lock().await;
        let sender_was_present = state.sender.is_some();
        if state.sender.is_none() {
            self.connect_sender(&mut state).await?;
        }

        let readiness = {
            let sender = state.sender.as_mut().unwrap();
            poll_fn(|context| sender.poll_ready(context)).await
        };
        if let Err(error) = readiness {
            let error = error.to_string();
            state.sender = None;
            if !sender_was_present {
                return Err(error);
            }
            self.connect_sender(&mut state).await?;
            let readiness = {
                let sender = state.sender.as_mut().unwrap();
                poll_fn(|context| sender.poll_ready(context)).await
            };
            if let Err(error) = readiness {
                state.sender = None;
                return Err(error.to_string());
            }
        }

        match state
            .sender
            .as_mut()
            .unwrap()
            .send_request(request, end_of_stream)
        {
            Ok(result) => Ok(result),
            Err(error) => {
                state.sender = None;
                Err(error.to_string())
            }
        }
    }
}

impl WebSocketStorageClient {
    pub fn new(initial_window_size: u32, initial_connection_window_size: u32) -> Self {
        Self {
            connection: Arc::new(Mutex::new(WebSocketConnectionState {
                generation: 0,
                sender: None,
            })),
            initial_window_size,
            initial_connection_window_size,
        }
    }

    pub async fn serve_connection<S>(
        &self,
        websocket: tokio_tungstenite::WebSocketStream<S>,
        mut shutdown: crate::shutdown::Receiver,
    ) -> Result<(), String>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let config = Config {
            h2_initial_stream_window_size: self.initial_window_size,
            h2_initial_connection_window_size: self.initial_connection_window_size,
            ..Config::default()
        };
        let session = tokio::select! {
            result = connect_client(websocket, config) => result.map_err(|error| error.to_string())?,
            signal = crate::shutdown::requested(&mut shutdown) => return signal,
        };
        let _close_guard = ClientSessionCloseGuard(session.clone());
        let generation = {
            let mut state = self.connection.lock().await;
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or_else(|| "storage WebSocket generation overflow".to_owned())?;
            state.sender = Some(session.sender());
            state.generation
        };
        println!("site storage WebSocket session connected");
        let result = tokio::select! {
            result = session.wait() => result.map_err(|error| error.to_string()),
            signal = crate::shutdown::requested(&mut shutdown) => {
                session.close();
                match session.wait().await {
                    Ok(()) => signal,
                    Err(error) => Err(error.to_string()),
                }
            }
        };
        let mut state = self.connection.lock().await;
        if state.generation == generation {
            state.sender = None;
        }
        println!("site storage WebSocket session ended");
        result
    }

    async fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), String> {
        let sender = self
            .connection
            .lock()
            .await
            .sender
            .clone()
            .ok_or_else(|| "storage WebSocket is unavailable".to_owned())?;
        let mut ready = timeout(Duration::from_secs(3), sender.ready())
            .await
            .map_err(|_| "storage WebSocket request readiness timed out".to_owned())?
            .map_err(|error| error.to_string())?;
        ready
            .send_request(request, end_of_stream)
            .map_err(|error| error.to_string())
    }
}

impl SiteStorageClient {
    pub async fn close(&self) {
        if let Self::H2c(client) = self {
            client.close().await;
        }
    }
}

struct ClientSessionCloseGuard(journey_websocket::ClientSession);

impl Drop for ClientSessionCloseGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

trait StorageRequestTransport: Clone + Send + Sync + 'static {
    fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> impl Future<Output = Result<(client::ResponseFuture, h2::SendStream<Bytes>), String>> + Send;
}

impl StorageRequestTransport for H2cStorageClient {
    async fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), String> {
        H2cStorageClient::start_request(self, request, end_of_stream).await
    }
}

impl StorageRequestTransport for WebSocketStorageClient {
    async fn start_request(
        &self,
        request: Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), String> {
        WebSocketStorageClient::start_request(self, request, end_of_stream).await
    }
}

macro_rules! impl_storage_client {
    ($client:ty) => {
        impl StorageClient for $client {
            async fn put_file(&self, content_type: &str, path: &Path) -> Result<String, String> {
                put_file(self, content_type, path).await
            }

            async fn put_stream(
                &self,
                content_type: &str,
                content_length: Option<u64>,
                body: StorageBody,
                max_bytes: u64,
            ) -> Result<(String, u64), String> {
                put_stream(self, content_type, content_length, body, max_bytes).await
            }

            async fn get(
                &self,
                key: &str,
                range: Option<&str>,
                head: bool,
                thumbnail: bool,
            ) -> Result<StorageResponse, String> {
                get(self, key, range, head, thumbnail).await
            }

            async fn get_reduced_image(
                &self,
                key: &str,
                options: ImageReductionOptions,
                video_thumbnail: bool,
                head: bool,
            ) -> Result<StorageResponse, String> {
                get_reduced_image(self, key, options, video_thumbnail, head).await
            }
        }
    };
}

impl_storage_client!(H2cStorageClient);
impl_storage_client!(WebSocketStorageClient);

impl StorageClient for SiteStorageClient {
    async fn put_file(&self, content_type: &str, path: &Path) -> Result<String, String> {
        match self {
            Self::H2c(client) => client.put_file(content_type, path).await,
            Self::WebSocket(client) => client.put_file(content_type, path).await,
        }
    }

    async fn put_stream(
        &self,
        content_type: &str,
        content_length: Option<u64>,
        body: StorageBody,
        max_bytes: u64,
    ) -> Result<(String, u64), String> {
        match self {
            Self::H2c(client) => client.put_stream(content_type, content_length, body, max_bytes).await,
            Self::WebSocket(client) => client.put_stream(content_type, content_length, body, max_bytes).await,
        }
    }

    async fn get(
        &self,
        key: &str,
        range: Option<&str>,
        head: bool,
        thumbnail: bool,
    ) -> Result<StorageResponse, String> {
        match self {
            Self::H2c(client) => client.get(key, range, head, thumbnail).await,
            Self::WebSocket(client) => client.get(key, range, head, thumbnail).await,
        }
    }

    async fn get_reduced_image(
        &self,
        key: &str,
        options: ImageReductionOptions,
        video_thumbnail: bool,
        head: bool,
    ) -> Result<StorageResponse, String> {
        match self {
            Self::H2c(client) => get_reduced_image(client, key, options, video_thumbnail, head).await,
            Self::WebSocket(client) => get_reduced_image(client, key, options, video_thumbnail, head).await,
        }
    }
}

async fn put_file<T: StorageRequestTransport>(
    transport: &T,
    content_type: &str,
    path: &Path,
) -> Result<String, String> {
    let mut file = File::open(path).await.map_err(|error| error.to_string())?;
    let length = file
        .metadata()
        .await
        .map_err(|error| error.to_string())?
        .len();
    let request = storage_request(
        Method::PUT,
        MEDIA_KEY_PREFIX,
        None,
        Some(content_type),
        Some(length),
        false,
        true,
        None,
    )?;
    let (response, mut send) = transport.start_request(request, false).await?;
    let upload_result = async {
        let mut sent = 0_u64;
        let mut buffer = vec![0_u8; STORAGE_CHUNK_SIZE];
        loop {
            let count = file.read(&mut buffer).await.map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            let next_sent = sent
                .checked_add(count as u64)
                .ok_or_else(|| "media file size overflow".to_owned())?;
            if next_sent > length {
                return Err(format!("media file changed while uploading: {}", path.display()));
            }
            send_data(&mut send, Bytes::copy_from_slice(&buffer[..count])).await?;
            sent = next_sent;
        }
        if sent != length {
            return Err(format!("media file changed while uploading: {}", path.display()));
        }
        send.send_data(Bytes::new(), true)
            .map_err(|error| error.to_string())?;
        Ok::<(), String>(())
    }
    .await;

    if let Err(upload_error) = upload_result {
        drop(send);
        if let Ok(response) = response.await {
            return Err(format!(
                "storage upload failed with HTTP {} while sending media",
                response.status()
            ));
        }
        return Err(upload_error);
    }

    let response = response.await.map_err(|error| error.to_string())?;
    if response.status() != StatusCode::OK {
        return Err(format!("storage upload failed with HTTP {}", response.status()));
    }
    extract_generated_media_key(response.headers())
}

async fn put_stream<T: StorageRequestTransport>(
    transport: &T,
    content_type: &str,
    content_length: Option<u64>,
    mut body: StorageBody,
    max_bytes: u64,
) -> Result<(String, u64), String> {
    let request = storage_request(
        Method::PUT,
        MEDIA_KEY_PREFIX,
        None,
        Some(content_type),
        content_length,
        false,
        true,
        None,
    )?;
    let (response, mut send) = transport.start_request(request, false).await?;
    let mut sent = 0_u64;
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            format!("incoming upload request body failed after {sent} bytes: {error}")
        })?;
        let next_sent = sent
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| UPLOAD_LIMIT_ERROR.to_owned())?;
        if next_sent > max_bytes {
            return Err(UPLOAD_LIMIT_ERROR.to_owned());
        }
        send_data(&mut send, chunk)
            .await
            .map_err(|error| format!("forwarding upload body to storage failed after {sent} bytes: {error}"))?;
        sent = next_sent;
    }
    if content_length.is_some_and(|length| length != sent) {
        return Err("media upload length did not match Content-Length".to_owned());
    }
    send.send_data(Bytes::new(), true)
        .map_err(|error| error.to_string())?;
    let response = response.await.map_err(|error| error.to_string())?;
    if response.status() != StatusCode::OK {
        return Err(format!("storage upload failed with HTTP {}", response.status()));
    }
    Ok((extract_generated_media_key(response.headers())?, sent))
}

async fn get<T: StorageRequestTransport>(
    transport: &T,
    key: &str,
    range: Option<&str>,
    head: bool,
    thumbnail: bool,
) -> Result<StorageResponse, String> {
    let method = if head { Method::HEAD } else { Method::GET };
    let representation = thumbnail.then_some("thumbnail");
    let request = storage_request(method, key, range, None, None, false, false, representation)?;
    get_response(transport, request, head).await
}

async fn get_reduced_image<T: StorageRequestTransport>(
    transport: &T,
    key: &str,
    options: ImageReductionOptions,
    video_thumbnail: bool,
    head: bool,
) -> Result<StorageResponse, String> {
    let method = if head { Method::HEAD } else { Method::GET };
    let representation = if video_thumbnail { "thumbnail" } else { "reduced-image" };
    let mut request = storage_request(method, key, None, None, None, false, false, Some(representation))?;
    let headers = request.headers_mut();
    let edge = options.max_edge.to_string();
    let edge = edge.parse().map_err(|error| format!("invalid image edge header: {error}"))?;
    headers.insert("Object-Image-Max-Edge", edge);
    headers.insert(
        "Object-Image-Fit",
        "contain"
            .parse()
            .map_err(|error| format!("invalid image fit header: {error}"))?,
    );
    headers.insert(
        "Object-Image-Format",
        "jpeg"
            .parse()
            .map_err(|error| format!("invalid image format header: {error}"))?,
    );
    if let Some(max_bytes) = options.max_bytes {
        let value = max_bytes
            .to_string()
            .parse()
            .map_err(|error| format!("invalid image byte limit header: {error}"))?;
        headers.insert(
            "Object-Image-Max-Bytes",
            value,
        );
    }
    get_response(transport, request, head).await
}

async fn get_response<T: StorageRequestTransport>(
    transport: &T,
    request: Request<()>,
    head: bool,
) -> Result<StorageResponse, String> {
    let (response, _send) = transport.start_request(request, true).await?;
    let response = response.await.map_err(|error| error.to_string())?;
    let status = response.status();
    let headers = response.headers().clone();
    let body = if head {
        Box::pin(stream::empty()) as StorageBody
    } else {
        Box::pin(stream::unfold(response.into_body(), |mut body| async move {
            match body.data().await {
                Some(Ok(chunk)) => {
                    let length = chunk.len();
                    let result = body
                        .flow_control()
                        .release_capacity(length)
                        .map_err(|error| std::io::Error::other(error.to_string()))
                        .map(|()| chunk);
                    Some((result, body))
                }
                Some(Err(error)) => Some((Err(std::io::Error::other(error.to_string())), body)),
                None => None,
            }
        })) as StorageBody
    };
    Ok(StorageResponse { status, headers, body })
}

fn storage_request(
    method: Method,
    key: &str,
    range: Option<&str>,
    content_type: Option<&str>,
    content_length: Option<u64>,
    create_only: bool,
    generate_key: bool,
    representation: Option<&str>,
) -> Result<Request<()>, String> {
    let mut builder = Request::builder()
        .version(Version::HTTP_2)
        .method(method)
        .uri(format!("http://storage.internal/objects/{key}"));
    if let Some(range) = range {
        builder = builder.header(header::RANGE, range);
    }
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    if let Some(content_length) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, content_length);
    }
    if create_only {
        builder = builder.header(header::IF_NONE_MATCH, "*");
    }
    if generate_key {
        builder = builder.header("Object-Key-Mode", "sha256");
    }
    if let Some(representation) = representation {
        builder = builder.header("Object-Representation", representation);
    }
    builder.body(()).map_err(|error| error.to_string())
}

async fn send_data(send: &mut h2::SendStream<Bytes>, mut chunk: Bytes) -> Result<(), String> {
    while !chunk.is_empty() {
        send.reserve_capacity(chunk.len().min(STORAGE_CHUNK_SIZE));
        let capacity = poll_fn(|context| send.poll_capacity(context))
            .await
            .ok_or_else(|| "storage upload stream closed".to_owned())?
            .map_err(|error| error.to_string())?;
        if capacity == 0 {
            continue;
        }
        let amount = capacity.min(chunk.len());
        send.send_data(chunk.split_to(amount), false)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn extract_generated_media_key(headers: &http::HeaderMap) -> Result<String, String> {
    let mut values = headers.get_all("Object-Name").iter();
    let value = values
        .next()
        .ok_or_else(|| "storage upload response is missing Object-Name".to_owned())?;
    if values.next().is_some() {
        return Err("storage upload response has duplicate Object-Name headers".to_owned());
    }
    let name = value
        .to_str()
        .map_err(|_| "storage upload response has malformed Object-Name".to_owned())?;
    let digest = name;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(format!("storage upload response has invalid Object-Name: {name:?}"));
    }
    Ok(format!("{MEDIA_KEY_PREFIX}{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        net::TcpListener,
        sync::oneshot,
        task::JoinHandle,
    };

    #[tokio::test]
    async fn websocket_storage_fails_promptly_without_a_connected_peer() {
        let client = WebSocketStorageClient::new(512 * 1024, 4 * 1024 * 1024);
        let started = std::time::Instant::now();
        let error = client.get("media/missing", None, true, false).await.err().unwrap();

        assert_eq!(error, "storage WebSocket is unavailable");
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    struct ReconnectingStorageServer {
        client: H2cStorageClient,
        server: JoinHandle<usize>,
        close_initial: oneshot::Sender<()>,
        initial_closed: oneshot::Receiver<()>,
        restart: oneshot::Sender<()>,
        restarted: oneshot::Receiver<()>,
        finished: oneshot::Sender<()>,
    }

    async fn start_reconnecting_storage_server() -> ReconnectingStorageServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (close_initial, close_initial_rx) = oneshot::channel();
        let (initial_closed_tx, initial_closed) = oneshot::channel();
        let (restart, restart_rx) = oneshot::channel();
        let (restarted_tx, restarted) = oneshot::channel();
        let (finished, mut finished_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut accepted_connections = 1;
            let mut connection = h2::server::handshake(stream).await.unwrap();
            let mut close_initial_rx = close_initial_rx;
            loop {
                tokio::select! {
                    _ = &mut close_initial_rx => break,
                    _ = respond_to_next_request(&mut connection) => {}
                }
            }
            drop(connection);
            drop(listener);
            let _ = initial_closed_tx.send(());

            let _ = restart_rx.await;
            let listener = TcpListener::bind(address).await.unwrap();
            let _ = restarted_tx.send(());
            let (stream, _) = listener.accept().await.unwrap();
            accepted_connections += 1;
            let mut connection = h2::server::handshake(stream).await.unwrap();
            loop {
                tokio::select! {
                    _ = &mut finished_rx => break,
                    _ = respond_to_next_request(&mut connection) => {}
                }
            }
            drop(connection);
            accepted_connections
        });
        let client = H2cStorageClient::connect(address, 512 * 1024, 4 * 1024 * 1024)
            .await
            .unwrap();
        ReconnectingStorageServer {
            client,
            server,
            close_initial,
            initial_closed,
            restart,
            restarted,
            finished,
        }
    }

    async fn respond_to_next_request(
        connection: &mut h2::server::Connection<TcpStream, Bytes>,
    ) {
        let Some(Ok((_request, mut respond))) = connection.accept().await else {
            panic!("h2 client did not send a request");
        };
        let response = http::Response::builder()
            .status(StatusCode::OK)
            .body(())
            .unwrap();
        respond.send_response(response, true).unwrap();
    }

    async fn wait_until_disconnected(client: &H2cStorageClient) {
        for _ in 0..1_000 {
            if client.connection.lock().await.sender.is_none() {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("h2 connection driver did not clear its sender");
    }

    #[test]
    fn extract_generated_media_key_requires_exactly_one_valid_object_name() {
        let mut headers = http::HeaderMap::new();
        assert!(extract_generated_media_key(&headers).is_err());

        headers.append("Object-Name", "a".to_owned().parse().unwrap());
        headers.append("Object-Name", "b".to_owned().parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.clear();
        headers.insert("Object-Name", http::HeaderValue::from_bytes(b"\xff").unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "A".repeat(64).parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "a".repeat(63).parse().unwrap());
        assert!(extract_generated_media_key(&headers).is_err());

        headers.insert("Object-Name", "a".repeat(64).parse().unwrap());
        assert_eq!(
            extract_generated_media_key(&headers).unwrap(),
            format!("{MEDIA_KEY_PREFIX}{}", "a".repeat(64))
        );
    }

    #[tokio::test]
    async fn reconnects_after_storage_becomes_unavailable_and_restarts() {
        let server = start_reconnecting_storage_server().await;
        server.client.get("first", None, true, false).await.unwrap();
        server.close_initial.send(()).unwrap();
        server.initial_closed.await.unwrap();
        wait_until_disconnected(&server.client).await;

        assert!(server.client.get("while-down", None, true, false).await.is_err());
        server.restart.send(()).unwrap();
        server.restarted.await.unwrap();
        let response = server.client.get("after-restart", None, true, false).await.unwrap();
        assert_eq!(response.status, StatusCode::OK);

        server.finished.send(()).unwrap();
        assert_eq!(server.server.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_replacement_connection() {
        const REQUEST_COUNT: usize = 8;

        let server = start_reconnecting_storage_server().await;
        server.client.get("first", None, true, false).await.unwrap();
        server.close_initial.send(()).unwrap();
        server.initial_closed.await.unwrap();
        wait_until_disconnected(&server.client).await;
        server.restart.send(()).unwrap();
        server.restarted.await.unwrap();

        let requests = (0..REQUEST_COUNT)
            .map(|_| {
                let client = server.client.clone();
                tokio::spawn(async move { client.get("concurrent", None, true, false).await })
            })
            .collect::<Vec<_>>();
        for request in requests {
            assert_eq!(request.await.unwrap().unwrap().status, StatusCode::OK);
        }

        server.finished.send(()).unwrap();
        assert_eq!(server.server.await.unwrap(), 2);
    }
}
