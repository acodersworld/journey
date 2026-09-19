//! HTTP/2 over WebSocket transport.
//!
//! This crate adapts a WebSocket into the ordered byte stream used by an
//! inner HTTP/2 connection. WebSocket framing and control traffic stay here;
//! applications receive normal streaming `h2` request and response values.

use std::{fmt, sync::Arc};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use h2::{client, server};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream},
    sync::{mpsc, Mutex, Notify},
    task::JoinSet,
};
use tokio_tungstenite::{
    accept_hdr_async_with_config, connect_async_with_config,
    tungstenite::{
        client::IntoClientRequest,
        handshake::server::{ErrorResponse, Request, Response},
        http::HeaderValue,
        protocol::{CloseFrame, WebSocketConfig},
        Message,
    },
    WebSocketStream,
};

const DEFAULT_DUPLEX_CAPACITY: usize = 256 * 1024;
const DEFAULT_BRIDGE_BUFFER_SIZE: usize = 16 * 1024;
const DEFAULT_MAX_MESSAGE_SIZE: usize = 256 * 1024;
const DEFAULT_MAX_FRAME_SIZE: usize = 256 * 1024;
const DEFAULT_WRITE_BUFFER_SIZE: usize = 16 * 1024;
const DEFAULT_MAX_WRITE_BUFFER_SIZE: usize = 512 * 1024;
const WRITER_CHANNEL_CAPACITY: usize = 8;

/// WebSocket subprotocol negotiated by this crate.
pub const SUBPROTOCOL: &str = "h2-over-websocket-v1";

/// Runtime limits for one HTTP/2-over-WebSocket connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Maximum number of bytes buffered between the WebSocket bridge and the
    /// application protocol.
    pub duplex_capacity: usize,
    /// Number of bytes read from the application stream for each outbound
    /// binary WebSocket message.
    pub bridge_buffer_size: usize,
    /// Maximum size of an incoming WebSocket message.
    pub max_message_size: usize,
    /// Maximum size of one incoming WebSocket frame.
    pub max_frame_size: usize,
    /// Target size of the WebSocket write buffer.
    pub write_buffer_size: usize,
    /// Maximum size of the WebSocket write buffer.
    pub max_write_buffer_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duplex_capacity: DEFAULT_DUPLEX_CAPACITY,
            bridge_buffer_size: DEFAULT_BRIDGE_BUFFER_SIZE,
            max_message_size: DEFAULT_MAX_MESSAGE_SIZE,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            write_buffer_size: DEFAULT_WRITE_BUFFER_SIZE,
            max_write_buffer_size: DEFAULT_MAX_WRITE_BUFFER_SIZE,
        }
    }
}

impl Config {
    fn validate(self) -> Result<(), Error> {
        if self.duplex_capacity == 0 {
            return Err(Error::Configuration("duplex capacity must be greater than zero"));
        }
        if self.bridge_buffer_size == 0 {
            return Err(Error::Configuration("bridge buffer size must be greater than zero"));
        }
        if self.max_message_size == 0 {
            return Err(Error::Configuration("maximum message size must be greater than zero"));
        }
        if self.max_frame_size == 0 {
            return Err(Error::Configuration("maximum frame size must be greater than zero"));
        }
        if self.bridge_buffer_size > self.max_message_size {
            return Err(Error::Configuration(
                "bridge buffer size must not exceed maximum message size",
            ));
        }
        if self.bridge_buffer_size > self.max_frame_size {
            return Err(Error::Configuration(
                "bridge buffer size must not exceed maximum frame size",
            ));
        }
        if self.write_buffer_size == 0 {
            return Err(Error::Configuration("write buffer size must be greater than zero"));
        }
        if self.max_write_buffer_size <= self.write_buffer_size {
            return Err(Error::Configuration(
                "maximum write buffer size must be greater than write buffer size",
            ));
        }
        Ok(())
    }

