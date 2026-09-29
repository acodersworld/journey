mod auth;
mod db;
mod import;
mod storage;
mod web;

use std::{
    error::Error,
    net::SocketAddr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use db::Database;
use storage::H2cStorageClient;
use tokio::net::TcpListener;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const HELP: &str = "\
journey-site — read-only post backend and manifest importer

Usage:
  journey-site serve [--allow-insecure-cookies]
  journey-site import <manifest.json>
  journey-site db tables
  journey-site db schema
  journey-site db posts
  journey-site db post <id>
  journey-site users create <username> <owner|reader>
  journey-site users list
  journey-site users password <username>
  journey-site users disable <username>
  journey-site users enable <username>
  journey-site share-links lifetime [seconds]
  journey-site share-links create <published-post-id>
  journey-site share-links list
  journey-site share-links revoke <link-id>
  journey-site --help

Environment:
  JOURNEY_SITE_DB        SQLite file (default: journey-site.sqlite3)
  JOURNEY_SITE_BIND     HTTP listen address (default: 127.0.0.1:8080)
  JOURNEY_STORAGE_H2C   loopback h2c address (default: 127.0.0.1:8081)
  JOURNEY_SITE_PUBLIC_ORIGIN      site's public origin (required off loopback)
  JOURNEY_SITE_SESSION_TTL_SECONDS absolute session lifetime (default: 604800)
";
const DB_HELP: &str = "\
Usage:
  journey-site db tables
  journey-site db schema
  journey-site db posts
  journey-site db post <id>
";
const USERS_HELP: &str = "\
Usage:
  journey-site users create <username> <owner|reader>
  journey-site users list
  journey-site users password <username>
  journey-site users disable <username>
  journey-site users enable <username>
";
const SHARE_LINKS_HELP: &str = "\
Usage:
  journey-site share-links lifetime [seconds]
  journey-site share-links create <published-post-id>
  journey-site share-links list
  journey-site share-links revoke <link-id>
";

#[tokio::main]
async fn main() -> AppResult<()> {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        print!("{HELP}");
        return Ok(());
    };
    if command == "--help" || command == "-h" || command == "help" {
        if args.next().is_some() {
            return Err("help does not take arguments".into());
        }
        print!("{HELP}");
        return Ok(());
    }

    let database = Database::new(database_path());
    match command.to_str() {
        Some("import") => {
            let manifest = args.next().ok_or("import requires a manifest path")?;
            if manifest == "--help" || manifest == "-h" {
                if args.next().is_some() {
                    return Err("import help does not take arguments".into());
                }
                println!("Usage: journey-site import <manifest.json>");
                return Ok(());
            }
            if args.next().is_some() {
                return Err("usage: journey-site import <manifest.json>".into());
            }
            let prepared = import::prepare_manifest(&PathBuf::from(manifest)).await?;
            let storage = connect_storage().await?;
            import::apply_import(prepared, &database, &storage).await
        }
        Some("db") => {
            let Some(subcommand) = args.next() else {
                print!("{DB_HELP}");
                return Ok(());
            };
            if subcommand == "--help" || subcommand == "-h" {
                if args.next().is_some() {
                    return Err("db help does not take arguments".into());
                }
                print!("{DB_HELP}");
                return Ok(());
            }
            database.initialize().await.map_err(std::io::Error::other)?;
            match subcommand.to_str() {
                Some("tables") if args.next().is_none() => {
                    for table in database.tables().await.map_err(std::io::Error::other)? {
                        println!("{table}");
                    }
                    Ok(())
                }
                Some("schema") if args.next().is_none() => {
                    for (name, statement) in database.schema().await.map_err(std::io::Error::other)? {
                        println!("-- {name}\n{statement};");
                    }
                    Ok(())
                }
                Some("posts") if args.next().is_none() => {
                    let posts = database.all_summaries().await.map_err(std::io::Error::other)?;
                    println!("{}", serde_json::to_string_pretty(&posts)?);
                    Ok(())
                }
                Some("post") => {
                    let id = args
                        .next()
                        .ok_or("db post requires an ID")?
                        .into_string()
                        .map_err(|_| "post ID must be UTF-8")?
                        .parse::<i64>()?;
                    if args.next().is_some() {
                        return Err("usage: journey-site db post <id>".into());
                    }
                    match database.post(id).await.map_err(std::io::Error::other)? {
                        Some(post) => println!("{}", serde_json::to_string_pretty(&post)?),
                        None => return Err(format!("published post {id} was not found").into()),
                    }
                    Ok(())
                }
                _ => Err("usage: journey-site db <tables|schema|posts|post <id>>".into()),
            }
        }
        Some("users") => {
            let subcommand = args.next().ok_or("users requires a subcommand")?;
            if subcommand == "--help" || subcommand == "-h" {
                if args.next().is_some() {
                    return Err("users help does not take arguments".into());
                }
                print!("{USERS_HELP}");
                return Ok(());
            }
            database.initialize().await.map_err(std::io::Error::other)?;
            match subcommand.to_str() {
                Some("create") => {
                    let username = next_username(&mut args)?;
                    let role = args.next().ok_or("users create requires an account role")?;
                    let role = db::AccountRole::parse(role.to_str().ok_or("account role must be UTF-8")?)?;
                    if args.next().is_some() {
                        return Err("usage: journey-site users create <username> <owner|reader>".into());
                    }
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
                Some("list") if args.next().is_none() => {
                    let accounts = database.accounts().await.map_err(std::io::Error::other)?;
                    for account in accounts {
                        println!("{}\t{}\t{}", account.role, account.username, if account.enabled { "enabled" } else { "disabled" });
                    }
                    Ok(())
                }
                Some("password") => {
                    let username = next_username(&mut args)?;
                    if args.next().is_some() {
                        return Err("usage: journey-site users password <username>".into());
                    }
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
                Some("disable") | Some("enable") => {
                    let enabled = subcommand == "enable";
                    let username = next_username(&mut args)?;
                    if args.next().is_some() {
                        return Err(format!("usage: journey-site users {} <username>", if enabled { "enable" } else { "disable" }).into());
                    }
                    auth::validate_username(&username)?;
                    database
                        .set_reader_enabled(username.clone(), enabled)
                        .await
                        .map_err(std::io::Error::other)?;
                    println!("{} reader account {username}", if enabled { "enabled" } else { "disabled" });
                    Ok(())
                }
                _ => Err("usage: journey-site users <create|list|password|disable|enable>".into()),
            }
        }
        Some("share-links") => {
            let subcommand = args.next().ok_or("share-links requires a subcommand")?;
            if subcommand == "--help" || subcommand == "-h" {
                if args.next().is_some() {
                    return Err("share-links help does not take arguments".into());
                }
                print!("{SHARE_LINKS_HELP}");
                return Ok(());
            }
            database.initialize().await.map_err(std::io::Error::other)?;
            match subcommand.to_str() {
                Some("lifetime") => {
                    match args.next() {
                        Some(value) => {
                            let seconds = value
                                .into_string()
                                .map_err(|_| "share link lifetime must be UTF-8")?
                                .parse::<i64>()?;
                            if args.next().is_some() {
                                return Err("usage: journey-site share-links lifetime [seconds]".into());
                            }
                            database
                                .set_share_link_lifetime_seconds(seconds)
                                .await
                                .map_err(std::io::Error::other)?;
                            println!("share link lifetime set to {seconds} seconds");
                        }
                        None => println!("{}", database.share_link_lifetime_seconds().await.map_err(std::io::Error::other)?),
                    }
                    Ok(())
                }
                Some("create") => {
                    let post_id = args
                        .next()
                        .ok_or("share-links create requires a published post ID")?
                        .into_string()
                        .map_err(|_| "post ID must be UTF-8")?
                        .parse::<i64>()?;
                    if args.next().is_some() || post_id <= 0 {
                        return Err("usage: journey-site share-links create <published-post-id>".into());
                    }
                    let bind = std::env::var("JOURNEY_SITE_BIND")
                        .unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
                    let bind_address = bind.parse::<SocketAddr>()?;
                    let origin = web::share_link_origin(bind_address).map_err(std::io::Error::other)?;
                    let link_id = auth::new_share_link_id();
                    let secret = auth::new_share_link_secret();
                    let created_at = unix_time_seconds();
                    match database
                        .create_share_link(
                            link_id.clone(),
                            post_id,
                            auth::session_token_digest(&secret),
                            created_at,
                        )
                        .await
                        .map_err(std::io::Error::other)?
                    {
                        Some(link) => {
                            println!("{}", link_url(&origin, &link.id, &secret));
                            Ok(())
                        }
                        None => Err(format!("published post {post_id} was not found").into()),
                    }
                }
                Some("list") if args.next().is_none() => {
                    let now = unix_time_seconds();
                    println!("Link ID\tPost ID\tExpires (epoch seconds)\tExpires (UTC)\tRevocation status");
                    for link in database.share_links().await.map_err(std::io::Error::other)? {
                        let status = match link.revoked_at {
                            Some(revoked_at) => format!("revoked at {revoked_at}"),
                            None if link.expires_at <= now => "expired, not revoked".to_owned(),
                            None => "not revoked".to_owned(),
                        };
                        println!(
                            "{}\t{}\t{}\t{}\t{}",
                            link.id,
                            link.post_id,
                            link.expires_at,
                            link.expires_at_utc.as_deref().unwrap_or("out of range"),
                            status,
                        );
                    }
                    Ok(())
                }
                Some("revoke") => {
                    let link_id = args
                        .next()
                        .ok_or("share-links revoke requires a link ID")?
                        .into_string()
                        .map_err(|_| "share link ID must be UTF-8")?;
                    if args.next().is_some() {
                        return Err("usage: journey-site share-links revoke <link-id>".into());
                    }
                    if !database
                        .revoke_share_link(link_id.clone(), unix_time_seconds())
                        .await
                        .map_err(std::io::Error::other)?
                    {
                        return Err(format!("share link {link_id} was not found").into());
                    }
                    println!("revoked share link {link_id}");
                    Ok(())
                }
                _ => Err("usage: journey-site share-links <lifetime|create|list|revoke>".into()),
            }
        }
        Some("serve") => {
            let next = args.next();
            if next.as_deref() == Some(std::ffi::OsStr::new("--help"))
                || next.as_deref() == Some(std::ffi::OsStr::new("-h"))
            {
                if args.next().is_some() {
                    return Err("serve help does not take arguments".into());
                }
                println!("Usage: journey-site serve [--allow-insecure-cookies]");
                return Ok(());
            }
            let allow_insecure_cookies = match next.as_deref() {
                None => false,
                Some(flag) if flag == std::ffi::OsStr::new("--allow-insecure-cookies") => true,
                Some(_) => return Err("usage: journey-site serve [--allow-insecure-cookies]".into()),
            };
            if args.next().is_some() {
                return Err("usage: journey-site serve [--allow-insecure-cookies]".into());
            }
            let bind = std::env::var("JOURNEY_SITE_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
            let bind_address = bind.parse::<SocketAddr>()?;
            if allow_insecure_cookies && !bind_address.ip().is_loopback() {
                return Err("--allow-insecure-cookies is only permitted when binding to loopback".into());
            }
            let security = web::SiteSecurity::from_env(bind_address, allow_insecure_cookies)
                .map_err(std::io::Error::other)?;
            database.initialize().await.map_err(std::io::Error::other)?;
            let storage = connect_storage().await?;
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
        _ => Err("unknown command; use journey-site --help".into()),
    }
}

fn next_username(args: &mut impl Iterator<Item = std::ffi::OsString>) -> AppResult<String> {
    args.next()
        .ok_or_else(|| std::io::Error::other("users command requires a username"))?
        .into_string()
        .map_err(|_| std::io::Error::other("username must be UTF-8").into())
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
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

async fn connect_storage() -> AppResult<H2cStorageClient> {
    let address = std::env::var("JOURNEY_STORAGE_H2C")
        .unwrap_or_else(|_| "127.0.0.1:8081".to_owned())
        .parse::<SocketAddr>()?;
    H2cStorageClient::connect(address).await
}
