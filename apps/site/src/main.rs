mod auth;
mod config;
mod control;
mod db;
mod import;
mod shutdown;
mod storage;
mod storage_websocket;
mod web;

use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, CommandFactory, Parser, Subcommand};
use db::Database;
use storage::{H2cStorageClient, SiteStorageClient, WebSocketStorageClient};
use tokio::{
    net::TcpListener,
    task::JoinSet,
};

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Parser)]
#[command(
    name = "journey-site",
    about = "Post backend and manifest importer",
    version,
    after_help = "Configuration:\n  --config <PATH> or JOURNEY_CONFIG selects a TOML configuration file.\n  See deploy/site.toml.example for the available settings."
)]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Import(ImportArgs),
    Db {
        #[command(subcommand)]
        command: DbCommands,
    },
    Users {
        #[command(subcommand)]
        command: UserCommands,
    },
    #[command(name = "share-links")]
    ShareLinks {
        #[command(subcommand)]
        command: ShareLinkCommands,
    },
    Serve(ServeArgs),
}

#[derive(Args)]
struct ImportArgs {
    manifest: PathBuf,
    #[arg(long)]
    via_running_site: bool,
    #[command(flatten)]
    storage_windows: StorageWindowArgs,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    allow_insecure_lan_http: bool,
    #[command(flatten)]
    storage_windows: StorageWindowArgs,
}

#[derive(Args, Clone, Copy, Default)]
struct StorageWindowArgs {
    #[arg(long = "window-size", value_name = "SIZE", value_parser = parse_window_size)]
    initial_window_size: Option<u32>,
    #[arg(long = "connection-window-size", value_name = "SIZE", value_parser = parse_window_size)]
    initial_connection_window_size: Option<u32>,
}

#[derive(Subcommand)]
enum DbCommands {
    Tables,
    Schema,
    Posts,
    Post { id: i64 },
}

#[derive(Subcommand)]
enum UserCommands {
    Create {
        username: String,
        #[arg(value_parser = ["read", "write", "admin"])]
        role: String,
    },
    List,
    Password { username: String },
    Disable { username: String },
    Enable { username: String },
}

#[derive(Subcommand)]
enum ShareLinkCommands {
    Lifetime { seconds: Option<u64> },
    Create {
        #[arg(value_name = "PUBLISHED-POST-ID")]
        published_post_id: i64,
        #[arg(long)]
        allow_insecure_lan_http: bool,
    },
    List,
    Revoke { link_id: String },
}