    fn websocket_config(self) -> WebSocketConfig {
        WebSocketConfig::default()
            .read_buffer_size(self.bridge_buffer_size)
            .write_buffer_size(self.write_buffer_size)
            .max_write_buffer_size(self.max_write_buffer_size)
            .max_message_size(Some(self.max_message_size))
            .max_frame_size(Some(self.max_frame_size))
    }
}

/// Errors produced while connecting or moving bytes through the transport.
#[derive(Debug)]
pub enum Error {
    /// The inner HTTP/2 handshake, request, or response failed.
    Http2(h2::Error),
    /// An HTTP request or response could not be constructed.
    Http(http::Error),
    /// The local byte-stream adapter failed.
    Io(std::io::Error),
    /// The WebSocket implementation rejected a frame or operation.
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    /// The peer sent a WebSocket message or handshake this transport does not support.
    Protocol(&'static str),
    /// The adapter configuration is invalid.
    Configuration(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http2(error) => write!(formatter, "HTTP/2 error: {error}"),
            Self::Http(error) => write!(formatter, "HTTP error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::WebSocket(error) => write!(formatter, "WebSocket error: {error}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Configuration(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<h2::Error> for Error {
    fn from(error: h2::Error) -> Self {
        Self::Http2(error)
    }
}

impl From<http::Error> for Error {
    fn from(error: http::Error) -> Self {
        Self::Http(error)
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(error: tokio_tungstenite::tungstenite::Error) -> Self {
        Self::WebSocket(Box::new(error))
    }
}

/// The result of serving an application protocol over a WebSocket.
#[derive(Debug)]
pub enum ServeError<E> {
    /// The WebSocket or byte-stream bridge stopped with an error.
    Transport(Error),
    /// The application protocol running over the byte stream stopped with an
    /// error.
    Application(E),
}

impl<E: fmt::Display> fmt::Display for ServeError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(formatter, "WebSocket transport error: {error}"),
            Self::Application(error) => write!(formatter, "application server error: {error}"),
        }
    }
}

impl<E> std::error::Error for ServeError<E>
where
    E: std::error::Error + 'static,
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(error) => Some(error),
            Self::Application(error) => Some(error),
        }
    }
}

enum WriterCommand {
    Binary(Bytes),
    Pong(Bytes),
    Close {
        frame: Option<CloseFrame>,
        completed: tokio::sync::oneshot::Sender<()>,
    },
}

async fn websocket_writer<S>(
    mut websocket_write: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut commands: mpsc::Receiver<WriterCommand>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(command) = commands.recv().await {
        match command {
            WriterCommand::Binary(data) => websocket_write.send(Message::Binary(data)).await?,
            WriterCommand::Pong(data) => websocket_write.send(Message::Pong(data)).await?,
            WriterCommand::Close { frame, completed } => {
                websocket_write.send(Message::Close(frame)).await?;
                let _ = completed.send(());
                return Ok(());
            }
        }
    }

    Ok(())
}

async fn close_websocket(commands: &mpsc::Sender<WriterCommand>, frame: Option<CloseFrame>) {
    let (completed, wait) = tokio::sync::oneshot::channel();
    if commands
        .send(WriterCommand::Close { frame, completed })
        .await
        .is_ok()
    {
        let _ = wait.await;
    }
}

async fn local_to_websocket<T>(
    mut io_read: tokio::io::ReadHalf<T>,
    commands: mpsc::Sender<WriterCommand>,
    buffer_size: usize,
) -> Result<(), Error>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; buffer_size];

    loop {
        let read = io_read.read(&mut buffer).await?;
        if read == 0 {
            close_websocket(&commands, None).await;
            return Ok(());
        }

        commands
            .send(WriterCommand::Binary(Bytes::copy_from_slice(&buffer[..read])))
            .await
            .map_err(|_| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "WebSocket writer stopped",
                ))
            })?;
    }
}

