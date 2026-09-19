use std::fmt;

use bytes::Bytes;
use http::{Request, Response, StatusCode, Version};
use tokio::io::{AsyncRead, AsyncWrite};

/// The capacity of the in-process byte stream used by the proof of concept.
pub const DUPLEX_CAPACITY: usize = 256 * 1024;

/// Errors returned by the minimal HTTP/2 server.
#[derive(Debug)]
pub enum Error {
    Http(http::Error),
    H2(h2::Error),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(error) => write!(formatter, "HTTP error: {error}"),
            Self::H2(error) => write!(formatter, "HTTP/2 error: {error}"),
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use h2::client::{self, SendRequest};
    use tokio::io::duplex;

    async fn request(client: &mut SendRequest<Bytes>, path: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .version(Version::HTTP_2)
            .method("GET")
            .uri(format!("https://home.internal{path}"))
            .body(())
            .expect("request builder");

        let (response_future, _request_body) = client
            .send_request(request, true)
            .expect("send request");
        let response = response_future.await.expect("response");
        let status = response.status();
        let mut body = response.into_body();
        let mut bytes = Vec::new();

        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("response data");
            bytes.extend_from_slice(&chunk);
            body.flow_control()
                .release_capacity(chunk.len())
                .expect("release capacity");
        }

        (status, String::from_utf8(bytes).expect("UTF-8 response"))
    }

    #[tokio::test]
    async fn completes_http2_handshake_and_ping_pong_requests() {
        let (client_io, server_io) = duplex(DUPLEX_CAPACITY);
        let server = tokio::spawn(serve(server_io));

        let (mut client, connection) = client::handshake(client_io).await.expect("client handshake");
        let client_driver = tokio::spawn(connection);

        let (status, body) = request(&mut client, "/ping").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "pong");

        let (status, body) = request(&mut client, "/pong").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ping");

        let (status, body) = request(&mut client, "/unknown").await;
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