#[tokio::main]
async fn main() -> AppResult<()> {
    let cli = Cli::parse();
    let Cli { config: config_path, command } = cli;
    let Some(command) = command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    let config = config::AppConfig::load(config_path)?;
    let database = Database::new(config.site.database_path.clone());
    match command {
        Commands::Import(options) => {
            if options.via_running_site {
                let result = control::request_import(&options.manifest, &config.site.control_socket).await?;
                print_import_result(result);
                return Ok(());
            }
            match config.storage.transport.as_str() {
                "h2c" => {}
                "websocket" => {
                    return Err(
                        concat!(
                            "storage.transport is \"websocket\"; run this import with ",
                            "--via-running-site while journey-site is serving. ",
                            "The manifest path must be absolute and visible to the site process"
                        )
                        .into(),
                    );
                }
                transport => {
                    return Err(format!("unsupported storage.transport value: {transport}").into());
                }
            }
            let prepared = import::prepare_manifest(&options.manifest).await?;
            let windows = resolve_storage_windows(options.storage_windows, &config.storage)?;
            let address = parse_socket_address("storage.h2c_address", &config.storage.h2c_address)?;
            let storage = connect_storage(windows, address).await?;
            let result = import::apply_import(prepared, &database, &storage).await?;
            print_import_result(result);
            Ok(())
        }
        Commands::Db { command } => {
            database.initialize().await.map_err(std::io::Error::other)?;
            match command {
                DbCommands::Tables => {
                    for table in database.tables().await.map_err(std::io::Error::other)? {
                        println!("{table}");
                    }
                    Ok(())
                }
                DbCommands::Schema => {
                    for (name, statement) in database.schema().await.map_err(std::io::Error::other)? {
                        println!("-- {name}\n{statement};");
                    }
                    Ok(())
                }
                DbCommands::Posts => {
                    let posts = database.all_summaries().await.map_err(std::io::Error::other)?;
                    println!("{}", serde_json::to_string_pretty(&posts)?);
                    Ok(())
                }
                DbCommands::Post { id } => {
                    match database.post(id).await.map_err(std::io::Error::other)? {
                        Some(post) => println!("{}", serde_json::to_string_pretty(&post)?),
                        None => return Err(format!("published post {id} was not found").into()),
                    }
                    Ok(())
                }
            }
        }
        Commands::Users { command } => {
            database.initialize().await.map_err(std::io::Error::other)?;
            match command {
                UserCommands::Create { username, role } => {
                    let role = db::AccountRole::parse(&role)?;
                    auth::validate_username(&username)?;
                    let password = read_new_password()?;
                    let password_hash = auth::hash_password(&password).map_err(std::io::Error::other)?;
                    database
                        .create_account(username.clone(), role, password_hash)
                        .await
                        .map_err(std::io::Error::other)?;
                    println!("created {role} account {username}");
                    Ok(())
                }
                UserCommands::List => {
                    let accounts = database.accounts().await.map_err(std::io::Error::other)?;
                    for account in accounts {
                        println!("{}\t{}\t{}", account.role, account.username, if account.enabled { "enabled" } else { "disabled" });
                    }
                    Ok(())
                }
                UserCommands::Password { username } => {
                    auth::validate_username(&username)?;
                    let password = read_new_password()?;
                    let password_hash = auth::hash_password(&password).map_err(std::io::Error::other)?;
                    database
                        .change_password(username.clone(), password_hash)
                        .await
                        .map_err(std::io::Error::other)?;
                    println!("changed password for {username}; existing sessions were revoked");
                    Ok(())
                }
                UserCommands::Disable { username } => {
                    set_account_enabled(&database, username, false).await
                }
                UserCommands::Enable { username } => {
                    set_account_enabled(&database, username, true).await
                }
            }
        }
        Commands::ShareLinks { command } => {
            database.initialize().await.map_err(std::io::Error::other)?;
            match command {
                ShareLinkCommands::Lifetime { seconds: Some(seconds) } => {
                    database
                        .set_share_link_lifetime(Duration::from_secs(seconds))
                        .await
                        .map_err(std::io::Error::other)?;
                    println!("share link lifetime set to {seconds} seconds");
                    Ok(())
                }
                ShareLinkCommands::Lifetime { seconds: None } => {
                    println!("{}", database.share_link_lifetime().await.map_err(std::io::Error::other)?.as_secs());
                    Ok(())
                }
                ShareLinkCommands::Create { published_post_id, allow_insecure_lan_http } => {
                    if published_post_id <= 0 {
                        return Err("published post ID must be positive".into());
                    }
                    let bind_address = parse_socket_address("site.bind", &config.site.bind)?;
                    let origin = web::share_link_origin_with_insecure_lan_http(
                        bind_address,
                        config.site.public_origin.as_deref(),
                        allow_insecure_lan_http || config.site.allow_insecure_lan_http,
                    )
                    .map_err(std::io::Error::other)?;
                    let link_id = auth::new_share_link_id();
                    let secret = auth::new_share_link_secret();
                    let created_at = unix_time();
                    match database
                        .create_share_link(
                            link_id.clone(),
                            published_post_id,
                            auth::session_token_digest(&secret),
                            created_at,
                            db::ShareAccess::Admin,
                        )
                        .await
                        .map_err(std::io::Error::other)?
                    {
                        Some(link) => {
                            println!("{}", link_url(&origin, &link.id, &secret));
                            Ok(())
                        }
                        None => Err(format!("published post {published_post_id} was not found").into()),
                    }
                }
                ShareLinkCommands::List => {
                    let now = unix_time();
                    println!("Link ID\tPost ID\tExpires (epoch seconds)\tExpires (UTC)\tRevocation status");
                    for link in database.share_links().await.map_err(std::io::Error::other)? {
                        let status = match link.revoked_at {
                            Some(revoked_at) => format!("revoked at {}", revoked_at.as_secs()),
                            None if link.expires_at <= now => "expired, not revoked".to_owned(),
                            None => "not revoked".to_owned(),
                        };
                        println!(
                            "{}\t{}\t{}\t{}\t{}",
                            link.id,
                            link.post_id,
                            link.expires_at.as_secs(),
                            link.expires_at_utc.as_deref().unwrap_or("out of range"),
                            status,
                        );
                    }
                    Ok(())
                }
                ShareLinkCommands::Revoke { link_id } => {
                    if !database
                        .revoke_share_link(link_id.clone(), unix_time(), db::ShareAccess::Admin)
                        .await
                        .map_err(std::io::Error::other)?
                    {
                        return Err(format!("share link {link_id} was not found").into());
                    }
                    println!("revoked share link {link_id}");
                    Ok(())
                }
            }
        }
        Commands::Serve(options) => {
            let mut shutdown = shutdown::listen();
            let bind = config.site.bind.clone();
            let bind_address = parse_socket_address("site.bind", &bind)?;
            let allow_insecure_lan_http =
                options.allow_insecure_lan_http || config.site.allow_insecure_lan_http;
            let security = web::SiteSecurity::from_config(
                bind_address,
                config.site.public_origin.as_deref(),
                config.site.session_ttl_seconds,
                config.site.max_media_upload_bytes,
                config.site.whatsapp_preview_image_ttl_seconds,
                allow_insecure_lan_http,
            )
            .map_err(std::io::Error::other)?;
            let windows = resolve_storage_windows(options.storage_windows, &config.storage)?;
            let storage_transport = match config.storage.transport.as_str() {
                "h2c" => ConfiguredStorageTransport::H2c(parse_socket_address(
                    "storage.h2c_address",
                    &config.storage.h2c_address,
                )?),
                "websocket" => {
                    if config.storage.websocket_secret.is_empty() {
                        return Err("storage.websocket_secret is required when storage.transport is websocket".into());
                    }
                    ConfiguredStorageTransport::WebSocket(
                        storage_websocket::parse_bind_address(&config.storage.websocket_bind)?,
                    )
                }
                transport => return Err(format!("unsupported storage.transport value: {transport}").into()),
            };
            let database_path = &config.site.database_path;
            database.initialize().await.map_err(|error| {
                format!("could not initialize site database at {}: {error}", database_path.display())
            })?;
            let (storage, websocket_listener) = match storage_transport {
                ConfiguredStorageTransport::H2c(address) => {
                    let storage = tokio::select! {
                        result = connect_storage(windows, address) => {
                            SiteStorageClient::H2c(result?)
                        }
                        result = shutdown::requested(&mut shutdown) => {
                            result.map_err(std::io::Error::other)?;
                            return Ok(());
                        }
                    };
                    (storage, None)
                }
                ConfiguredStorageTransport::WebSocket(websocket_address) => {
                    let storage = WebSocketStorageClient::new(
                        windows.initial_window_size,
                        windows.initial_connection_window_size,
                    );
                    let websocket_listener = TcpListener::bind(websocket_address)
                        .await
                        .map_err(|error| format!("could not bind storage WebSocket listener at {websocket_address}: {error}"))?;
                    (SiteStorageClient::WebSocket(storage), Some(websocket_listener))
                }
            };
            if let Some(listener) = &websocket_listener {
                println!("journey-site storage WebSocket listener on {}", listener.local_addr()?);
            }
            let listener = TcpListener::bind(&bind)
                .await
                .map_err(|error| format!("could not bind site HTTP listener at {bind}: {error}"))?;
            let listener_address = listener.local_addr()?;
            let control_listener = control::bind(&config.site.control_socket)
                .await
                .map_err(|error| format!("could not bind private import control socket: {error}"))?;
            let mut control_socket = control::BoundSocketPath::new(config.site.control_socket.clone());
            println!("journey-site import control socket ready");
            println!("journey-site HTTP listener on {listener_address}");

            let mut listener_tasks = JoinSet::new();
            if let Some(websocket_listener) = websocket_listener {
                let websocket_storage = match &storage {
                    SiteStorageClient::WebSocket(storage) => storage.clone(),
                    SiteStorageClient::H2c(_) => unreachable!(),
                };
                let secret = config.storage.websocket_secret.clone();
                let websocket_shutdown = shutdown.clone();
                listener_tasks.spawn(async move {
                    storage_websocket::serve(
                        websocket_listener,
                        secret,
                        websocket_storage,
                        windows.initial_window_size,
                        windows.initial_connection_window_size,
                        websocket_shutdown,
                    )
                    .await
                });
            }
            let control_database = database.clone();
            let control_storage = storage.clone();
            let control_shutdown = shutdown.clone();
            listener_tasks.spawn(async move {
                control::serve(control_listener, control_database, control_storage, control_shutdown).await
            });

            let app_state = web::state_with_security(database, storage.clone(), security);
            let cleanup_state = app_state.clone();
            let mut cleanup_shutdown = shutdown.clone();
            listener_tasks.spawn(async move {
                let mut cleanup_interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + Duration::from_secs(60),
                    Duration::from_secs(60),
                );
                loop {
                    tokio::select! {
                        signal = shutdown::requested(&mut cleanup_shutdown) => {
                            signal.map_err(std::io::Error::other)?;
                            return Ok(());
                        }
                        _ = cleanup_interval.tick() => {
                            web::whatsapp_preview::cleanup_expired(&cleanup_state);
                        }
                    }
                }
            });

            let shutdown_storage = storage.clone();
            let mut http_server = Box::pin(async move {
                axum::serve(
                    listener,
                    web::router(app_state)
                        .into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
            });
            let mut result: AppResult<()> = loop {
                tokio::select! {
                    biased;
                    signal = shutdown::requested(&mut shutdown) => {
                        break shutdown_result(signal);
                    }
                    result = &mut http_server => {
                        break Err(format!("journey-site HTTP listener stopped unexpectedly: {result:?}").into());
                    }
                    Some(task_result) = listener_tasks.join_next(), if !listener_tasks.is_empty() => {
                        break match task_result {
                            Ok(Ok(())) => Err("journey-site background listener stopped unexpectedly".into()),
                            Ok(Err(error)) => Err(format!("journey-site background listener failed: {error}").into()),
                            Err(error) => Err(format!("journey-site background listener task failed: {error}").into()),
                        };
                    }
                }
            };
            drop(http_server);
            if result.is_err() {
                listener_tasks.abort_all();
            }
            while let Some(task_result) = listener_tasks.join_next().await {
                match task_result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) if result.is_ok() => {
                        result = Err(format!("journey-site background listener failed: {error}").into());
                    }
                    Err(error) if error.is_cancelled() => {}
                    Err(error) if result.is_ok() => {
                        result = Err(format!("journey-site background listener task failed: {error}").into());
                    }
                    _ => {}
                }
            }
            shutdown_storage.close().await;
            if let Err(error) = control_socket.remove().await {
                if result.is_ok() {
                    result = Err(format!("could not remove site control socket: {error}").into());
                }
            }
            result
        }
    }
}

