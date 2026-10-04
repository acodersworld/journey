mod config;
mod shutdown;

use std::{
    error::Error,
    io,
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
    let mut shutdown = shutdown::listen();

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
        FilesystemStore::open(
            FilesystemStoreConfig::new(config.storage.object_dir)
                .with_thumbnail_time_ms(config.storage.thumbnail_time_ms),
        )
            .await
            .map_err(|error| format!("could not open object store at {object_dir_display}: {error}"))?,
    );
    let management_listener = TcpListener::bind(management_address)
        .await
        .map_err(|error| format!("could not bind storage management UI to {management_bind}: {error}"))?;
    let mut management_tasks = JoinSet::new();
    let management_store = Arc::clone(&store);
    management_tasks.spawn(async move {
        serve_web_interface(management_listener, management_store, credentials).await
    });
    let service = Service::new(store);
    println!("journey-storage management UI on {management_bind}");

    let mut result: AppResult<()> = loop {
        let request = ClientRequestBuilder::new(websocket_uri.clone())
            .with_header("X-Journey-Storage-Secret", secret.clone());
        let connection = tokio::select! {
            biased;
            signal = shutdown::requested(&mut shutdown) => {
                break shutdown_result(signal);
            }
            Some(task_result) = management_tasks.join_next(), if !management_tasks.is_empty() => {
                break Err(management_ui_task_failure(task_result));
            }
            connection = timeout(Duration::from_secs(10), connect_websocket(request, websocket_config)) => connection,
        };
        match connection {
            Ok(Ok((websocket, _))) => {
                println!("connected to journey-site storage listener");
                let session = tokio::select! {
                    biased;
                    signal = shutdown::requested(&mut shutdown) => {
                        break shutdown_result(signal);
                    }
                    Some(task_result) = management_tasks.join_next(), if !management_tasks.is_empty() => {
                        break Err(management_ui_task_failure(task_result));
                    }
                    result = server_session(websocket, websocket_config) => result,
                };
                match session {
                    Ok(session) => {
                        tokio::select! {
                            biased;
                            signal = shutdown::requested(&mut shutdown) => {
                                break shutdown_result(signal);
                            }
                            Some(task_result) = management_tasks.join_next(), if !management_tasks.is_empty() => {
                                break Err(management_ui_task_failure(task_result));
                            }
                            _ = run_storage_session(session, service.clone()) => {}
                        }
                    }
                    Err(error) => eprintln!("storage HTTP/2 session failed: {error}"),
                }
            }
            Ok(Err(error)) => eprintln!("storage WebSocket connection to {websocket_url} failed: {error}"),
            Err(_) => eprintln!("storage WebSocket connection timed out"),
        }
        tokio::select! {
            biased;
            signal = shutdown::requested(&mut shutdown) => {
                break shutdown_result(signal);
            }
            Some(task_result) = management_tasks.join_next(), if !management_tasks.is_empty() => {
                break Err(management_ui_task_failure(task_result));
            }
            _ = sleep(Duration::from_secs(1)) => {}
        }
    };

    management_tasks.abort_all();
    while let Some(task_result) = management_tasks.join_next().await {
        match task_result {
            Err(error) if error.is_cancelled() => {}
            Ok(Ok(())) if result.is_ok() => {
                result = Err("storage management UI stopped unexpectedly".into());
            }
            Ok(Err(error)) if result.is_ok() => {
                result = Err(format!("storage management UI failed: {error}").into());
            }
            Err(error) if result.is_ok() => {
                result = Err(management_ui_task_failure(Err(error)));
            }
            _ => {}
        }
    }
    result
}

fn shutdown_result(signal: Result<(), String>) -> AppResult<()> {
    match signal {
        Ok(()) => Ok(()),
        Err(error) => Err(io::Error::other(error).into()),
    }
}

fn management_ui_task_failure(
    task_result: Result<io::Result<()>, tokio::task::JoinError>,
) -> Box<dyn Error + Send + Sync> {
    let message = match task_result {
        Ok(Ok(())) => "storage management UI stopped unexpectedly".to_owned(),
        Ok(Err(error)) => format!("storage management UI failed: {error}"),
        Err(error) => format!("storage management UI task failed: {error}"),
    };
    io::Error::other(message).into()
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
