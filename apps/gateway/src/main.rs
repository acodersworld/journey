use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures_util::stream::Stream;
use journey_websocket::{ClientSession, Config, Error, accept_websocket, connect_client};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

#[derive(Clone)]
struct GatewayState {
    home_client: Arc<Mutex<ClientSession>>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bind_address = std::env::var("GATEWAY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let websocket_bind_address = std::env::var("GATEWAY_WEBSOCKET_BIND")
        .unwrap_or_else(|_| "0.0.0.0:9000".to_owned());
    let websocket_listener = TcpListener::bind(&websocket_bind_address).await?;
    println!("gateway WebSocket listener on {websocket_bind_address}");

    let home_client = accept_home(&websocket_listener).await?;
    let home_client = Arc::new(Mutex::new(home_client));
    tokio::spawn(accept_reconnections(
        websocket_listener,
        Arc::clone(&home_client),
    ));
    let state = GatewayState { home_client };
    let app = Router::new()
        .route("/health", get(health))
        .route("/ping", get(ping))
        .route("/pong", get(pong))
        .with_state(state);
    let listener = TcpListener::bind(&bind_address).await?;

    println!("gateway HTTP listener on {bind_address}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(state): State<GatewayState>) -> Response {
    match request(&state.home_client, "/ping").await {
        Ok((status, body)) => {
            if status == StatusCode::OK && collect_body(body).await.as_deref() == Some(b"pong") {
                (StatusCode::OK, "ok\n").into_response()
            } else {
                (StatusCode::BAD_GATEWAY, "home health check failed\n").into_response()
            }
        }
        Err(error) => {
            eprintln!("gateway health check failed: {error}");
            (StatusCode::BAD_GATEWAY, "home connection failed\n").into_response()
        }
    }
}

async fn ping(State(state): State<GatewayState>) -> Response {
    proxy(&state.home_client, "/ping").await
}

async fn pong(State(state): State<GatewayState>) -> Response {
    proxy(&state.home_client, "/pong").await
}

async fn proxy(client: &Arc<Mutex<ClientSession>>, path: &str) -> Response {
    match request(client, path).await {
        Ok((status, body)) => (status, Body::from_stream(stream_body(body))).into_response(),
        Err(error) => {
            eprintln!("gateway request {path} failed: {error}");
            (StatusCode::BAD_GATEWAY, "home connection failed\n").into_response()
        }
    }
}

async fn request(
    client: &Arc<Mutex<ClientSession>>,
    path: &str,
) -> Result<(StatusCode, h2::RecvStream), Error> {
    let sender = client.lock().await.sender();
    let request = http::Request::builder()
        .version(http::Version::HTTP_2)
        .method(http::Method::GET)
        .uri(format!("https://home.internal{path}"))
        .body(())?;
    let (response, _request_body) = sender.send_request(request, true).await?;
    let response = response.await?;
    Ok((response.status(), response.into_body()))
}

async fn collect_body(mut body: h2::RecvStream) -> Option<Vec<u8>> {
    let mut result = Vec::new();
    while let Some(chunk) = body.data().await {
        let chunk = chunk.ok()?;
        body.flow_control().release_capacity(chunk.len()).ok()?;
        result.extend_from_slice(&chunk);
    }
    Some(result)
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

async fn accept_home(
    listener: &TcpListener,
) -> Result<ClientSession, Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (stream, peer) = listener.accept().await?;
        match accept_websocket(stream, Config::default()).await {
            Ok(websocket) => match connect_client(websocket, Config::default()).await {
                Ok(client) => return Ok(client),
                Err(error) => eprintln!("home HTTP/2 connection from {peer} failed: {error}"),
            },
            Err(error) => eprintln!("home WebSocket handshake from {peer} failed: {error}"),
        }
    }
}

async fn accept_reconnections(listener: TcpListener, home_client: Arc<Mutex<ClientSession>>) {
    loop {
        match accept_home(&listener).await {
            Ok(client) => {
                *home_client.lock().await = client;
                println!("home WebSocket connection replaced");
            }
            Err(error) => {
                eprintln!("home WebSocket listener failed: {error}");
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{future::poll_fn, StreamExt};
    use h2::{client, server};
    use http::{Request, Version};
    use tokio::{
        io::duplex,
        sync::oneshot,
        time::{timeout, Duration},
    };

    #[tokio::test]
    async fn response_capacity_is_released_on_the_next_poll() {
        let (client_io, server_io) = duplex(1024 * 1024);
        let (sent, sent_received) = oneshot::channel();
        let (check, check_received) = oneshot::channel();
        let (before_release, before_release_received) = oneshot::channel();
        let (after_poll_signal, after_poll_signal_received) = oneshot::channel();
        let (after_poll, after_poll_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connection = server::handshake(server_io).await.expect("server handshake");
            let Some(Ok((_, mut respond))) = connection.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let response = http::Response::builder()
                    .version(Version::HTTP_2)
                    .status(200)
                    .body(())
                    .expect("response");
                let mut send = respond.send_response(response, false).expect("headers");
                send.reserve_capacity(4);
                poll_fn(|context| send.poll_capacity(context))
                    .await
                    .expect("initial capacity")
                    .expect("initial capacity result");
                send.send_data(Bytes::from_static(b"data"), false)
                    .expect("first data");
                sent.send(()).expect("sent signal receiver");

                check_received.await.expect("capacity check signal");
                send.reserve_capacity(1);
                let released_before_next_poll = timeout(
                    Duration::from_millis(100),
                    poll_fn(|context| send.poll_capacity(context)),
                )
                .await
                .is_ok();
                before_release
                    .send(released_before_next_poll)
                    .expect("before-release receiver");

                after_poll_signal_received
                    .await
                    .expect("after-poll signal");
                let released_after_next_poll = timeout(
                    Duration::from_millis(100),
                    poll_fn(|context| send.poll_capacity(context)),
                )
                .await
                .is_ok();
                after_poll
                    .send(released_after_next_poll)
                    .expect("after-poll receiver");
            });

            while connection.accept().await.is_some() {}
        });

        let mut builder = client::Builder::new();
        builder.initial_window_size(4);
        let (mut client, connection) = builder
            .handshake::<_, Bytes>(client_io)
            .await
            .expect("client handshake");
        let client_driver = tokio::spawn(connection);
        let request = Request::builder()
            .version(Version::HTTP_2)
            .method("GET")
            .uri("https://gateway.internal/stream")
            .body(())
            .expect("request");
        let (response, _) = client.send_request(request, true).expect("request send");
        let response = response.await.expect("response");
        let mut body = H2BodyStream::new(response.into_body());

        sent_received.await.expect("sent signal");
        let first = timeout(Duration::from_millis(100), body.next())
            .await
            .expect("first chunk timed out")
            .expect("first chunk ended")
            .expect("first chunk error");
        assert_eq!(first, Bytes::from_static(b"data"));

        check.send(()).expect("capacity check receiver");
        assert!(!before_release_received
            .await
            .expect("before-release signal"));

        assert!(timeout(Duration::from_millis(100), body.next())
            .await
            .is_err());
        after_poll_signal
            .send(())
            .expect("after-poll receiver");
        assert!(after_poll_received
            .await
            .expect("after-poll signal"));

        drop(body);
        drop(client);
        client_driver.abort();
        server.abort();
    }

    #[tokio::test]
    async fn dropping_response_body_resets_inner_stream() {
        let (client_io, server_io) = duplex(1024 * 1024);
        let (sent, sent_received) = oneshot::channel();
        let (drop_body, drop_body_received) = oneshot::channel();
        let (reset, reset_received) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connection = server::handshake(server_io).await.expect("server handshake");
            let Some(Ok((_, mut respond))) = connection.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let response = http::Response::builder()
                    .version(Version::HTTP_2)
                    .status(200)
                    .body(())
                    .expect("response");
                let mut send = respond.send_response(response, false).expect("headers");
                send.reserve_capacity(4);
                poll_fn(|context| send.poll_capacity(context))
                    .await
                    .expect("initial capacity")
                    .expect("initial capacity result");
                send.send_data(Bytes::from_static(b"data"), false)
                    .expect("first data");
                sent.send(()).expect("sent signal receiver");

                drop_body_received
                    .await
                    .expect("drop-body signal");
                send.reserve_capacity(1);
                let observed_reset = timeout(
                    Duration::from_millis(100),
                    poll_fn(|context| send.poll_capacity(context)),
                )
                .await
                .map(|result| matches!(result, Some(Err(_)) | None))
                .unwrap_or(false);
                reset.send(observed_reset).expect("reset receiver");
            });

            while connection.accept().await.is_some() {}
        });

        let mut builder = client::Builder::new();
        builder.initial_window_size(4);
        let (mut client, connection) = builder
            .handshake::<_, Bytes>(client_io)
            .await
            .expect("client handshake");
        let client_driver = tokio::spawn(connection);
        let request = Request::builder()
            .version(Version::HTTP_2)
            .method("GET")
            .uri("https://gateway.internal/stream")
            .body(())
            .expect("request");
        let (response, _) = client.send_request(request, true).expect("request send");
        let response = response.await.expect("response");
        let mut body = H2BodyStream::new(response.into_body());
        sent_received.await.expect("sent signal");
        let first = timeout(Duration::from_millis(100), body.next())
            .await
            .expect("first chunk timed out")
            .expect("first chunk ended")
            .expect("first chunk error");
        assert_eq!(first, Bytes::from_static(b"data"));

        drop(body);
        drop_body.send(()).expect("drop-body receiver");
        assert!(reset_received.await.expect("reset signal"));

        drop(client);
        client_driver.abort();
        server.abort();
    }
}
