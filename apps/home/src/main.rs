use journey_h2_duplex::serve;
use journey_websocket::{Config, connect_websocket, serve as serve_websocket};
use tokio::time::{Duration, sleep};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let gateway_websocket_url = std::env::var("HOME_WEBSOCKET_URL")
        .unwrap_or_else(|_| "ws://gateway:9000".to_owned());
    println!("home connecting to gateway WebSocket at {gateway_websocket_url}");

    loop {
        match connect_websocket(&gateway_websocket_url, Config::default()).await {
            Ok((websocket, _)) => {
                println!("home WebSocket connected to gateway");
                if let Err(error) = serve_websocket(websocket, Config::default(), serve).await {
                    eprintln!("home WebSocket connection failed: {error}");
                }
            }
            Err(error) => eprintln!("home WebSocket connection failed: {error}"),
        }

        sleep(Duration::from_secs(1)).await;
    }
}
