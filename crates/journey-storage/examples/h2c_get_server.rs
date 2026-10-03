use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use bytes::Bytes;
use clap::Parser;
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

#[derive(Parser)]
#[command(
    name = "h2c_get_server",
    about = "Example HTTP/2 storage server",
    after_help = "Window sizes accept bytes, K/KiB, KB, M/MiB, or MB. K/M are binary; KB/MB are decimal."
)]
struct Options {
    #[arg(long, value_name = "PATH")]
    storage_dir: Option<PathBuf>,
    #[arg(long, default_value = DEFAULT_BIND)]
    bind: SocketAddr,
    #[arg(long, default_value = DEFAULT_WEB_BIND)]
    web_bind: SocketAddr,
    #[arg(long = "connection-window-size", value_name = "SIZE", default_value = "256M", value_parser = parse_window_size)]
    connection_window_size: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse();
    match options.storage_dir {
        Some(root) => {
            let store = FilesystemStore::open(FilesystemStoreConfig::new(&root)).await?;
            println!("filesystem storage root: {}", root.display());
            run_server(
                store,
                false,
                options.connection_window_size,
                options.bind,
                options.web_bind,
            )
            .await
        }
        None => {
            let store = Store::new([
                (Key::new("image.jpg").unwrap(), object("image/jpeg", IMAGE)?),
                (Key::new("video.mp4").unwrap(), object("video/mp4", VIDEO)?),
            ])
            .map_err(std::io::Error::other)?;
            run_server(
                store,
                true,
                options.connection_window_size,
                options.bind,
                options.web_bind,
            )
            .await
        }
    }
}

fn parse_window_size(value: &str) -> Result<u32, String> {
    let digit_end = value
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(value.len());
    if digit_end == 0 {
        return Err("size must start with a byte count".to_owned());
    }
    let count = value[..digit_end]
        .parse::<u64>()
        .map_err(|error| error.to_string())?;
    let multiplier = match value[digit_end..].to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1024,
        "kb" => 1000,
        "m" | "mib" => 1024 * 1024,
        "mb" => 1000 * 1000,
        suffix => return Err(format!("unsupported size suffix: {suffix}")),
    };
    let bytes = count
        .checked_mul(multiplier)
        .ok_or_else(|| "size is too large".to_owned())?;
    if bytes > 0x7fff_ffff {
        return Err("size must be at most 2147483647 bytes".to_owned());
    }
    Ok(bytes as u32)
}

async fn run_server<S: StoreInterface + Sync + Send>(
    store: S,
    seeded_fixtures: bool,
    initial_connection_window_size: u32,
    address: SocketAddr,
    web_address: SocketAddr,
) -> Result<(), Box<dyn Error>> {
    let store = Arc::new(store);
    let listener = TcpListener::bind(address).await.map_err(|error| {
        std::io::Error::new(error.kind(), format!("failed to bind h2c listener at {address}: {error}"))
    })?;
    let bound_address = listener.local_addr()?;
    let web_store = Arc::clone(&store);
    let service = Arc::new(Service::new(store));

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
    println!("Upload with a server-generated SHA-256 key (the response includes Object-Name):");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Object-Key-Mode: sha256' -H 'Content-Type: image/jpeg' --data-binary @crates/journey-storage/examples/assets/image.jpg -D - http://{bound_address}/objects");
    println!("Upload into a logical folder with a server-generated key:");
    println!("  curl --http2-prior-knowledge -X PUT -H 'Object-Key-Mode: sha256' -H 'Content-Type: image/jpeg' --data-binary @crates/journey-storage/examples/assets/image.jpg -D - http://{bound_address}/objects/photos/");
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
                        if let Err(error) = serve_connection(stream, service, initial_connection_window_size).await {
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
    initial_connection_window_size: u32,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut builder = server::Builder::new();
    builder
        .max_concurrent_streams(MAX_CONCURRENT_STREAMS)
        .max_header_list_size(MAX_HEADER_LIST_SIZE)
        .initial_connection_window_size(initial_connection_window_size);
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
