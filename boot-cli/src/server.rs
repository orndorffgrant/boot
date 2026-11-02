use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use comfy_table::{Attribute, Cell, Table, presets::NOTHING};

use boot_api::api_db::ApiDb;
use boot_api::api_db::resolve_api_db_path;
use boot_api::auth::{CreateTokenRequest, Scopes};

use crate::auth::{self, SetTokenArgs, TokenSourceArgs};
use crate::cli_db::{CliDb, resolve_cli_db_path};
use crate::systemd_service::{self, ServiceConfig};

#[derive(Args, Default)]
pub struct ServerArgs {
    /// Host to listen on (defaults to 127.0.0.1)
    #[arg(long)]
    pub host: Option<String>,
    /// Port to run the server on (defaults to 46673)
    #[arg(short, long)]
    pub port: Option<u16>,
    /// Skip creating the local server entry, token, and credential
    #[arg(long)]
    pub no_provision: bool,
    /// PEM certificate for TLS
    #[arg(long, value_name = "PATH", requires = "tls_key")]
    pub tls_cert: Option<std::path::PathBuf>,
    /// PEM private key for TLS
    #[arg(long, value_name = "PATH", requires = "tls_cert")]
    pub tls_key: Option<std::path::PathBuf>,
    /// Acknowledge plaintext HTTP on a non-loopback bind
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// URL to store for the local server entry
    #[arg(long, value_name = "URL")]
    pub advertise_url: Option<String>,
}

#[derive(Args)]
pub struct RemoveServerArgs {
    /// Name of the server to remove
    pub name: String,
}

#[derive(Args)]
pub struct AddServerArgs {
    /// Name of the server
    pub name: String,
    /// Connection URL (e.g., http://localhost:46673)
    pub url: String,
    #[command(flatten)]
    pub token_source: TokenSourceArgs,
    /// Optional PEM CA certificate for a self-signed server
    #[arg(long, value_name = "PATH")]
    pub ca_cert: Option<std::path::PathBuf>,
}

#[derive(Args)]
pub struct CheckAuthArgs {
    /// Server name
    pub server: String,
}

pub async fn start_server(
    args: ServerArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    let prepared = prepare(&args, cli_db, api_db).await?;
    let service = service_config(&args, prepared.cli_db.clone(), prepared.api_db);
    systemd_service::start_service(&service)?;

    let config = CliDb::open(&prepared.cli_db).await?;
    print_servers(&config, Some("local")).await?;
    Ok(())
}

pub async fn stop_server(_cli_db: Option<&Path>) -> Result<()> {
    systemd_service::stop_service()?;
    println!("Server stopped");
    Ok(())
}

pub async fn restart_server() -> Result<()> {
    systemd_service::restart_service()?;
    println!("Server restarted");
    Ok(())
}

pub async fn run_server(
    args: ServerArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    let prepared = prepare(&args, cli_db, api_db).await?;
    boot_api::run(boot_api::RunOptions {
        host: args.host,
        port: args.port,
        api_db: Some(prepared.api_db),
        tls_cert: args.tls_cert,
        tls_key: args.tls_key,
        allow_insecure_http: args.allow_insecure_http,
    })
    .await
}

struct Prepared {
    api_db: std::path::PathBuf,
    cli_db: std::path::PathBuf,
}

async fn prepare(
    args: &ServerArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<Prepared> {
    let host = args.host.as_deref().unwrap_or("127.0.0.1");
    if !boot_api::is_loopback_host(host) && args.tls_cert.is_none() && !args.allow_insecure_http {
        anyhow::bail!(
            "refusing to bind {host} without TLS; pass --tls-cert/--tls-key or \
             --allow-insecure-http (only on an encrypted network)"
        );
    }

    let api_db_path = resolve_api_db_path(api_db)?;
    let cli_db_path = resolve_cli_db_path(cli_db)?;

    let db = ApiDb::open(&api_db_path)
        .await
        .with_context(|| format!("Failed to open API database {}", api_db_path.display()))?;
    let config = CliDb::open(&cli_db_path).await?;

    if !args.no_provision {
        let url = advertised_url(
            args.host.as_deref(),
            args.port.unwrap_or(46673),
            args.tls_cert.is_some(),
            args.advertise_url.as_deref(),
        );
        config.upsert_server("local", &url, None, None).await?;

        let has_credential = config
            .get_server("local")
            .await?
            .and_then(|server| server.token)
            .is_some();

        if !db
            .has_tokens()
            .await
            .map_err(|error| anyhow::anyhow!(error))?
        {
            let created = db
                .create_token(CreateTokenRequest {
                    name: "local".to_string(),
                    scopes: Scopes::admin(),
                    expires_at: None,
                    replace: false,
                })
                .await
                .map_err(|error| anyhow::anyhow!(error))?;
            match config.set_token("local", &created.token).await {
                Ok(()) => {
                    eprintln!(
                        "Provisioned admin token 'local' and stored it in {}",
                        cli_db_path.display()
                    );
                }
                Err(error) => {
                    eprintln!(
                        "Warning: created the local admin token but could not store it: {}. \
                         Recover with:\n  boot admin create-token --name local-admin --scope admin \
                         --save-to local",
                        error
                    );
                }
            }
        } else if !db
            .has_admin_token()
            .await
            .map_err(|error| anyhow::anyhow!(error))?
        {
            eprintln!(
                "Warning: the API database has tokens but no admin token. Create one with:\n  \
                 boot admin create-token --name local-admin --scope admin\n  \
                 boot set-token local --token-stdin"
            );
        } else if !has_credential {
            eprintln!(
                "Warning: the local server entry has no stored credential. Use:\n  \
                 boot admin create-token --name local-admin --scope admin --save-to local\n\
                 or, if you have the secret:\n  boot set-token local --token-stdin"
            );
        }
    }

    Ok(Prepared {
        api_db: api_db_path,
        cli_db: cli_db_path,
    })
}

fn advertised_url(host: Option<&str>, port: u16, tls: bool, explicit: Option<&str>) -> String {
    if let Some(url) = explicit {
        return url.to_string();
    }
    let host = host.unwrap_or("127.0.0.1");
    let host = match host {
        "0.0.0.0" | "::" | "[::]" => "localhost",
        host => host,
    };
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{}]", host)
    } else {
        host.to_string()
    };
    format!("{}://{}:{}", if tls { "https" } else { "http" }, host, port)
}

