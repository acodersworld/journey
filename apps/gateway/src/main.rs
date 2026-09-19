use std::sync::Arc;

use axum::{
    body::Body,
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use bytes::Bytes;
use futures_util::stream::{self, Stream};
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

fn stream_body(body: h2::RecvStream) -> impl Stream<Item = Result<Bytes, std::io::Error>> {
    stream::unfold(Some(body), |body| async move {
        let mut body = body?;
        match body.data().await {
            Some(Ok(chunk)) => {
                let result = body
                    .flow_control()
                    .release_capacity(chunk.len())
                    .map(|()| chunk)
                    .map_err(|error| std::io::Error::other(error.to_string()));
                let next = if result.is_ok() { Some(body) } else { None };
                Some((result, next))
            }
            Some(Err(error)) => Some((Err(std::io::Error::other(error.to_string())), None)),
            None => None,
        }
    })
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
