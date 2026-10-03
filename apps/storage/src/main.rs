mod config;

use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use config::AppConfig;
use journey_storage::{
    serve_web_interface, FilesystemStore, FilesystemStoreConfig, Service, WebCredentials,
};
use journey_websocket::{
    connect_websocket, server_session, tungstenite::ClientRequestBuilder, Config, ServerSession,
};
use tokio::{
    net::TcpListener,
    task::JoinSet,
    time::{sleep, timeout},
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::main]
async fn main() -> AppResult<()> {
    let Some(config_path) = config_path_from_arguments()? else {
        return Ok(());
    };
    let config = AppConfig::load(Some(config_path))?;

    let management_bind = &config.management.bind;
    let management_address = management_bind.parse::<SocketAddr>().map_err(|error| {
        format!("management.bind must be a socket address (got {management_bind:?}): {error}")
    })?;
    if config.management.username.is_empty() {
        return Err("management.username must be set in the TOML configuration".into());
    }
    if config.management.password.is_empty() {
        return Err("management.password must be set in the TOML configuration".into());
    }
    if config.site_connection.websocket_secret.is_empty() {
        return Err("site_connection.websocket_secret must be set in the TOML configuration".into());
    }
    validate_window(
        "storage.initial_stream_window_size",
        config.storage.initial_stream_window_size,
    )?;
    validate_window(
        "storage.initial_connection_window_size",
        config.storage.initial_connection_window_size,
    )?;
    let credentials = WebCredentials::new(
        config.management.username.clone(),
        config.management.password.clone(),
    )
        .map_err(|error| format!("invalid storage management UI credentials: {error}"))?;
    let websocket_url = config.site_connection.websocket_url.clone();
    let secret = config.site_connection.websocket_secret.clone();
    let websocket_config = Config {
        h2_initial_stream_window_size: config.storage.initial_stream_window_size,
        h2_initial_connection_window_size: config.storage.initial_connection_window_size,
        ..Config::default()
    };
    let websocket_uri: http::Uri = websocket_url.parse().map_err(|error| {
        format!(
            "site_connection.websocket_url is not a valid WebSocket URL ({websocket_url:?}): {error}"
        )
    })?;

    let object_dir_display = config.storage.object_dir.display().to_string();
    let store = Arc::new(
        FilesystemStore::open(FilesystemStoreConfig::new(config.storage.object_dir))
            .await
            .map_err(|error| format!("could not open object store at {object_dir_display}: {error}"))?,
    );
    let management_listener = TcpListener::bind(management_address)
        .await
        .map_err(|error| format!("could not bind storage management UI to {management_bind}: {error}"))?;
    let management_store = Arc::clone(&store);
    tokio::spawn(async move {
        if let Err(error) = serve_web_interface(management_listener, management_store, credentials).await {
            eprintln!("object management UI stopped: {error}");
        }
    });
    let service = Service::new(store);
    println!("journey-storage management UI on {management_bind}");

    loop {
        let request = ClientRequestBuilder::new(websocket_uri.clone())
            .with_header("X-Journey-Storage-Secret", secret.clone());
        match timeout(Duration::from_secs(10), connect_websocket(request, websocket_config)).await {
            Ok(Ok((websocket, _))) => {
                println!("connected to journey-site storage listener");
                match server_session(websocket, websocket_config).await {
                    Ok(session) => run_storage_session(session, service.clone()).await,
                    Err(error) => eprintln!("storage HTTP/2 session failed: {error}"),
                }
            }
            Ok(Err(error)) => eprintln!("storage WebSocket connection to {websocket_url} failed: {error}"),
            Err(_) => eprintln!("storage WebSocket connection timed out"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn config_path_from_arguments() -> AppResult<Option<PathBuf>> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    match arguments.as_slice() {
        [] => std::env::var_os("JOURNEY_CONFIG")
            .map(PathBuf::from)
            .map(Some)
            .ok_or_else(|| "configuration file required; pass --config or set JOURNEY_CONFIG".into()),
        [argument] if argument == "-h" || argument == "--help" => {
            print_help();
            Ok(None)
        }
        [flag, path] if flag == "--config" || flag == "-c" => Ok(Some(PathBuf::from(path))),
        [flag] if flag == "--config" || flag == "-c" => {
            Err(format!("{flag} requires a TOML configuration file path").into())
        }
        [argument, ..] => Err(format!(
            "unexpected argument {argument:?}; use --config <PATH> or --help"
        )
        .into()),
    }
}

fn print_help() {
    println!(
        r#"journey-storage-service

Usage: journey-storage-service [--config <PATH>]

Select the TOML configuration with --config or JOURNEY_CONFIG.
See deploy/storage.toml.example for the available settings."#
    );
}

fn validate_window(name: &str, size: u32) -> AppResult<()> {
    if !(1..=0x7fff_ffff).contains(&size) {
        return Err(format!("{name} must be between 1 and 2147483647 bytes").into());
    }
    Ok(())
}

async fn run_storage_session<S: journey_storage::StoreInterface>(
    mut session: ServerSession,
    service: Service<S>,
) {
    let mut handlers = JoinSet::new();
    loop {
        tokio::select! {
            accepted = session.accept() => match accepted {
                Ok(Some((request, respond))) => {
                    let service = service.clone();
                    handlers.spawn(async move {
                        if let Err(error) = service.handle(request, respond).await {
                            eprintln!("storage request failed: {error}");
                        }
                    });
                }
                Ok(None) => break,
                Err(error) => {
                    eprintln!("storage HTTP/2 session failed: {error}");
                    break;
                }
            },
            Some(result) = handlers.join_next(), if !handlers.is_empty() => {
                if let Err(error) = result {
                    eprintln!("storage request task failed: {error}");
                }
            }
        }
    }
    while let Some(result) = handlers.join_next().await {
        if let Err(error) = result {
            eprintln!("storage request task failed: {error}");
        }
    }
    if let Err(error) = session.wait().await {
        eprintln!("storage WebSocket session ended: {error}");
    }
}