async fn websocket_to_local<S, T>(
    mut websocket_read: futures_util::stream::SplitStream<WebSocketStream<S>>,
    mut io_write: tokio::io::WriteHalf<T>,
    commands: mpsc::Sender<WriterCommand>,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    while let Some(message) = websocket_read.next().await {
        match message? {
            Message::Binary(data) => io_write.write_all(&data).await?,
            Message::Ping(data) => {
                commands
                    .send(WriterCommand::Pong(data))
                    .await
                    .map_err(|_| {
                        Error::Io(std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "WebSocket writer stopped",
                        ))
                    })?;
            }
            Message::Pong(_) => {}
            Message::Close(frame) => {
                close_websocket(&commands, frame).await;
                io_write.shutdown().await?;
                return Ok(());
            }
            Message::Text(_) => {
                return Err(Error::Protocol("text WebSocket messages are not supported"));
            }
            Message::Frame(_) => {}
        }
    }

    io_write.shutdown().await?;
    Ok(())
}

/// Pumps a WebSocket and byte stream in both directions.
async fn bridge<S, T>(websocket: WebSocketStream<S>, io: T, buffer_size: usize) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (websocket_write, websocket_read) = websocket.split();
    let (io_read, io_write) = tokio::io::split(io);
    let (commands, command_queue) = mpsc::channel(WRITER_CHANNEL_CAPACITY);
    let mut tasks = JoinSet::new();

    tasks.spawn(websocket_writer(websocket_write, command_queue));
    tasks.spawn(local_to_websocket(io_read, commands.clone(), buffer_size));
    tasks.spawn(websocket_to_local(websocket_read, io_write, commands));

    let result = match tasks.join_next().await {
        Some(Ok(result)) => result,
        Some(Err(error)) => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            format!("WebSocket bridge task failed: {error}"),
        ))),
        None => Ok(()),
    };

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    result
}

/// Serves an application protocol over an established WebSocket.
///
/// Callers that perform the outer WebSocket handshake themselves must apply
/// the limits in [`Config`] and negotiate [`SUBPROTOCOL`] before calling this
/// function. Prefer [`accept_websocket`] or [`connect_websocket`] when the
/// crate can own the outer handshake.
pub async fn serve<S, F, Fut, E>(
    websocket: WebSocketStream<S>,
    config: Config,
    start_server: F,
) -> Result<(), ServeError<E>>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: FnOnce(DuplexStream) -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    config.validate().map_err(ServeError::Transport)?;
    let (server_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let server = start_server(server_io);
    let bridge = bridge(websocket, bridge_io, config.bridge_buffer_size);

    tokio::select! {
        result = server => result.map_err(ServeError::Application),
        result = bridge => result.map_err(ServeError::Transport),
    }
}

fn has_subprotocol(value: Option<&HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(',').any(|token| token.trim() == SUBPROTOCOL))
        .unwrap_or(false)
}

fn reject_subprotocol() -> ErrorResponse {
    http::Response::builder()
        .status(http::StatusCode::BAD_REQUEST)
        .body(Some("required WebSocket subprotocol was not offered".to_owned()))
        .expect("valid WebSocket rejection response")
}

/// Accepts a WebSocket with the transport's limits and subprotocol.
pub async fn accept_websocket<S>(stream: S, config: Config) -> Result<WebSocketStream<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    config.validate()?;
    let websocket = accept_hdr_async_with_config(
        stream,
        |request: &Request, mut response: Response| {
            if !has_subprotocol(request.headers().get("Sec-WebSocket-Protocol")) {
                return Err(reject_subprotocol());
            }

            response.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(SUBPROTOCOL),
            );
            Ok(response)
        },
        Some(config.websocket_config()),
    )
    .await?;
    Ok(websocket)
}

