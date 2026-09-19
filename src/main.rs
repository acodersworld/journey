use journey_h2_duplex::{DUPLEX_CAPACITY, connect, request, serve};
use tokio::io::duplex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (client_io, server_io) = duplex(DUPLEX_CAPACITY);
    let server = tokio::spawn(serve(server_io));

    let (mut client, connection) = connect(client_io).await?;
    let client_driver = tokio::spawn(connection);

    let (status, body) = request(&mut client, "/ping").await?;
    println!("GET /ping -> {status} {body}");

    drop(client);
    client_driver.await??;
    server.abort();
    Ok(())
}
