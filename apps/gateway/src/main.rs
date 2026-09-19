use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use journey_websocket::{Client, Config, connect_client};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_tungstenite::accept_async;

#[derive(Clone)]
struct GatewayState {
    home_client: Arc<Mutex<Client>>,
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
    let home_client = state.home_client.lock().await;
    match home_client.request("/ping").await {
        Ok((StatusCode::OK, body)) if body == "pong" => {
            (StatusCode::OK, "ok\n").into_response()
        }
        Ok(_) => (StatusCode::BAD_GATEWAY, "home health check failed\n").into_response(),
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

async fn proxy(client: &Arc<Mutex<Client>>, path: &str) -> Response {
    let client = client.lock().await;
    match client.request(path).await {
        Ok((status, body)) => (status, body).into_response(),
        Err(error) => {
            eprintln!("gateway request {path} failed: {error}");
            (StatusCode::BAD_GATEWAY, "home connection failed\n").into_response()
        }
    }
}

async fn accept_home(
    listener: &TcpListener,
) -> Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let (stream, peer) = listener.accept().await?;
        match accept_async(stream).await {
            Ok(websocket) => match connect_client(websocket, Config::default()).await {
                Ok(client) => return Ok(client),
                Err(error) => eprintln!("home HTTP/2 connection from {peer} failed: {error}"),
            },
            Err(error) => eprintln!("home WebSocket handshake from {peer} failed: {error}"),
        }
    }
}

async fn accept_reconnections(listener: TcpListener, home_client: Arc<Mutex<Client>>) {
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
