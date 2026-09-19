//! WebSocket transport for the inner Journey HTTP/2 connection.
//!
//! This crate adapts a WebSocket into the ordered byte stream used by the
//! inner HTTP/2 connection. It deliberately keeps WebSocket framing and
//! control traffic here so application protocol code can remain unaware of
//! the outer transport.

use std::sync::Arc;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use h2::client::{self, SendRequest};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const DEFAULT_DUPLEX_CAPACITY: usize = 256 * 1024;
const DEFAULT_BRIDGE_BUFFER_SIZE: usize = 16 * 1024;

/// Runtime limits for the WebSocket-to-byte-stream adapter.
///
/// These values apply to each WebSocket connection. `Default` preserves the
/// prototype's original settings: a 256 KiB duplex capacity and a 16 KiB
/// bridge read buffer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    /// Maximum number of bytes buffered between the WebSocket bridge and the
    /// application protocol.
    ///
    /// Increasing this can absorb larger bursts and reduce transport stalls,
    /// but increases the memory reserved for each connection and allows more
    /// data to sit ahead of a slow application. Decreasing it applies
    /// backpressure sooner and reduces per-connection buffering, but may
    /// reduce throughput when the application and network alternate between
    /// short bursts of activity.
    pub duplex_capacity: usize,
    /// Number of bytes read from the application stream for each outbound
    /// binary WebSocket message.
    ///
    /// Increasing this usually means fewer WebSocket messages and fewer
    /// write operations, at the cost of a larger temporary allocation per
    /// connection and coarser delivery to the peer. Decreasing it produces
    /// smaller, more frequent messages with finer-grained backpressure, but
    /// adds framing and scheduling overhead. This setting controls outbound
    /// chunking; it does not impose a maximum size on inbound WebSocket
    /// messages supplied by the WebSocket implementation.
    pub bridge_buffer_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            duplex_capacity: DEFAULT_DUPLEX_CAPACITY,
            bridge_buffer_size: DEFAULT_BRIDGE_BUFFER_SIZE,
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
        Ok(())
    }
}

