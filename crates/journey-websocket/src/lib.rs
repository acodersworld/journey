//! HTTP/2 over WebSocket transport.
//!
//! This crate adapts a WebSocket into the ordered byte stream used by an
//! inner HTTP/2 connection. WebSocket framing and control traffic stay here;
//! applications receive normal streaming `h2` request and response values.

use std::{fmt, sync::Arc, time::Duration};

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
const DEFAULT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
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
    /// Maximum time to wait for a queued WebSocket close command and local
    /// write-half shutdown. Expiration tears down the bridge cleanly.
    pub close_timeout: Duration,
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
            close_timeout: DEFAULT_CLOSE_TIMEOUT,
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
        if self.close_timeout.is_zero() {
            return Err(Error::Configuration("close timeout must be greater than zero"));
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
                let result = websocket_write.send(Message::Close(frame)).await;
                let _ = completed.send(());
                match result {
                    Ok(())
                    | Err(tokio_tungstenite::tungstenite::Error::Protocol(
                        tokio_tungstenite::tungstenite::error::ProtocolError::SendAfterClosing,
                    )) => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            }
        }
    }

    Ok(())
}

async fn close_websocket(
    commands: &mpsc::Sender<WriterCommand>,
    frame: Option<CloseFrame>,
    close_timeout: Duration,
) {
    let (completed, wait) = tokio::sync::oneshot::channel();
    let close = async {
        if commands
            .send(WriterCommand::Close { frame, completed })
            .await
            .is_ok()
        {
            let _ = wait.await;
        }
    };
    let _ = tokio::time::timeout(close_timeout, close).await;
}

async fn local_to_websocket<T>(
    mut io_read: tokio::io::ReadHalf<T>,
    commands: mpsc::Sender<WriterCommand>,
    buffer_size: usize,
    close_timeout: Duration,
) -> Result<(), Error>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut buffer = vec![0_u8; buffer_size];

    loop {
        let read = io_read.read(&mut buffer).await?;
        if read == 0 {
            close_websocket(&commands, None, close_timeout).await;
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

async fn shutdown_local<T>(
    mut io_write: tokio::io::WriteHalf<T>,
    close_timeout: Duration,
) -> Result<(), Error>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    tokio::time::timeout(close_timeout, io_write.shutdown())
        .await
        .map_err(|_| {
            Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local byte-stream shutdown timed out",
            ))
        })??;
    Ok(())
}

async fn websocket_to_local<S, T>(
    mut websocket_read: futures_util::stream::SplitStream<WebSocketStream<S>>,
    mut io_write: tokio::io::WriteHalf<T>,
    commands: mpsc::Sender<WriterCommand>,
    close_timeout: Duration,
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
                close_websocket(&commands, frame, close_timeout).await;
                return shutdown_local(io_write, close_timeout).await;
            }
            Message::Text(_) => {
                return Err(Error::Protocol("text WebSocket messages are not supported"));
            }
            Message::Frame(_) => {}
        }
    }

    shutdown_local(io_write, close_timeout).await
}

