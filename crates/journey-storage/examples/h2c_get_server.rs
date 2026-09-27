use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use bytes::Bytes;
use h2::server;
use journey_storage::{
    serve_web_interface, ContentType, FilesystemStore, FilesystemStoreConfig, Key, Object,
    Service, Store, StoreInterface, WebCredentials,
};
use http::HeaderValue;
use tokio::{
    net::{TcpListener, TcpStream},
    task::JoinSet,
};

const DEFAULT_BIND: &str = "127.0.0.1:8081";
const DEFAULT_WEB_BIND: &str = "0.0.0.0:8082";
const MAX_CONCURRENT_STREAMS: u32 = 8;
const MAX_HEADER_LIST_SIZE: u32 = 16 * 1024;

const IMAGE: &[u8] = include_bytes!("assets/image.jpg");
const VIDEO: &[u8] = include_bytes!("assets/video.mp4");

enum StorageSelection {
    Help,
    InMemory,
    Filesystem(PathBuf),
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    match storage_selection()? {
        StorageSelection::Help => Ok(()),
        StorageSelection::Filesystem(root) => {
            let store = FilesystemStore::open(FilesystemStoreConfig::new(&root)).await?;
            println!("filesystem storage root: {}", root.display());
            run_server(store, false).await
        }
        StorageSelection::InMemory => {
            let store = Store::new([
                (Key::new("image.jpg").unwrap(), object("image/jpeg", IMAGE)?),
                (Key::new("video.mp4").unwrap(), object("video/mp4", VIDEO)?),
            ])
            .map_err(std::io::Error::other)?;
            run_server(store, true).await
        }
    }
}

fn storage_selection() -> Result<StorageSelection, Box<dyn Error>> {
    let mut args = std::env::args_os().skip(1);
    let Some(option) = args.next() else {
        return Ok(StorageSelection::InMemory);
    };
    if option == "--help" && args.next().is_none() {
        println!("Usage: h2c_get_server [--storage-dir PATH]");
        return Ok(StorageSelection::Help);
    }
    if option != "--storage-dir" {
        return Err("Usage: h2c_get_server [--storage-dir PATH]".into());
    }
    let root = args.next().ok_or("--storage-dir requires a path")?;
    if args.next().is_some() {
        return Err("Usage: h2c_get_server [--storage-dir PATH]".into());
    }
    Ok(StorageSelection::Filesystem(root.into()))
}

async fn run_server<S: StoreInterface + Sync + Send>(
    store: S,
    seeded_fixtures: bool,
) -> Result<(), Box<dyn Error>> {
    let store = Arc::new(store);
    let bind = std::env::var("JOURNEY_STORAGE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_owned());
    let address: SocketAddr = bind.parse()?;
    let listener = TcpListener::bind(address).await.map_err(|error| {
        std::io::Error::new(error.kind(), format!("failed to bind h2c listener at {address}: {error}"))
    })?;
    let bound_address = listener.local_addr()?;
    let web_store = Arc::clone(&store);
    let service = Arc::new(Service::new(store));

    let web_bind = std::env::var("JOURNEY_STORAGE_WEB_BIND")
        .unwrap_or_else(|_| DEFAULT_WEB_BIND.to_owned());
    let web_address: SocketAddr = web_bind.parse()?;
    let web_listener = TcpListener::bind(web_address).await.map_err(|error| {
        std::io::Error::new(error.kind(), format!("failed to bind web listener at {web_address}: {error}"))
    })?;
    let web_bound_address = web_listener.local_addr()?;
    let web_credentials = WebCredentials::new("user", "pass")?;
    let mut web_server = tokio::spawn(async move {
        serve_web_interface(web_listener, web_store, web_credentials).await
    });

    println!("journey-storage HTTP/2 listening on http://{bound_address}");
    println!("journey-storage web interface listening on http://{web_bound_address}");
    println!("  example web credentials: user / pass");
    if seeded_fixtures {
        println!("  http://{bound_address}/objects/image.jpg");
        println!("  http://{bound_address}/objects/video.mp4");
    }
    println!("List stored objects:");
    println!("  curl --http2-prior-knowledge --get --data-urlencode 'prefix=' http://{bound_address}/objects");
    if seeded_fixtures {
        println!("Inspect object metadata:");
        println!("  curl --http2-prior-knowledge --head http://{bound_address}/objects/image.jpg");
    }
    println!("Upload and download an object with:");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Content-Type: image/jpeg' --data-binary @crates/journey-storage/examples/assets/image.jpg http://{bound_address}/objects/uploaded.jpg");
    println!("  curl --http2-prior-knowledge http://{bound_address}/objects/uploaded.jpg --output downloaded.jpg");
    println!("  cmp crates/journey-storage/examples/assets/image.jpg downloaded.jpg");
    if seeded_fixtures {
        println!("Get the first 100 bytes of image.jpg:");
        println!("  curl --http2-prior-knowledge -H 'Range: bytes=0-99' -D - http://{bound_address}/objects/image.jpg --output first-100-bytes.bin");
        println!("  expected: HTTP/2 206, Content-Range: bytes 0-99/{}, 100-byte payload", IMAGE.len());
    }
    println!("Replace it with a different type and verify the replacement with:");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Content-Type: video/mp4' --data-binary @crates/journey-storage/examples/assets/video.mp4 http://{bound_address}/objects/uploaded.jpg");
    println!("  curl --http2-prior-knowledge -D - http://{bound_address}/objects/uploaded.jpg --output downloaded.mp4");
    println!("  cmp crates/journey-storage/examples/assets/video.mp4 downloaded.mp4");
    println!("Delete it with:");
    println!("  curl --http2-prior-knowledge -X DELETE http://{bound_address}/objects/uploaded.jpg");

    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            result = &mut web_server => {
                match result {
                    Ok(Ok(())) => return Err("storage web interface stopped unexpectedly".into()),
                    Ok(Err(error)) => return Err(error.into()),
                    Err(error) => return Err(error.into()),
                }
            }
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
    content_type: &str,
    contents: &'static [u8],
) -> Result<Object, std::io::Error> {
    let content_type = content_type
        .parse::<HeaderValue>()
        .map_err(std::io::Error::other)?;
    let content_type = ContentType::try_from_header(&content_type)
        .map_err(std::io::Error::other)?;
    Ok(Object::new(
        content_type,
        Bytes::from_static(contents),
    ))
}

async fn serve_connection<S: StoreInterface + Sync + Send>(
    stream: TcpStream,
    service: Arc<Service<S>>,
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
