use std::{
    error::Error,
    net::SocketAddr,
    sync::Arc,
};

use bytes::Bytes;
use h2::server;
use http::HeaderValue;
use journey_storage::{Service, Key, Object, Store};
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const DEFAULT_BIND: &str = "127.0.0.1:8081";
const MAX_CONCURRENT_STREAMS: u32 = 8;
const MAX_HEADER_LIST_SIZE: u32 = 16 * 1024;

const IMAGE: &[u8] = include_bytes!("assets/image.jpg");
const VIDEO: &[u8] = include_bytes!("assets/video.mp4");

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let bind = std::env::var("JOURNEY_STORAGE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());
    let address: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(address).await?;
    let bound_address = listener.local_addr()?;
    let store = Store::new([
        object("image.jpg", "image/jpeg", IMAGE)?,
        object("video.mp4", "video/mp4", VIDEO)?,
    ])
    .map_err(std::io::Error::other)?;
    let service = Arc::new(Service::new(store));

    println!("journey-storage listening on http://{bound_address}");
    println!("  http://{bound_address}/objects/image.jpg");
    println!("  http://{bound_address}/objects/video.mp4");
    println!("Upload and download an object with:");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Content-Type: image/jpeg' --data-binary @crates/journey-storage/examples/assets/image.jpg http://{bound_address}/objects/uploaded.jpg");
    println!("  curl --http2-prior-knowledge http://{bound_address}/objects/uploaded.jpg --output downloaded.jpg");
    println!("  cmp crates/journey-storage/examples/assets/image.jpg downloaded.jpg");
    println!("Replace it with a different type and verify the replacement with:");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Content-Type: video/mp4' --data-binary @crates/journey-storage/examples/assets/video.mp4 http://{bound_address}/objects/uploaded.jpg");
    println!("  curl --http2-prior-knowledge -D - http://{bound_address}/objects/uploaded.jpg --output downloaded.mp4");
    println!("  cmp crates/journey-storage/examples/assets/video.mp4 downloaded.mp4");

    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, peer)) => {
                    let service = Arc::clone(&service);
                    connections.spawn(async move {
                        if let Err(error) = serve_connection(stream, service).await {
                            eprintln!("HTTP/2 connection from {peer} failed: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("TCP accept failed: {error}"),
            },
            result = connections.join_next(), if !connections.is_empty() => {
                if let Some(Err(error)) = result {
                    eprintln!("HTTP/2 connection task failed: {error}");
                }
            }
        }
    }
}

fn object(
    key: &str,
    content_type: &str,
    contents: &'static [u8],
) -> Result<Object, std::io::Error> {
    let key = Key::new(key).map_err(std::io::Error::other)?;
    Ok(Object::new(
        key,
        content_type.parse::<HeaderValue>().map_err(std::io::Error::other)?,
        Bytes::from_static(contents),
    ))
}

async fn serve_connection(
    stream: TcpStream,
    service: Arc<Service>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut builder = server::Builder::new();
    builder
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(MAX_HEADER_LIST_SIZE);
    let mut connection = builder.handshake::<_, Bytes>(stream).await?;
    let mut requests = JoinSet::new();

    loop {
        tokio::select! {
            accepted = connection.accept() => match accepted {
                Some(Ok((request, respond))) => {
                    let service = Arc::clone(&service);
                    requests.spawn(async move {
                        if let Err(error) = service.handle(request, respond).await {
                            eprintln!("HTTP/2 request failed: {error}");
                        }
                    });
                }
                Some(Err(error)) => {
                    eprintln!("HTTP/2 request stream failed: {error}");
                    break;
                }
                None => break,
            },
            result = requests.join_next(), if !requests.is_empty() => {
                if let Some(Err(error)) = result {
                    eprintln!("HTTP/2 request task failed: {error}");
                }
            }
        }
    }

    while let Some(result) = requests.join_next().await {
        if let Err(error) = result {
            eprintln!("HTTP/2 request task failed: {error}");
        }
    }
    Ok(())
}