/// Connects a WebSocket with the transport's limits and subprotocol.
pub async fn connect_websocket<R>(
    request: R,
    config: Config,
) -> Result<
    (
        WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    Error,
>
where
    R: IntoClientRequest + Unpin,
{
    config.validate()?;
    let mut request = request.into_client_request()?;
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let (websocket, response) = connect_async_with_config(
        request,
        Some(config.websocket_config()),
        false,
    )
    .await?;

    if response
        .headers()
        .get("Sec-WebSocket-Protocol")
        .and_then(|value| value.to_str().ok())
        != Some(SUBPROTOCOL)
    {
        return Err(Error::Protocol("WebSocket subprotocol negotiation failed"));
    }

    Ok((websocket, response))
}

struct SessionState {
    result: Mutex<Option<Result<(), Arc<Error>>>>,
    notify: Notify,
}

impl SessionState {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            notify: Notify::new(),
        }
    }

    async fn finish(&self, result: Result<(), Error>) {
        let mut terminal = self.result.lock().await;
        if terminal.is_none() {
            *terminal = Some(result.map_err(|error| Arc::new(error)));
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<(), Arc<Error>> {
        loop {
            // Create this before checking the result: finish() may notify between
            // the check and the await, and notify_waiters() does not retain permits.
            let notified = self.notify.notified();
            if let Some(result) = self.result.lock().await.clone() {
                return result;
            }
            notified.await;
        }
    }
}

/// A cloneable streaming request handle for one HTTP/2 connection.
#[derive(Clone)]
pub struct ClientSender {
    client: Arc<client::SendRequest<Bytes>>,
}

impl ClientSender {
    /// Waits until the peer permits another HTTP/2 stream.
    pub async fn ready(&self) -> Result<ReadyClientSender, Error> {
        Ok(ReadyClientSender(self.client.as_ref().clone().ready().await?))
    }

    /// Sends a complete HTTP/2 request head and returns its streaming response future.
    pub async fn send_request(
        &self,
        request: http::Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), Error> {
        self.ready().await?.send_request(request, end_of_stream)
    }
}

/// A sender that has completed the HTTP/2 stream-readiness check.
pub struct ReadyClientSender(client::SendRequest<Bytes>);

impl ReadyClientSender {
    /// Sends a request using this ready HTTP/2 sender.
    pub fn send_request(
        &mut self,
        request: http::Request<()>,
        end_of_stream: bool,
    ) -> Result<(client::ResponseFuture, h2::SendStream<Bytes>), Error> {
        Ok(self.0.send_request(request, end_of_stream)?)
    }
}

/// A reusable HTTP/2 client carried by one persistent WebSocket connection.
#[derive(Clone)]
pub struct ClientSession {
    sender: ClientSender,
    state: Arc<SessionState>,
}

impl ClientSession {
    /// Returns a cloneable streaming request handle.
    pub fn sender(&self) -> ClientSender {
        self.sender.clone()
    }

    /// Waits for the terminal HTTP/2 or WebSocket result.
    pub async fn wait(&self) -> Result<(), Arc<Error>> {
        self.state.wait().await
    }
}

async fn connect_h2(
    io: DuplexStream,
) -> Result<(client::SendRequest<Bytes>, client::Connection<DuplexStream, Bytes>), Error> {
    Ok(client::handshake(io).await?)
}

/// Establishes an HTTP/2 client session over an already-upgraded WebSocket.
pub async fn connect_client<S>(
    websocket: WebSocketStream<S>,
    config: Config,
) -> Result<ClientSession, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let (client_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let mut bridge_task = tokio::spawn(bridge(websocket, bridge_io, config.bridge_buffer_size));
    let (client, connection) = match connect_h2(client_io).await {
        Ok(connection) => connection,
        Err(error) => {
            bridge_task.abort();
            return Err(error);
        }
    };

    let state = Arc::new(SessionState::new());
    let driver_state = Arc::clone(&state);
    tokio::spawn(async move {
        let result = tokio::select! {
            result = connection => {
                bridge_task.abort();
                let _ = bridge_task.await;
                result.map_err(Error::from)
            },
            result = &mut bridge_task => match result {
                Ok(result) => result,
                Err(error) => Err(Error::Io(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("WebSocket bridge task failed: {error}"),
                ))),
            },
        };
        driver_state.finish(result).await;
    });

    Ok(ClientSession {
        sender: ClientSender {
            client: Arc::new(client),
        },
        state,
    })
}

