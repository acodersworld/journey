mod db;
mod import;
mod storage;
mod web;

use std::{error::Error, net::SocketAddr, path::PathBuf};

use db::Database;
use storage::H2cStorageClient;
use tokio::net::TcpListener;

type AppResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const HELP: &str = "\
journey-site — read-only post backend and manifest importer

Usage:
  journey-site serve
  journey-site import <manifest.json>
  journey-site db tables
  journey-site db schema
  journey-site db posts
  journey-site db post <id>
  journey-site --help

Environment:
  JOURNEY_SITE_DB        SQLite file (default: journey-site.sqlite3)
  JOURNEY_SITE_BIND     HTTP listen address (default: 127.0.0.1:8080)
  JOURNEY_STORAGE_H2C   loopback h2c address (default: 127.0.0.1:8081)
";
const DB_HELP: &str = "\
Usage:
  journey-site db tables
  journey-site db schema
  journey-site db posts
  journey-site db post <id>
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
        Some("serve") => {
            let next = args.next();
            if next.as_deref() == Some(std::ffi::OsStr::new("--help"))
                || next.as_deref() == Some(std::ffi::OsStr::new("-h"))
            {
                if args.next().is_some() {
                    return Err("serve help does not take arguments".into());
                }
                println!("Usage: journey-site serve");
                return Ok(());
            }
            if next.is_some() {
                return Err("serve does not take arguments".into());
            }
            database.initialize().await.map_err(std::io::Error::other)?;
            let storage = connect_storage().await?;
            let bind = std::env::var("JOURNEY_SITE_BIND")
                .unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
            let listener = TcpListener::bind(&bind).await?;
            println!("journey-site HTTP listener on {}", listener.local_addr()?);
            axum::serve(listener, web::router(web::state(database, storage))).await?;
            Ok(())
        }
        _ => Err("unknown command; use journey-site --help".into()),
    }
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