fn shutdown_result(signal: Result<(), String>) -> AppResult<()> {
    match signal {
        Ok(()) => Ok(()),
        Err(error) => Err(std::io::Error::other(error).into()),
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

async fn set_account_enabled(database: &Database, username: String, enabled: bool) -> AppResult<()> {
    auth::validate_username(&username)?;
    database
        .set_account_enabled(username.clone(), enabled)
        .await
        .map_err(std::io::Error::other)?;
    println!("{} account {username}", if enabled { "enabled" } else { "disabled" });
    Ok(())
}

fn unix_time() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

fn link_url(origin: &str, link_id: &str, secret: &str) -> String {
    format!("{origin}/share/{link_id}/{secret}")
}

fn print_import_result(result: import::ImportResult) {
    println!(
        "import complete: {} post(s) replaced, {} account(s) in manifest, {} unique media file(s), {} new account(s)",
        result.posts_replaced,
        result.imported_accounts,
        result.unique_media_files,
        result.accounts_added,
    );
}

fn read_new_password() -> AppResult<String> {
    let password = read_password("Password: ")?;
    let confirmation = read_password("Confirm password: ")?;
    if password != confirmation {
        return Err("passwords do not match".into());
    }
    if password.chars().count() < 12 {
        return Err("passwords must contain at least 12 characters".into());
    }
    if password.len() > 1024 {
        return Err("passwords must be at most 1024 bytes".into());
    }
    Ok(password)
}

#[cfg(unix)]
fn read_password(prompt: &str) -> std::io::Result<String> {
    use std::io::{self, Write};

    print!("{prompt}");
    io::stdout().flush()?;
    let descriptor = libc::STDIN_FILENO;
    let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(descriptor, original.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let original = unsafe { original.assume_init() };
    let mut hidden = original;
    hidden.c_lflag &= !libc::ECHO;
    if unsafe { libc::tcsetattr(descriptor, libc::TCSAFLUSH, &hidden) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let _restore = TerminalEchoGuard { descriptor, settings: original };
    let mut password = String::new();
    io::stdin().read_line(&mut password)?;
    drop(_restore);
    println!();
    while matches!(password.as_bytes().last(), Some(b'\n' | b'\r')) {
        password.pop();
    }
    Ok(password)
}

#[cfg(unix)]
struct TerminalEchoGuard {
    descriptor: libc::c_int,
    settings: libc::termios,
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.descriptor, libc::TCSAFLUSH, &self.settings);
        }
    }
}

#[cfg(not(unix))]
fn read_password(_prompt: &str) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "interactive password input without echo is not supported on this platform",
    ))
}