/// Opens an outbound WebSocket and establishes an HTTP/2 client session.
pub async fn connect(websocket_url: &str, config: Config) -> Result<ClientSession, Error> {
    let (websocket, _) = connect_websocket(websocket_url, config).await?;
    connect_client(websocket, config).await
}

/// A request and response handle accepted by [`ServerSession`].
pub type IncomingRequest = (http::Request<h2::RecvStream>, server::SendResponse<Bytes>);

/// An HTTP/2 server session carried by one persistent WebSocket connection.
pub struct ServerSession {
    requests: mpsc::Receiver<IncomingRequest>,
    state: Arc<SessionState>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ServerSession {
    /// Accepts the next HTTP/2 request and its response handle.
    pub async fn accept(
        &mut self,
    ) -> Result<Option<IncomingRequest>, Error> {
        Ok(self.requests.recv().await)
    }

    /// Waits for the HTTP/2 or WebSocket connection to reach its terminal state.
    pub async fn wait(&self) -> Result<(), Arc<Error>> {
        self.state.wait().await
    }
}

impl Drop for ServerSession {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

async fn run_server_connection(
    mut connection: server::Connection<DuplexStream, Bytes>,
    requests: mpsc::Sender<IncomingRequest>,
    mut bridge_task: tokio::task::JoinHandle<Result<(), Error>>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> Result<(), Error> {
    let connection_result = async move {
        while let Some(result) = connection.accept().await {
            let request = result?;
            if requests.send(request).await.is_err() {
                return Ok(());
            }
        }
        Ok(())
    };

    let result = tokio::select! {
        result = connection_result => {
            bridge_task.abort();
            let _ = bridge_task.await;
            result
        },
        result = &mut bridge_task => match result {
            Ok(result) => result,
            Err(error) => Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("WebSocket bridge task failed: {error}"),
            ))),
        },
        _ = &mut shutdown => {
            bridge_task.abort();
            let _ = bridge_task.await;
            Ok(())
        },
    };
    result
}

/// Establishes an HTTP/2 server session over an already-upgraded WebSocket.
pub async fn server_session<S>(
    websocket: WebSocketStream<S>,
    config: Config,
) -> Result<ServerSession, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let (server_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let bridge_task = tokio::spawn(bridge(websocket, bridge_io, config.bridge_buffer_size));
    let connection = match server::handshake(server_io).await {
        Ok(connection) => connection,
        Err(error) => {
            bridge_task.abort();
            return Err(error.into());
        }
    };
    let (requests, request_queue) = mpsc::channel(WRITER_CHANNEL_CAPACITY);
    let (shutdown, shutdown_signal) = tokio::sync::oneshot::channel();
    let state = Arc::new(SessionState::new());
    let driver_state = Arc::clone(&state);
    tokio::spawn(async move {
        let result = run_server_connection(connection, requests, bridge_task, shutdown_signal).await;
        driver_state.finish(result).await;
    });

    Ok(ServerSession {
        requests: request_queue,
        state,
        shutdown: Some(shutdown),
    })
}

