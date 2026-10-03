use std::{
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use journey_websocket::{accept_websocket_with_authorizer, Config};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::Semaphore,
    time::timeout,
};

use crate::storage::WebSocketStorageClient;

const SECRET_HEADER: &str = "x-journey-storage-secret";
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn serve(
    listener: TcpListener,
    secret: String,
    storage: WebSocketStorageClient,
    initial_window_size: u32,
    initial_connection_window_size: u32,
) -> std::io::Result<()> {
    let active_session = Arc::new(Semaphore::new(1));
    loop {
        let (stream, peer) = listener.accept().await?;
        let secret = secret.clone();
        let storage = storage.clone();
        let active_session = Arc::clone(&active_session);
        tokio::spawn(async move {
            let Ok(permit) = active_session.try_acquire_owned() else {
                eprintln!("rejected additional storage WebSocket session from {peer}");
                return;
            };
            let expected_secret = secret.into_bytes();
            let handshake = timeout(
                HANDSHAKE_TIMEOUT,
                accept_websocket_with_authorizer(stream, Config {
                    h2_initial_stream_window_size: initial_window_size,
                    h2_initial_connection_window_size: initial_connection_window_size,
                    ..Config::default()
                }, move |request| {
                    let supplied = request.headers().get(SECRET_HEADER);
                    let authorized = supplied
                        .and_then(|value| value.to_str().ok())
                        .filter(|value| value.len() <= 4096)
                        .is_some_and(|value| {
                            expected_secret.as_slice().ct_eq(value.as_bytes()).into()
                        });
                    if authorized && request.uri().path() == "/internal/storage" {
                        Ok(())
                    } else {
                        Err(http::Response::builder()
                            .status(http::StatusCode::UNAUTHORIZED)
                            .body(Some("storage authentication failed\n".to_owned()))
                            .expect("valid unauthorized response"))
                    }
                }),
            )
            .await;
            let websocket = match handshake {
                Ok(Ok(websocket)) => websocket,
                Ok(Err(error)) => {
                    eprintln!("storage WebSocket handshake from {peer} failed: {error}");
                    return;
                }
                Err(_) => {
                    eprintln!("storage WebSocket handshake from {peer} timed out");
                    return;
                }
            };
            println!("storage WebSocket session accepted from {peer}");
            if let Err(error) = storage.serve_connection(websocket).await {
                eprintln!("storage WebSocket session from {peer} ended: {error}");
            }
            drop(permit);
        });
    }
}

pub fn parse_bind_address(bind: &str) -> Result<SocketAddr, Box<dyn std::error::Error + Send + Sync>> {
    bind.parse::<SocketAddr>().map_err(|error| {
        std::io::Error::other(format!(
            "storage.websocket_bind must be a socket address (got {bind:?}): {error}"
        ))
        .into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use journey_websocket::{
        connect_websocket, tungstenite::ClientRequestBuilder, Config,
    };

    #[tokio::test]
    async fn rejects_a_storage_connection_with_the_wrong_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let storage = WebSocketStorageClient::new(512 * 1024, 4 * 1024 * 1024);
        let server = tokio::spawn(serve(
            listener,
            "expected-secret".to_owned(),
            storage,
            512 * 1024,
            4 * 1024 * 1024,
        ));
        let request = ClientRequestBuilder::new(
            format!("ws://{address}/internal/storage").parse().unwrap(),
        )
        .with_header("X-Journey-Storage-Secret", "incorrect-secret");

        let result = connect_websocket(request, Config::default()).await;
        assert!(result.is_err());

        server.abort();
    }
}