/// Errors produced while connecting or moving bytes through the WebSocket
/// transport.
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
    /// The peer sent a WebSocket message this transport does not support.
    Protocol(&'static str),
    /// The adapter configuration is invalid.
    Configuration(&'static str),
    /// The inner HTTP/2 response body was not valid UTF-8.
    InvalidResponse(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http2(error) => write!(formatter, "HTTP/2 error: {error}"),
            Self::Http(error) => write!(formatter, "HTTP error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::WebSocket(error) => write!(formatter, "WebSocket error: {error}"),
            Self::Protocol(message) => formatter.write_str(message),
            Self::Configuration(message) => formatter.write_str(message),
            Self::InvalidResponse(message) => formatter.write_str(message),
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

async fn connect_h2<T>(
    io: T,
) -> Result<(SendRequest<Bytes>, h2::client::Connection<T, Bytes>), Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Ok(client::handshake(io).await?)
}

async fn request(
    client: &mut SendRequest<Bytes>,
    path: &str,
) -> Result<(http::StatusCode, String), Error> {
    let request = http::Request::builder()
        .version(http::Version::HTTP_2)
        .method("GET")
        .uri(format!("https://home.internal{path}"))
        .body(())?;

    let (response_future, _request_body) = client.send_request(request, true)?;
    let response = response_future.await?;
    let status = response.status();
    let mut body = response.into_body();
    let mut bytes = Vec::new();

    while let Some(chunk) = body.data().await {
        let chunk = chunk?;
        bytes.extend_from_slice(&chunk);
        body.flow_control().release_capacity(chunk.len())?;
    }

    let body = String::from_utf8(bytes)
        .map_err(|_| Error::InvalidResponse("response body was not UTF-8"))?;
    Ok((status, body))
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

impl<E: std::fmt::Display> std::fmt::Display for ServeError<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
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

/// Pumps a WebSocket and a byte stream in both directions.
///
/// HTTP/2 sees only the byte stream. WebSocket message boundaries are not
/// exposed to it. Binary messages carry stream bytes; control frames remain
/// WebSocket control traffic.
///
/// The bridge applies backpressure when the byte-stream side cannot accept
/// more data, and it ends when either side closes. Text messages are rejected
/// because the inner protocol is binary.
///
/// # Errors
///
/// Returns an error when a WebSocket operation, byte-stream operation, or
/// protocol validation step fails.
async fn bridge<S, T>(
    websocket: WebSocketStream<S>,
    io: T,
    buffer_size: usize,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (mut io_read, mut io_write) = tokio::io::split(io);
    let mut buffer = vec![0_u8; buffer_size];

    loop {
        tokio::select! {
            message = websocket_read.next() => {
                match message {
                    Some(Ok(Message::Binary(data))) => io_write.write_all(&data).await?,
                    Some(Ok(Message::Ping(data))) => {
                        websocket_write.send(Message::Pong(data)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let _ = websocket_write.send(Message::Close(frame)).await;
                        return Ok(());
                    }
                    Some(Ok(Message::Text(_))) => {
                        return Err(Error::Protocol("text WebSocket messages are not supported"));
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error.into()),
                    None => return Ok(()),
                }
            }
            read = io_read.read(&mut buffer) => {
                let read = read?;
                if read == 0 {
                    let _ = websocket_write.send(Message::Close(None)).await;
                    return Ok(());
                }
                websocket_write.send(Message::Binary(Bytes::copy_from_slice(&buffer[..read]))).await?;
            }
        }
    }
}

/// Serves an application protocol over an established WebSocket.
///
/// The WebSocket crate owns the bounded duplex stream and the byte bridge. The
/// caller owns the protocol server that runs over the other end of that
/// stream, so this function does not assume that the application is using
/// HTTP/2.
///
/// The function returns when either the transport or the application server
/// stops.
///
/// # Errors
///
/// Returns [`ServeError::Transport`] for a WebSocket or bridge failure and
/// [`ServeError::Application`] for an error returned by `start_server`.
pub async fn serve<S, F, Fut, E>(
    websocket: WebSocketStream<S>,
    config: Config,
    start_server: F,
) -> Result<(), ServeError<E>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: FnOnce(DuplexStream) -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    config
        .validate()
        .map_err(ServeError::Transport)?;
    let (server_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let server = start_server(server_io);
    let bridge = bridge(websocket, bridge_io, config.bridge_buffer_size);

    tokio::select! {
        result = server => result.map_err(ServeError::Application),
        result = bridge => result.map_err(ServeError::Transport),
    }
}

/// A reusable HTTP/2 client carried by one persistent WebSocket connection.
///
/// Cloning this value creates another handle to the same inner HTTP/2
/// connection; it does not open another WebSocket. Dropping all handles does
/// not explicitly send a close frame, but the connection driver will observe
/// the dropped request handles and terminate the transport.
#[derive(Clone)]
pub struct Client {
    client: Arc<SendRequest<Bytes>>,
}

impl Client {
    /// Sends one request over the existing inner HTTP/2 connection.
    ///
    /// The request uses the HTTP/2 client established by [`connect`]. The
    /// response body is collected because the current prototype only carries
    /// the small `/ping` and `/pong` responses.
    ///
    /// # Errors
    ///
    /// Returns an error if the persistent connection has failed or the inner
    /// HTTP/2 request cannot be completed.
    pub async fn request(
        &self,
        path: &str,
    ) -> Result<(http::StatusCode, String), Error> {
        let mut client = self.client.as_ref().clone();
        request(&mut client, path).await
    }
}

/// Connects the inner HTTP/2 client over an already-upgraded WebSocket.
///
/// This function returns only after both handshakes have completed. The
/// WebSocket and HTTP/2 driver continue running in the background, and every
/// [`Client::request`] call reuses them until the peer or driver
/// closes the connection.
///
/// # Errors
///
/// Returns an error if the inner HTTP/2 handshake fails.
pub async fn connect_client<S>(
    websocket: WebSocketStream<S>,
    config: Config,
) -> Result<Client, Error>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    config.validate()?;
    let (client_io, bridge_io) = tokio::io::duplex(config.duplex_capacity);
    let bridge_task = tokio::spawn(bridge(websocket, bridge_io, config.bridge_buffer_size));

    let (client, connection) = match connect_h2(client_io).await {
        Ok(connection) => connection,
        Err(error) => {
            bridge_task.abort();
            return Err(error);
        }
    };

    tokio::spawn(async move {
        let mut bridge_task = bridge_task;
        tokio::select! {
            result = connection => {
                if let Err(error) = result {
                    eprintln!("inner HTTP/2 connection stopped: {error}");
                }
                bridge_task.abort();
            }
            result = &mut bridge_task => match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("WebSocket bridge stopped: {error}"),
                Err(error) => eprintln!("WebSocket bridge task stopped: {error}"),
            }
        }
    });

    Ok(Client {
        client: Arc::new(client),
    })
}

/// Opens an outbound WebSocket and establishes the inner HTTP/2 client it
/// carries.
///
/// This is the outbound client-side convenience form of [`connect_client`]. The returned
/// [`Client`] keeps the WebSocket and inner HTTP/2 connection alive for
/// multiple requests.
pub async fn connect(websocket_url: &str, config: Config) -> Result<Client, Error> {
    let (websocket, _) = tokio_tungstenite::connect_async(websocket_url).await?;
    connect_client(websocket, config).await
}