/// Pumps a WebSocket and byte stream in both directions.
async fn bridge<S, T>(
    websocket: WebSocketStream<S>,
    io: T,
    buffer_size: usize,
    close_timeout: Duration,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (websocket_write, websocket_read) = websocket.split();
    let (io_read, io_write) = tokio::io::split(io);
    let (commands, command_queue) = mpsc::channel(WRITER_CHANNEL_CAPACITY);
    let mut tasks = JoinSet::new();

    tasks.spawn(websocket_writer(websocket_write, command_queue));
    tasks.spawn(local_to_websocket(
        io_read,
        commands.clone(),
        buffer_size,
        close_timeout,
    ));
    tasks.spawn(websocket_to_local(
        websocket_read,
        io_write,
        commands,
        close_timeout,
    ));

    let result = match tasks.join_next().await {
        Some(Ok(result)) => result,
        Some(Err(error)) => Err(Error::Io(std::io::Error::other(
            format!("WebSocket bridge task failed: {error}"),
        ))),
        None => Ok(()),
    };

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    result
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

// Tungstenite requires this concrete, non-boxed error type for its handshake
// callback, so this narrow allowance is intentional.
#[allow(clippy::result_large_err)]
fn accept_websocket_response(
    request: &Request,
    mut response: Response,
) -> Result<Response, ErrorResponse> {
    if !has_subprotocol(request.headers().get("Sec-WebSocket-Protocol")) {
        return Err(reject_subprotocol());
    }

    response.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    Ok(response)
}

/// Accepts a WebSocket with the transport's limits and subprotocol.
pub async fn accept_websocket<S>(stream: S, config: Config) -> Result<WebSocketStream<S>, Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    config.validate()?;
    let websocket = accept_hdr_async_with_config(
        stream,
        accept_websocket_response,
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
            *terminal = Some(result.map_err(Arc::new));
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
///
/// The caller must have applied [`Config`] limits and negotiated [`SUBPROTOCOL`]
/// during the WebSocket handshake. This function cannot retroactively constrain
/// allocations made during that handshake. Prefer [`connect`] or
/// [`connect_websocket`] when this crate should own those defaults.
pub async fn connect_client<S>(
    websocket: WebSocketStream<S>,
    config: Config,
) -> Result<ClientSession, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let (client_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let mut bridge_task = tokio::spawn(bridge(
        websocket,
        bridge_io,
        config.bridge_buffer_size,
        config.close_timeout,
    ));
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
                Err(error) => Err(Error::Io(std::io::Error::other(
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
            Err(error) => Err(Error::Io(std::io::Error::other(
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
///
/// The caller must have applied [`Config`] limits and negotiated [`SUBPROTOCOL`]
/// during the WebSocket handshake. This function cannot retroactively constrain
/// allocations made during that handshake. Prefer [`accept_server`] or
/// [`accept_websocket`] when this crate should own those defaults.
pub async fn server_session<S>(
    websocket: WebSocketStream<S>,
    config: Config,
) -> Result<ServerSession, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let (server_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let bridge_task = tokio::spawn(bridge(
        websocket,
        bridge_io,
        config.bridge_buffer_size,
        config.close_timeout,
    ));
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
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, Ordering},
        task::{Context, Poll},
    };
    use tokio::io::duplex;
    use tokio::time::{timeout, Duration};
    use tokio_tungstenite::client_async_with_config;
    use tokio_tungstenite::tungstenite::handshake::client::Response as ClientResponse;

    // Tungstenite requires its concrete, non-boxed handshake error type here.
    #[allow(clippy::result_large_err)]
    fn accept_without_subprotocol(
        _request: &Request,
        response: Response,
    ) -> Result<Response, ErrorResponse> {
        Ok(response)
    }

    struct PendingWriteNotifier {
        inner: DuplexStream,
        pending: Arc<Notify>,
    }

    impl AsyncRead for PendingWriteNotifier {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl AsyncWrite for PendingWriteNotifier {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let result = Pin::new(&mut self.inner).poll_write(context, buffer);
            if result.is_pending() {
                self.pending.notify_waiters();
            }
            result
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

    struct FailingIo {
        inner: DuplexStream,
        fail_read: Arc<AtomicBool>,
        fail_write: Arc<AtomicBool>,
    }

    impl AsyncRead for FailingIo {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.fail_read.load(Ordering::SeqCst) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected read failure",
                )));
            }
            Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl AsyncWrite for FailingIo {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "injected write failure",
                )));
            }
            Pin::new(&mut self.inner).poll_write(context, buffer)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(context)
        }
    }

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

    async fn websocket_handshake_with_offer(
        config: Config,
        offer: Option<&str>,
    ) -> (
        Result<(WebSocketStream<DuplexStream>, ClientResponse), tokio_tungstenite::tungstenite::Error>,
        Result<WebSocketStream<DuplexStream>, Error>,
    ) {
        let (client_io, server_io) = duplex(1024 * 1024);
        let offer = offer.map(str::to_owned);
        let client = tokio::spawn(async move {
            let mut request = "ws://localhost".into_client_request().expect("request");
            if let Some(offer) = offer {
                request.headers_mut().insert(
                    "Sec-WebSocket-Protocol",
                    HeaderValue::from_str(&offer).expect("subprotocol offer"),
                );
            }
            client_async_with_config(request, client_io, Some(config.websocket_config())).await
        });
        let server = tokio::spawn(async move { accept_websocket(server_io, config).await });
        (
            client.await.expect("client task"),
            server.await.expect("server task"),
        )
    }

    async fn websocket_pair_with_server_write_backpressure(
        config: Config,
        capacity: usize,
    ) -> (
        WebSocketStream<DuplexStream>,
        WebSocketStream<PendingWriteNotifier>,
        Arc<Notify>,
    ) {
        let (client_io, server_io) = duplex(capacity);
        let pending = Arc::new(Notify::new());
        let server_pending = Arc::clone(&pending);
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
            accept_websocket(
                PendingWriteNotifier {
                    inner: server_io,
                    pending: server_pending,
                },
                config,
            )
            .await
            .expect("server websocket handshake")
        });
        (
            client.await.expect("client task"),
            server.await.expect("server task"),
            pending,
        )
    }

    async fn websocket_pair_with_failures(
        config: Config,
    ) -> (
        WebSocketStream<DuplexStream>,
        WebSocketStream<FailingIo>,
        Arc<AtomicBool>,
        Arc<AtomicBool>,
    ) {
        let (client_io, server_io) = duplex(1024 * 1024);
        let fail_read = Arc::new(AtomicBool::new(false));
        let fail_write = Arc::new(AtomicBool::new(false));
        let server_fail_read = Arc::clone(&fail_read);
        let server_fail_write = Arc::clone(&fail_write);
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
            accept_websocket(
                FailingIo {
                    inner: server_io,
                    fail_read: server_fail_read,
                    fail_write: server_fail_write,
                },
                config,
            )
            .await
            .expect("server websocket handshake")
        });
        (
            client.await.expect("client task"),
            server.await.expect("server task"),
            fail_read,
            fail_write,
        )
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
    async fn bridge_websocket_messages_form_one_continuous_byte_stream() {
        let config = Config {
            bridge_buffer_size: 4,
            ..Config::default()
        };
        let (mut client, server) = websocket_pair(config).await;
        let (mut application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));

        client
            .send(Message::Binary(Bytes::from_static(b"hello")))
            .await
            .expect("send first message");
        client
            .send(Message::Binary(Bytes::from_static(b"websocket")))
            .await
            .expect("send second message");
        client
            .send(Message::Binary(Bytes::from_static(b"bridge!")))
            .await
            .expect("send third message");

        let mut received = Vec::new();
        for size in [2, 7, 3, 9] {
            let mut chunk = vec![0_u8; size];
            timeout(Duration::from_millis(100), application.read_exact(&mut chunk))
                .await
                .expect("byte-stream read timed out")
                .expect("byte-stream read");
            received.extend_from_slice(&chunk);
        }
        assert_eq!(received, b"hellowebsocketbridge!");

        bridge.abort();
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn bridge_byte_stream_forms_binary_websocket_messages() {
        let config = Config {
            bridge_buffer_size: 4,
            ..Config::default()
        };
        let (mut client, server) = websocket_pair(config).await;
        let (mut application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        let expected = b"uneven byte-stream writes";

        for chunk in [b"uneven ".as_slice(), b"byte-stream ".as_slice(), b"writes".as_slice()] {
            application.write_all(chunk).await.expect("byte-stream write");
        }
        application.shutdown().await.expect("byte-stream shutdown");

        let mut received = Vec::new();
        while received.len() < expected.len() {
            let message = timeout(Duration::from_millis(100), client.next())
                .await
                .expect("WebSocket read timed out")
                .expect("WebSocket ended")
                .expect("WebSocket read");
            match message {
                Message::Binary(data) => received.extend_from_slice(&data),
                message => panic!("unexpected WebSocket message: {message:?}"),
            }
        }
        assert_eq!(received, expected);

        assert!(timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task")
            .is_ok());
    }

    #[tokio::test]
    async fn stalled_websocket_to_local_does_not_block_local_to_websocket() {
        let config = Config {
            bridge_buffer_size: 8,
            ..Config::default()
        };
        let (mut remote, server) = websocket_pair(config).await;
        let (application, bridge_io) = duplex(16);
        let pending = Arc::new(Notify::new());
        let bridge = tokio::spawn(bridge(
            server,
            PendingWriteNotifier {
                inner: bridge_io,
                pending: Arc::clone(&pending),
            },
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        let pending_write = pending.notified();

        remote
            .send(Message::Binary(Bytes::from(vec![b'x'; 128])))
            .await
            .expect("send stalled-direction message");
        timeout(Duration::from_millis(100), pending_write)
            .await
            .expect("WebSocket-to-local write did not stall");

        let (_application_read, mut application_write) = tokio::io::split(application);
        application_write
            .write_all(b"reverse direction")
            .await
            .expect("write opposite direction");
        let expected = b"reverse direction";
        let mut received = Vec::new();
        while received.len() < expected.len() {
            let message = timeout(Duration::from_millis(100), remote.next())
                .await
                .expect("local-to-WebSocket progress timed out")
                .expect("WebSocket ended")
                .expect("WebSocket read");
            match message {
                Message::Binary(data) => received.extend_from_slice(&data),
                message => panic!("unexpected WebSocket message: {message:?}"),
            }
        }
        assert_eq!(received, expected);

        bridge.abort();
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn stalled_local_to_websocket_does_not_block_websocket_to_local() {
        let config = Config {
            bridge_buffer_size: 4,
            ..Config::default()
        };
        let (mut remote, server, pending) =
            websocket_pair_with_server_write_backpressure(config, 64).await;
        let (application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        let (mut application_read, mut application_write) = tokio::io::split(application);
        let pending_write = pending.notified();
        let application_writer = tokio::spawn(async move {
            application_write
                .write_all(&vec![b'x'; 4096])
                .await
                .expect("write stalled-direction data");
        });

        timeout(Duration::from_millis(100), pending_write)
            .await
            .expect("local-to-WebSocket write did not stall");
        remote
            .send(Message::Binary(Bytes::from_static(b"reverse direction")))
            .await
            .expect("send opposite-direction message");
        let mut received = vec![0_u8; b"reverse direction".len()];
        timeout(
            Duration::from_millis(100),
            application_read.read_exact(&mut received),
        )
        .await
        .expect("WebSocket-to-local progress timed out")
        .expect("read opposite-direction message");
        assert_eq!(received, b"reverse direction");

        application_writer.abort();
        let _ = application_writer.await;
        bridge.abort();
        let _ = bridge.await;
    }

    #[tokio::test]
    async fn local_eof_performs_a_bounded_websocket_close() {
        let config = Config {
            close_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (mut remote, server) = websocket_pair(config).await;
        let (application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        drop(application);

        let message = timeout(Duration::from_millis(100), remote.next())
            .await
            .expect("close frame timed out")
            .expect("WebSocket ended")
            .expect("WebSocket read");
        assert!(matches!(message, Message::Close(_)));
        assert!(timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task")
            .is_ok());
    }

    #[tokio::test]
    async fn blocked_writer_reaches_the_close_deadline() {
        let config = Config {
            bridge_buffer_size: 4,
            close_timeout: Duration::from_millis(20),
            ..Config::default()
        };
        let (mut remote, server, pending) =
            websocket_pair_with_server_write_backpressure(config, 64).await;
        let (application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        let (_application_read, mut application_write) = tokio::io::split(application);
        let pending_write = pending.notified();
        let application_writer = tokio::spawn(async move {
            application_write
                .write_all(&vec![b'x'; 4096])
                .await
                .expect("write stalled-direction data");
        });

        timeout(Duration::from_millis(100), pending_write)
            .await
            .expect("local-to-WebSocket write did not stall");
        remote
            .send(Message::Close(None))
            .await
            .expect("send close frame");
        assert!(timeout(Duration::from_millis(200), bridge)
            .await
            .expect("blocked close did not reach its deadline")
            .expect("bridge task")
            .is_ok());

        application_writer.abort();
        let _ = application_writer.await;
    }

    #[tokio::test]
    async fn remote_close_shuts_down_the_local_byte_stream() {
        let config = Config {
            close_timeout: Duration::from_millis(50),
            ..Config::default()
        };
        let (mut remote, server) = websocket_pair(config).await;
        let (mut application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));

        remote
            .send(Message::Binary(Bytes::from_static(b"before close")))
            .await
            .expect("send data");
        remote
            .send(Message::Close(None))
            .await
            .expect("send close frame");
        let mut received = Vec::new();
        timeout(
            Duration::from_millis(100),
            application.read_to_end(&mut received),
        )
        .await
        .expect("local EOF timed out")
        .expect("local read");
        assert_eq!(received, b"before close");
        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(result.is_ok(), "bridge result: {result:?}");
        assert!(application.write_all(b"after close").await.is_err());
    }

    #[tokio::test]
    async fn websocket_read_failure_terminates_the_bridge() {
        let config = Config::default();
        let (_remote, server, fail_read, _fail_write) =
            websocket_pair_with_failures(config).await;
        fail_read.store(true, Ordering::SeqCst);
        let (_application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));

        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(matches!(result, Err(Error::WebSocket(_))), "result: {result:?}");
    }

    #[tokio::test]
    async fn websocket_write_failure_terminates_the_bridge() {
        let config = Config::default();
        let (_remote, server, _fail_read, fail_write) =
            websocket_pair_with_failures(config).await;
        fail_write.store(true, Ordering::SeqCst);
        let (mut application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        application
            .write_all(b"trigger WebSocket write")
            .await
            .expect("application write");

        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(matches!(result, Err(Error::WebSocket(_))), "result: {result:?}");
    }

    #[tokio::test]
    async fn local_read_failure_terminates_the_bridge() {
        let config = Config::default();
        let (_remote, server) = websocket_pair(config).await;
        let (_application, bridge_io) = duplex(64);
        let fail_read = Arc::new(AtomicBool::new(true));
        let bridge = tokio::spawn(bridge(
            server,
            FailingIo {
                inner: bridge_io,
                fail_read,
                fail_write: Arc::new(AtomicBool::new(false)),
            },
            config.bridge_buffer_size,
            config.close_timeout,
        ));

        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(matches!(result, Err(Error::Io(_))), "result: {result:?}");
    }

    #[tokio::test]
    async fn local_write_failure_terminates_the_bridge() {
        let config = Config::default();
        let (mut remote, server) = websocket_pair(config).await;
        let (_application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            FailingIo {
                inner: bridge_io,
                fail_read: Arc::new(AtomicBool::new(false)),
                fail_write: Arc::new(AtomicBool::new(true)),
            },
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        remote
            .send(Message::Binary(Bytes::from_static(b"trigger local write")))
            .await
            .expect("send binary message");

        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(matches!(result, Err(Error::Io(_))), "result: {result:?}");
    }

    #[tokio::test]
    async fn oversized_incoming_message_terminates_the_bridge() {
        let config = Config {
            bridge_buffer_size: 8,
            max_message_size: 8,
            max_frame_size: 8,
            ..Config::default()
        };
        let (mut remote, server) = websocket_pair(config).await;
        let (_application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        remote
            .send(Message::Binary(Bytes::from_static(b"too large")))
            .await
            .expect("send oversized message");

        let result = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task");
        assert!(matches!(result, Err(Error::WebSocket(_))), "result: {result:?}");
    }

    #[tokio::test]
    async fn aborting_the_outer_bridge_future_cancels_it() {
        let config = Config::default();
        let (_remote, server) = websocket_pair(config).await;
        let (_application, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            server,
            bridge_io,
            config.bridge_buffer_size,
            config.close_timeout,
        ));
        bridge.abort();

        assert!(bridge.await.expect_err("bridge should be cancelled").is_cancelled());
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
    async fn cancelled_http2_response_does_not_close_other_streams() {
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
        let cancelled = Arc::new(Notify::new());
        let server_cancelled = Arc::clone(&cancelled);
        let server_task = tokio::spawn(async move {
            let mut response_tasks = JoinSet::new();
            while let Some((request, mut respond)) = server.accept().await.expect("accept") {
                if request.uri().path() == "/large" {
                    let cancelled = Arc::clone(&server_cancelled);
                    response_tasks.spawn(async move {
                        let response = http::Response::builder()
                            .version(http::Version::HTTP_2)
                            .status(200)
                            .body(())
                            .expect("response");
                        let mut send = respond.send_response(response, false).expect("headers");
                        loop {
                            send.reserve_capacity(1024);
                            let capacity = futures_util::future::poll_fn(|context| {
                                send.poll_capacity(context)
                            })
                            .await;
                            let capacity = match capacity {
                                Some(Ok(capacity)) if capacity > 0 => capacity,
                                Some(Ok(_)) => continue,
                                Some(Err(_)) | None => {
                                    cancelled.notify_one();
                                    return;
                                }
                            };
                            if send
                                .send_data(Bytes::from(vec![b'x'; capacity.min(1024)]), false)
                                .is_err()
                            {
                                cancelled.notify_one();
                                return;
                            }
                        }
                    });
                } else {
                    let body = if request.uri().path() == "/small" {
                        Bytes::from_static(b"small response")
                    } else {
                        Bytes::from_static(b"not found")
                    };
                    let status = if request.uri().path() == "/small" {
                        200
                    } else {
                        404
                    };
                    let response = http::Response::builder()
                        .version(http::Version::HTTP_2)
                        .status(status)
                        .body(())
                        .expect("response");
                    respond
                        .send_response(response, false)
                        .expect("headers")
                        .send_data(body, true)
                        .expect("body");
                }
            }
            while response_tasks.join_next().await.is_some() {}
        });

        let sender = client.sender();
        let large_request = http::Request::builder()
            .version(http::Version::HTTP_2)
            .method("GET")
            .uri("https://home.internal/large")
            .body(())
            .expect("large request");
        let (large_response, _) = sender
            .send_request(large_request, true)
            .await
            .expect("large request send");
        let large_response = timeout(Duration::from_millis(100), large_response)
            .await
            .expect("large response timed out")
            .expect("large response");
        let mut large_body = large_response.into_body();
        let first_chunk = timeout(Duration::from_millis(100), large_body.data())
            .await
            .expect("large body timed out")
            .expect("large body ended")
            .expect("large body data");
        assert!(!first_chunk.is_empty());
        let cancellation = cancelled.notified();
        drop(large_body);
        timeout(Duration::from_millis(100), cancellation)
            .await
            .expect("server did not observe response cancellation");

        let small_request = http::Request::builder()
            .version(http::Version::HTTP_2)
            .method("GET")
            .uri("https://home.internal/small")
            .body(())
            .expect("small request");
        let (small_response, _) = sender
            .send_request(small_request, true)
            .await
            .expect("small request send");
        let small_response = timeout(Duration::from_millis(100), small_response)
            .await
            .expect("small response timed out")
            .expect("small response");
        assert_eq!(read_body(small_response.into_body()).await, Bytes::from_static(b"small response"));

        server_task.abort();
        let _ = server_task.await;
        let _ = timeout(Duration::from_millis(100), client.wait())
            .await
            .expect("client session did not terminate");
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
    async fn zero_close_timeout_is_rejected() {
        let config = Config {
            close_timeout: Duration::ZERO,
            ..Config::default()
        };
        assert!(matches!(
            config.validate(),
            Err(Error::Configuration("close timeout must be greater than zero"))
        ));
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
    async fn server_selects_exact_subprotocol_from_client_offer() {
        let config = Config::default();
        let (client, server) = websocket_handshake_with_offer(config, Some(SUBPROTOCOL)).await;
        let (_, response) = client.expect("client handshake");
        server.expect("server handshake");
        assert_eq!(
            response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok()),
            Some(SUBPROTOCOL)
        );
    }

    #[tokio::test]
    async fn server_accepts_comma_separated_subprotocol_offer() {
        let config = Config::default();
        let (client, server) = websocket_handshake_with_offer(
            config,
            Some("other-protocol, h2-over-websocket-v1, another-protocol"),
        )
        .await;
        let (_, response) = client.expect("client handshake");
        server.expect("server handshake");
        assert_eq!(
            response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok()),
            Some(SUBPROTOCOL)
        );
    }

    #[tokio::test]
    async fn server_rejects_missing_or_unequal_subprotocol_offer() {
        for offer in [None, Some("h2-over-websocket-v10")] {
            let (_, server) = websocket_handshake_with_offer(Config::default(), offer).await;
            assert!(matches!(server, Err(Error::WebSocket(_))));
        }
    }

    #[tokio::test]
    async fn default_and_custom_handshake_limits_are_installed() {
        let default_config = Config::default().websocket_config();
        assert_eq!(default_config.max_message_size, Some(DEFAULT_MAX_MESSAGE_SIZE));
        assert_eq!(default_config.max_frame_size, Some(DEFAULT_MAX_FRAME_SIZE));

        let custom = Config {
            max_message_size: 32,
            max_frame_size: 16,
            bridge_buffer_size: 16,
            ..Config::default()
        }
        .websocket_config();
        assert_eq!(custom.max_message_size, Some(32));
        assert_eq!(custom.max_frame_size, Some(16));
    }

    #[tokio::test]
    async fn invalid_config_is_rejected_before_handshake_work() {
        let config = Config {
            close_timeout: Duration::ZERO,
            ..Config::default()
        };
        assert!(matches!(
            connect_websocket("not a URL", config).await,
            Err(Error::Configuration("close timeout must be greater than zero"))
        ));
        let (_client_io, server_io) = duplex(64);
        assert!(matches!(
            accept_websocket(server_io, config).await,
            Err(Error::Configuration("close timeout must be greater than zero"))
        ));
    }

    // Tungstenite requires its concrete, non-boxed handshake error type for
    // this stateful callback, so the allowance is limited to this test.
    #[allow(clippy::result_large_err)]
    #[tokio::test]
    async fn connect_websocket_offers_and_server_selects_exact_subprotocol() {
        let config = Config::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let offered = Arc::new(std::sync::Mutex::new(None));
        let server_offered = Arc::clone(&offered);
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let websocket = accept_hdr_async_with_config(
                stream,
                move |request: &Request, mut response: Response| {
                    *server_offered.lock().expect("offered lock") = request
                        .headers()
                        .get("Sec-WebSocket-Protocol")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_owned);
                    response.headers_mut().insert(
                        "Sec-WebSocket-Protocol",
                        HeaderValue::from_static(SUBPROTOCOL),
                    );
                    Ok(response)
                },
                Some(config.websocket_config()),
            )
            .await
            .expect("server handshake");
            drop(websocket);
        });

        let (_, response) = connect_websocket(&format!("ws://{address}"), config)
            .await
            .expect("client handshake");
        assert_eq!(
            response
                .headers()
                .get("Sec-WebSocket-Protocol")
                .and_then(|value| value.to_str().ok()),
            Some(SUBPROTOCOL)
        );
        server.await.expect("server task");
        assert_eq!(
            offered
                .lock()
                .expect("offered lock")
                .as_deref(),
            Some(SUBPROTOCOL)
        );
    }

    #[tokio::test]
    async fn connect_websocket_rejects_response_without_selected_subprotocol() {
        let config = Config::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("listener address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let websocket = accept_hdr_async_with_config(
                stream,
                accept_without_subprotocol,
                Some(config.websocket_config()),
            )
            .await
            .expect("server handshake");
            drop(websocket);
        });

        let error = connect_websocket(&format!("ws://{address}"), config)
            .await
            .expect_err("missing selected subprotocol must fail");
        assert!(matches!(error, Error::WebSocket(_)), "error: {error:?}");
        server.await.expect("server task");
    }

    #[tokio::test]
    async fn text_messages_are_rejected_by_the_bridge() {
        let (client_websocket, server_websocket) = websocket_pair(Config::default()).await;
        let (_server_io, bridge_io) = duplex(64);
        let bridge = tokio::spawn(bridge(
            client_websocket,
            bridge_io,
            16,
            Config::default().close_timeout,
        ));
        let mut server_websocket = server_websocket;
        server_websocket
            .send(Message::Text("text".into()))
            .await
            .expect("send text");
        let error = timeout(Duration::from_millis(100), bridge)
            .await
            .expect("bridge task timed out")
            .expect("bridge task")
            .expect_err("text must fail");
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
