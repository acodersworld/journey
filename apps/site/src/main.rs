mod auth;
mod db;
mod import;
mod storage;
mod web;

use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use clap::{Args, CommandFactory, Parser, Subcommand};
use db::Database;
use storage::H2cStorageClient;
use tokio::net::TcpListener;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Parser)]
#[command(
    name = "journey-site",
    about = "Post backend and manifest importer",
    version,
    after_help = "Environment:\n  JOURNEY_SITE_DB                        SQLite file (default: journey-site.sqlite3)\n  JOURNEY_SITE_BIND                      HTTP listen address (default: 127.0.0.1:8080)\n  JOURNEY_STORAGE_H2C                    loopback h2c address (default: 127.0.0.1:8081)\n  JOURNEY_SITE_PUBLIC_ORIGIN             site's public origin (required off loopback)\n  JOURNEY_SITE_SESSION_TTL_SECONDS       absolute session lifetime (default: 604800)\n  JOURNEY_SITE_MAX_MEDIA_UPLOAD_BYTES    per-file media limit (default: 2147483648)\n\nWindow sizes accept bytes, K/KiB, KB, M/MiB, or MB (defaults: 512K and 4M). K/M are binary; KB/MB are decimal."
)]
struct Cli {
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
    #[command(flatten)]
    storage_windows: StorageWindowArgs,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long)]
    allow_insecure_cookies: bool,
    #[command(flatten)]
    storage_windows: StorageWindowArgs,
}

#[derive(Args, Clone, Copy)]
struct StorageWindowArgs {
    #[arg(long = "window-size", value_name = "SIZE", default_value = "512K", value_parser = parse_window_size)]
    initial_window_size: u32,
    #[arg(long = "connection-window-size", value_name = "SIZE", default_value = "4M", value_parser = parse_window_size)]
    initial_connection_window_size: u32,
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
    },
    List,
    Revoke { link_id: String },
}

#[tokio::main]
async fn main() -> AppResult<()> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    let database = Database::new(database_path());
    match command {
        Commands::Import(options) => {
            let prepared = import::prepare_manifest(&options.manifest).await?;
            let storage = connect_storage(options.storage_windows).await?;
            import::apply_import(prepared, &database, &storage).await
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
                ShareLinkCommands::Create { published_post_id } => {
                    if published_post_id <= 0 {
                        return Err("published post ID must be positive".into());
                    }
                    let bind = std::env::var("JOURNEY_SITE_BIND")
                        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
                    let bind_address = bind.parse::<SocketAddr>()?;
                    let origin = web::share_link_origin(bind_address).map_err(std::io::Error::other)?;
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
            let bind = std::env::var("JOURNEY_SITE_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
            let bind_address = bind.parse::<SocketAddr>()?;
            if options.allow_insecure_cookies && !bind_address.ip().is_loopback() {
                return Err("--allow-insecure-cookies is only permitted when binding to loopback".into());
            }
            let security = web::SiteSecurity::from_env(bind_address, options.allow_insecure_cookies)
                .map_err(std::io::Error::other)?;
            database.initialize().await.map_err(std::io::Error::other)?;
            let storage = connect_storage(options.storage_windows).await?;
            let listener = TcpListener::bind(&bind).await?;
            println!("journey-site HTTP listener on {}", listener.local_addr()?);
            axum::serve(
                listener,
                web::router(web::state_with_security(database, storage, security))
                    .into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await?;
            Ok(())
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

fn database_path() -> PathBuf {
    std::env::var_os("JOURNEY_SITE_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("journey-site.sqlite3"))
}

async fn connect_storage(windows: StorageWindowArgs) -> AppResult<H2cStorageClient> {
    let address = std::env::var("JOURNEY_STORAGE_H2C")
        .unwrap_or_else(|_| "127.0.0.1:8081".to_owned())
        .parse::<SocketAddr>()?;
    H2cStorageClient::connect(
        address,
        windows.initial_window_size,
        windows.initial_connection_window_size,
    )
    .await
}
