use axum::{Router, extract::State, http::StatusCode, response::{IntoResponse, Response}, routing::get};
use journey_h2_duplex::{PersistentClient, connect_over_websocket};
use tokio::net::TcpListener;
use tokio::time::{Duration, sleep};

#[derive(Clone)]
struct GatewayState {
    home_client: PersistentClient,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind_address = std::env::var("GATEWAY_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_owned());
    let home_websocket_url = std::env::var("HOME_WEBSOCKET_URL")
        .unwrap_or_else(|_| "ws://home:9000".to_owned());
    let home_client = connect_home(&home_websocket_url).await?;
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
    match state.home_client.request("/ping").await {
        Ok((StatusCode::OK, body)) if body == "pong" => (StatusCode::OK, "ok\n").into_response(),
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

async fn proxy(client: &PersistentClient, path: &str) -> Response {
    match client.request(path).await {
        Ok((status, body)) => (status, body).into_response(),
        Err(error) => {
            eprintln!("gateway request {path} failed: {error}");
            (StatusCode::BAD_GATEWAY, "home connection failed\n").into_response()
        }
    }
}

async fn connect_home(
    websocket_url: &str,
) -> Result<PersistentClient, Box<dyn std::error::Error>> {
    loop {
        match connect_over_websocket(websocket_url).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                eprintln!("waiting for home WebSocket at {websocket_url}: {error}");
                sleep(Duration::from_secs(1)).await;
            }
        }
    }
}