#[derive(Clone, Copy)]
struct StorageWindows {
    initial_window_size: u32,
    initial_connection_window_size: u32,
}

enum ConfiguredStorageTransport {
    H2c(SocketAddr),
    WebSocket(SocketAddr),
}

fn resolve_storage_windows(
    args: StorageWindowArgs,
    config: &config::StorageSettings,
) -> AppResult<StorageWindows> {
    let windows = StorageWindows {
        initial_window_size: args.initial_window_size.unwrap_or(config.initial_window_size),
        initial_connection_window_size: args
            .initial_connection_window_size
            .unwrap_or(config.initial_connection_window_size),
    };
    for (name, size) in [
        ("storage.initial_window_size", windows.initial_window_size),
        (
            "storage.initial_connection_window_size",
            windows.initial_connection_window_size,
        ),
    ] {
        if !(1..=0x7fff_ffff).contains(&size) {
            return Err(format!("{name} must be between 1 and 2147483647 bytes").into());
        }
    }
    Ok(windows)
}

fn parse_socket_address(name: &str, value: &str) -> AppResult<SocketAddr> {
    value
        .parse::<SocketAddr>()
        .map_err(|error| format!("{name} must be a socket address (got {value:?}): {error}").into())
}

async fn connect_storage(windows: StorageWindows, address: SocketAddr) -> AppResult<H2cStorageClient> {
    H2cStorageClient::connect(
        address,
        windows.initial_window_size,
        windows.initial_connection_window_size,
    )
    .await
    .map_err(|error| format!("could not connect to h2c storage service at {address}: {error}").into())
}