fn service_config(
    args: &ServerArgs,
    cli_db: std::path::PathBuf,
    api_db: std::path::PathBuf,
) -> ServiceConfig {
    ServiceConfig {
        host: args.host.clone(),
        port: args.port,
        api_db,
        cli_db,
        no_provision: args.no_provision,
        tls_cert: args.tls_cert.clone(),
        tls_key: args.tls_key.clone(),
        allow_insecure_http: args.allow_insecure_http,
        advertise_url: args.advertise_url.clone(),
    }
}

pub async fn list_servers(cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    print_servers(&config, None).await
}

pub async fn add_server(args: AddServerArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let token = args.token_source.read()?;
    let ca_cert = args.ca_cert.as_ref().map(|path| path.display().to_string());
    config
        .add_server(&args.name, &args.url, ca_cert.as_deref(), token.as_deref())
        .await?;

    if token.is_none() {
        eprintln!(
            "Warning: no token supplied for '{}'. Protected commands will fail until you run: \
             boot set-token {} --token-stdin",
            args.name, args.name
        );
    }
    print_servers(&config, Some(&args.name)).await?;
    Ok(())
}

pub async fn set_token(args: SetTokenArgs, cli_db: Option<&Path>) -> Result<()> {
    auth::set_token(args, cli_db).await
}

pub async fn check_auth(args: CheckAuthArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(&args.server)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", args.server))?;
    let token = auth::lookup_token(&server)?;

    let base = server.url.trim_end_matches('/');
    let health_client = auth::build_http_client(&server, None, Some(Duration::from_secs(5)))?;
    let health = health_client.get(format!("{base}/v1/health")).send().await;
    match health {
        Ok(response) if response.status().is_success() => {
            println!("server: reachable");
        }
        Ok(response) => {
            println!(
                "server: unreachable (health returned {})",
                response.status()
            );
            return Ok(());
        }
        Err(error) => {
            println!("server: unreachable ({error})");
            return Ok(());
        }
    }

    let Some(token) = token else {
        println!("token: missing");
        println!("read: missing token");
        println!("admin: missing token");
        return Ok(());
    };

    let client = auth::build_http_client(&server, Some(&token), Some(Duration::from_secs(5)))?;
    let read = client.get(format!("{base}/v1/probes")).send().await;
    match read {
        Ok(response) if response.status().is_success() => println!("read: ok"),
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            println!("read: invalid token")
        }
        Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
            println!("read: token lacks read scope")
        }
        Ok(response) => println!("read: unexpected status {}", response.status()),
        Err(error) => println!("read: request failed ({error})"),
    }

    let admin = client.get(format!("{base}/v1/tokens")).send().await;
    match admin {
        Ok(response) if response.status().is_success() => println!("admin: ok"),
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            println!("admin: invalid token")
        }
        Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
            println!("admin: token is not an admin token")
        }
        Ok(response) => println!("admin: unexpected status {}", response.status()),
        Err(error) => println!("admin: request failed ({error})"),
    }

    Ok(())
}

pub async fn remove_server(args: RemoveServerArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(&args.name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", args.name))?;
    eprintln!(
        "Removing server '{}' ({}). The server-side token is not revoked; use \
         boot admin --server {} revoke-token or the local API DB to revoke it.",
        server.name, server.url, server.name
    );
    print_servers_removed(&config, &args.name).await?;
    config.remove_server(&args.name).await?;
    Ok(())
}

pub async fn print_servers(config: &CliDb, highlight: Option<&str>) -> Result<()> {
    print_servers_with_style(config, highlight, Some(Attribute::Bold)).await
}

pub async fn print_servers_removed(config: &CliDb, name: &str) -> Result<()> {
    print_servers_with_style(config, Some(name), Some(Attribute::CrossedOut)).await
}

async fn print_servers_with_style(
    config: &CliDb,
    highlight: Option<&str>,
    style: Option<Attribute>,
) -> Result<()> {
    let servers = config.list_servers().await?;
    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }

    let mut table = Table::new();
    table.load_style(NOTHING);
    table.set_header(vec!["NAME", "URL", "TOKEN", "TLS"]);
    for server in &servers {
        let mut name = Cell::new(&server.name);
        if Some(server.name.as_str()) == highlight {
            if let Some(style) = style {
                name = name.add_attribute(style);
            }
        }
        table.add_row(vec![
            name,
            Cell::new(&server.url),
            Cell::new(if server.token.is_some() {
                "stored"
            } else {
                "missing"
            }),
            Cell::new(if server.tls() { "https" } else { "http" }),
        ]);
    }
    println!("{table}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_connectable_urls() {
        assert_eq!(
            advertised_url(None, 46673, false, None),
            "http://127.0.0.1:46673"
        );
        assert_eq!(
            advertised_url(Some("0.0.0.0"), 46673, false, None),
            "http://localhost:46673"
        );
        assert_eq!(
            advertised_url(Some("::1"), 46673, true, None),
            "https://[::1]:46673"
        );
        assert_eq!(
            advertised_url(Some("pi"), 46673, false, Some("http://pi.local:1")),
            "http://pi.local:1"
        );
    }
}
