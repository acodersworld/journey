use std::{fmt, sync::Arc};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use h2::client::{self, SendRequest};
use http::{Request, Response, StatusCode, Version};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

/// The capacity of the in-process byte stream used by the proof of concept.
pub const DUPLEX_CAPACITY: usize = 256 * 1024;

/// Errors returned by the minimal HTTP/2 client/server helpers.
#[derive(Debug)]
pub enum Error {
    Http(http::Error),
    H2(h2::Error),
    Io(std::io::Error),
    WebSocket(Box<tokio_tungstenite::tungstenite::Error>),
    InvalidResponse(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "HTTP error: {error}"),
            Self::H2(error) => write!(formatter, "HTTP/2 error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::WebSocket(error) => write!(formatter, "WebSocket error: {error}"),
            Self::InvalidResponse(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

impl From<http::Error> for Error {
    fn from(error: http::Error) -> Self {
        Self::Http(error)
    }
}

impl From<h2::Error> for Error {
    fn from(error: h2::Error) -> Self {
        Self::H2(error)
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

/// Starts the HTTP/2 server side on any compatible byte stream.
///
/// The server responds to `GET /ping` with `pong` and `GET /pong` with `ping`.
/// All other requests receive `404 Not Found`.
pub async fn serve<T>(io: T) -> Result<(), Error>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut connection = h2::server::handshake(io).await?;

    while let Some(result) = connection.accept().await {
        let (request, respond) = result?;
        tokio::spawn(async move {
            if let Err(error) = respond_to(request, respond).await {
                eprintln!("HTTP/2 request failed: {error}");
            }
        });
    }

    Ok(())
}

async fn respond_to(
    request: Request<h2::RecvStream>,
    mut respond: h2::server::SendResponse<Bytes>,
) -> Result<(), Error> {
    let response_body = match (request.method(), request.uri().path()) {
        (&http::Method::GET, "/ping") => Some("pong"),
        (&http::Method::GET, "/pong") => Some("ping"),
        _ => None,
    };

    let status = if response_body.is_some() {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    };

    let body = response_body.unwrap_or("not found");
    let response = Response::builder()
        .version(Version::HTTP_2)
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("content-length", body.len())
        .body(())?;

    let mut send_stream = respond.send_response(response, false)?;
    send_stream.send_data(Bytes::from_static(body.as_bytes()), true)?;
    Ok(())
}

/// Performs the client-side HTTP/2 handshake and returns the request handle.
///
/// The returned connection future must be spawned or otherwise polled for
/// requests and responses to make progress.
pub async fn connect<T>(
    io: T,
) -> Result<
    (
        SendRequest<Bytes>,
        h2::client::Connection<T, Bytes>,
    ),
    Error,
>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    Ok(client::handshake(io).await?)
}

/// Sends one ping/pong request and collects its small response body.
pub async fn request(
    client: &mut SendRequest<Bytes>,
    path: &str,
) -> Result<(StatusCode, String), Error> {
    let request = Request::builder()
        .version(Version::HTTP_2)
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

/// Pumps a WebSocket and a byte stream in both directions.
///
/// HTTP/2 sees only the byte stream. WebSocket message boundaries are not
/// exposed to it. Binary messages carry stream bytes; control frames remain
/// WebSocket control traffic.
pub async fn bridge_websocket<S, T>(
    websocket: WebSocketStream<S>,
    io: T,
) -> Result<(), Error>
where
    S: AsyncRead + AsyncWrite + Unpin,
    T: AsyncRead + AsyncWrite + Unpin,
{
    let (mut websocket_write, mut websocket_read) = websocket.split();
    let (mut io_read, mut io_write) = tokio::io::split(io);
    let mut buffer = [0_u8; 16 * 1024];

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
                        return Err(Error::InvalidResponse("text WebSocket messages are not supported"));
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

/// A reusable HTTP/2 client carried by one persistent WebSocket connection.
#[derive(Clone)]
pub struct PersistentClient {
    client: Arc<SendRequest<Bytes>>,
}

impl PersistentClient {
    /// Sends one request over the existing inner HTTP/2 connection.
    pub async fn request(&self, path: &str) -> Result<(StatusCode, String), Error> {
        let mut client = self.client.as_ref().clone();
        request(&mut client, path).await
    }
}

/// Opens one persistent WebSocket and the inner HTTP/2 connection it carries.
///
/// The returned client can issue multiple requests without reconnecting. The
/// driver task owns both connection pumps and stops the remaining pump when
/// either side terminates.
pub async fn connect_over_websocket(
    websocket_url: &str,
) -> Result<PersistentClient, Error> {
    let (websocket, _) = tokio_tungstenite::connect_async(websocket_url).await?;
    let (client_io, bridge_io) = tokio::io::duplex(DUPLEX_CAPACITY);
    let bridge = tokio::spawn(bridge_websocket(websocket, bridge_io));

    let (client, connection) = match connect(client_io).await {
        Ok(connection) => connection,
        Err(error) => {
            bridge.abort();
            return Err(error);
        }
    };

    tokio::spawn(async move {
        let mut bridge = bridge;
        tokio::select! {
            result = connection => {
                if let Err(error) = result {
                    eprintln!("inner HTTP/2 connection stopped: {error}");
                }
                bridge.abort();
            }
            result = &mut bridge => match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("WebSocket bridge stopped: {error}"),
                Err(error) => eprintln!("WebSocket bridge task stopped: {error}"),
            }
        }
    });

    Ok(PersistentClient {
        client: Arc::new(client),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn completes_http2_handshake_and_ping_pong_requests() {
        let (client_io, server_io) = duplex(DUPLEX_CAPACITY);
        let server = tokio::spawn(serve(server_io));

        let (mut client, connection) = connect(client_io).await.expect("client handshake");
        let client_driver = tokio::spawn(connection);

        let (status, body) = request(&mut client, "/ping").await.expect("ping request");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "pong");

        let (status, body) = request(&mut client, "/pong").await.expect("pong request");
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ping");

        let (status, body) = request(&mut client, "/unknown")
            .await
            .expect("unknown route request");
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, "not found");

        drop(client);
        client_driver
            .await
            .expect("client driver task")
            .expect("client connection");
        server.abort();
    }
}