/// Accepts a bounded, protocol-negotiated WebSocket and establishes an HTTP/2 server session.
pub async fn accept_server<S>(stream: S, config: Config) -> Result<ServerSession, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let websocket = accept_websocket(stream, config).await?;
    server_session(websocket, config).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;
    use tokio::time::{timeout, Duration};
    use tokio_tungstenite::client_async_with_config;

    async fn websocket_pair(
        config: Config,
    ) -> (WebSocketStream<DuplexStream>, WebSocketStream<DuplexStream>) {
        let (client_io, server_io) = duplex(1024 * 1024);
        let client = tokio::spawn(async move {
            let mut request = "ws://localhost".into_client_request().expect("request");
            request.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_static(SUBPROTOCOL),
            );
            client_async_with_config(request, client_io, Some(config.websocket_config()))
                .await
                .expect("client websocket handshake")
                .0
        });
        let server = tokio::spawn(async move {
            accept_websocket(server_io, config)
                .await
                .expect("server websocket handshake")
        });
        (client.await.expect("client task"), server.await.expect("server task"))
    }

    async fn read_body(mut body: h2::RecvStream) -> Bytes {
        let mut result = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("response data");
            result.extend_from_slice(&chunk);
            body.flow_control()
                .release_capacity(chunk.len())
                .expect("release capacity");
        }
        Bytes::from(result)
    }

    #[tokio::test]
    async fn binary_messages_form_one_continuous_byte_stream() {
        let (mut client, mut server) = websocket_pair(Config::default()).await;
        client
            .send(Message::Binary(Bytes::from_static(b"hello")))
            .await
            .expect("send first message");
        client
            .send(Message::Binary(Bytes::from_static(b" world")))
            .await
            .expect("send second message");
        assert_eq!(server.next().await.expect("first message").expect("message"), Message::Binary(Bytes::from_static(b"hello")));
        assert_eq!(server.next().await.expect("second message").expect("message"), Message::Binary(Bytes::from_static(b" world")));
    }

    #[tokio::test]
    async fn http2_client_and_server_reuse_one_websocket() {
        let config = Config::default();
        let (client_websocket, server_websocket) = websocket_pair(config).await;
        let server_session_task = tokio::spawn(server_session(server_websocket, config));
        let client = connect_client(client_websocket, config)
            .await
            .expect("client session");
        let mut server = server_session_task
            .await
            .expect("server session task")
            .expect("server session");
        let server_task = tokio::spawn(async move {
            while let Some((request, mut respond)) = server.accept().await.expect("accept") {
                let body = if request.uri().path() == "/one" { "first" } else { "second" };
                let response = http::Response::builder()
                    .version(http::Version::HTTP_2)
                    .status(200)
                    .body(())
                    .expect("response");
                respond
                    .send_response(response, false)
                    .expect("headers")
                    .send_data(Bytes::from(body), true)
                    .expect("body");
            }
        });

        let sender = client.sender();
        let first = sender
            .send_request(
                http::Request::builder()
                    .version(http::Version::HTTP_2)
                    .method("GET")
                    .uri("https://home.internal/one")
                    .body(())
                    .expect("request"),
                true,
            )
            .await
            .expect("first request");
        let second = sender
            .send_request(
                http::Request::builder()
                    .version(http::Version::HTTP_2)
                    .method("GET")
                    .uri("https://home.internal/two")
                    .body(())
                    .expect("request"),
                true,
            )
            .await
            .expect("second request");
        let (first, second) = tokio::join!(first.0, second.0);
        let first = first.expect("first response");
        let second = second.expect("second response");
        assert_eq!(read_body(first.into_body()).await, Bytes::from_static(b"first"));
        assert_eq!(read_body(second.into_body()).await, Bytes::from_static(b"second"));

        drop(client);
        server_task.abort();
    }

    #[tokio::test]
    async fn invalid_limits_are_rejected() {
        let config = Config {
            max_write_buffer_size: 1,
            ..Config::default()
        };
        assert!(matches!(config.validate(), Err(Error::Configuration(_))));
    }

    #[tokio::test]
    async fn default_configuration_is_valid() {
        assert!(Config::default().validate().is_ok());
    }

    #[tokio::test]
    async fn bridge_buffer_equal_to_message_and_frame_limits_is_valid() {
        let config = Config {
            bridge_buffer_size: 8,
            max_message_size: 8,
            max_frame_size: 8,
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn bridge_buffer_above_message_limit_is_rejected() {
        let config = Config {
            bridge_buffer_size: 9,
            max_message_size: 8,
            ..Config::default()
        };
        assert!(matches!(
            config.validate(),
            Err(Error::Configuration(
                "bridge buffer size must not exceed maximum message size"
            ))
        ));
    }

    #[tokio::test]
    async fn bridge_buffer_above_frame_limit_is_rejected() {
        let config = Config {
            bridge_buffer_size: 9,
            max_frame_size: 8,
            ..Config::default()
        };
        assert!(matches!(
            config.validate(),
            Err(Error::Configuration(
                "bridge buffer size must not exceed maximum frame size"
            ))
        ));
    }

    #[tokio::test]
    async fn incoming_message_limit_is_enforced_by_websocket_configuration() {
        let config = Config {
            bridge_buffer_size: 8,
            max_message_size: 8,
            max_frame_size: 8,
            ..Config::default()
        };
        let (mut client, mut server) = websocket_pair(config).await;
        client
            .send(Message::Binary(Bytes::from_static(b"too large")))
            .await
            .expect("send oversized message");
        assert!(server.next().await.expect("message result").is_err());
    }

    #[tokio::test]
    async fn text_messages_are_rejected_by_the_bridge() {
        let (client_websocket, server_websocket) = websocket_pair(Config::default()).await;
        let (_server_io, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(client_websocket, bridge_io, 16));
        let mut server_websocket = server_websocket;
        server_websocket
            .send(Message::Text("text".into()))
            .await
            .expect("send text");
        let error = bridge.await.expect("bridge task").expect_err("text must fail");
        assert!(matches!(error, Error::Protocol(_)));
    }

    #[tokio::test]
    async fn session_waiter_registered_before_finish_is_not_lost() {
        let state = Arc::new(SessionState::new());
        let waiter_state = Arc::clone(&state);
        let waiter = tokio::spawn(async move { waiter_state.wait().await });

        tokio::task::yield_now().await;
        state.finish(Ok(())).await;

        assert!(timeout(Duration::from_millis(100), waiter)
            .await
            .expect("waiter timed out")
            .expect("waiter task")
            .is_ok());
    }

    #[tokio::test]
    async fn session_wait_after_finish_returns_immediately() {
        let state = SessionState::new();
        state.finish(Ok(())).await;

        assert!(timeout(Duration::from_millis(100), state.wait())
            .await
            .expect("wait timed out")
            .is_ok());
    }

    #[tokio::test]
    async fn simultaneous_session_waiters_receive_the_terminal_result() {
        let state = Arc::new(SessionState::new());
        let mut waiters = Vec::new();
        for _ in 0..4 {
            let waiter_state = Arc::clone(&state);
            waiters.push(tokio::spawn(async move { waiter_state.wait().await }));
        }

        tokio::task::yield_now().await;
        state.finish(Ok(())).await;

        for waiter in waiters {
            assert!(timeout(Duration::from_millis(100), waiter)
                .await
                .expect("waiter timed out")
                .expect("waiter task")
                .is_ok());
        }
    }

    #[tokio::test]
    async fn repeated_session_waits_after_completion_return_immediately() {
        let state = SessionState::new();
        state.finish(Ok(())).await;

        for _ in 0..4 {
            assert!(timeout(Duration::from_millis(100), state.wait())
                .await
                .expect("wait timed out")
                .is_ok());
        }
    }

    #[tokio::test]
    async fn session_wait_preserves_error_result() {
        let state = SessionState::new();
        state
            .finish(Err(Error::Protocol("session failed")))
            .await;

        let error = timeout(Duration::from_millis(100), state.wait())
            .await
            .expect("wait timed out")
            .expect_err("wait should preserve the error");
        assert!(matches!(error.as_ref(), Error::Protocol("session failed")));
    }
}
