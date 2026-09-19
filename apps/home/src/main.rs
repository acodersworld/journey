use journey_h2_duplex::respond_to;
use journey_websocket::{Config, ServerSession, connect_websocket, server_session};
use tokio::task::JoinSet;
use tokio::time::{Duration, sleep};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gateway_websocket_url = std::env::var("HOME_WEBSOCKET_URL")
        .unwrap_or_else(|_| "ws://gateway:9000".to_owned());
    println!("home connecting to gateway WebSocket at {gateway_websocket_url}");

    loop {
        let config = Config::default();
        match connect_websocket(&gateway_websocket_url, config).await {
            Ok((websocket, _)) => {
                println!("home WebSocket connected to gateway");
                match server_session(websocket, config).await {
                    Ok(session) => run_server_session(session).await,
                    Err(error) => eprintln!("home HTTP/2 session failed: {error}"),
                }
            }
            Err(error) => eprintln!("home WebSocket connection failed: {error}"),
        }

        sleep(Duration::from_secs(1)).await;
    }
}

async fn run_server_session(mut session: ServerSession) {
    let mut handlers = JoinSet::new();

    loop {
        match session.accept().await {
            Ok(Some((request, respond))) => {
                handlers.spawn(async move {
                    if let Err(error) = respond_to(request, respond).await {
                        eprintln!("home request failed: {error}");
                    }
                });
            }
            Ok(None) => break,
            Err(error) => {
                eprintln!("home HTTP/2 session failed: {error}");
                break;
            }
        }
    }

    while let Some(result) = handlers.join_next().await {
        if let Err(error) = result {
            eprintln!("home request task failed: {error}");
        }
    }

    if let Err(error) = session.wait().await {
        eprintln!("home WebSocket session failed: {error}");
    }
}
