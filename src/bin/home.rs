use journey_h2_duplex::{DUPLEX_CAPACITY, bridge_websocket, serve};
use tokio::io::duplex;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::accept_async;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let bind_address = std::env::var("HOME_BIND").unwrap_or_else(|_| "0.0.0.0:9000".to_owned());
    let listener = TcpListener::bind(&bind_address).await?;
    println!("home WebSocket listener on {bind_address}");

    loop {
        let (stream, peer) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(error) = handle_connection(stream).await {
                eprintln!("home connection from {peer} failed: {error}");
            }
        });
    }
}

async fn handle_connection(stream: TcpStream) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let websocket = accept_async(stream).await?;
    let (h2_io, bridge_io) = duplex(DUPLEX_CAPACITY);
    let server = serve(h2_io);
    let bridge = bridge_websocket(websocket, bridge_io);

    tokio::select! {
        result = server => result?,
        result = bridge => result?,
    }

    Ok(())
}
