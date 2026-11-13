use std::env;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use comfy_table::{Attribute, Cell, Table, presets::NOTHING};
use serde::{Deserialize, Serialize};

use boot_api::auth::{CreateTokenRequest, Scopes};
use boot_api::store::{Store, resolve_api_db_path};

use crate::auth::{self, TokenSourceArgs};
use crate::cli_db::{CliDb, ServerEntry, resolve_cli_db_path, validate_url};
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
pub struct ShowServerArgs {
    /// Server name (defaults to local)
    pub name: Option<String>,
}

#[derive(Args, Default)]
pub struct EditServerArgs {
    /// Server name
    pub name: String,
    /// New connection URL
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
    /// New PEM CA certificate
    #[arg(long, value_name = "PATH", conflicts_with = "clear_ca_cert")]
    pub ca_cert: Option<PathBuf>,
    /// Remove the stored CA certificate
    #[arg(long, conflicts_with = "ca_cert")]
    pub clear_ca_cert: bool,
    #[command(flatten)]
    pub token_source: TokenSourceArgs,
    /// Remove the stored token
    #[arg(long, conflicts_with_all = ["token_stdin", "token_file", "token"])]
    pub clear_token: bool,
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

    let db = Store::open(&api_db_path)
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
                         Recover with:\n  boot create-token --name local-admin --scope admin \
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
                 boot create-token --name local-admin --scope admin\n  \
                 boot edit-server local --token-stdin"
            );
        } else if !has_credential {
            eprintln!(
                "Warning: the local server entry has no stored credential. Use:\n  \
                 boot create-token --name local-admin --scope admin --save-to local\n\
                 or, if you have the secret:\n  boot edit-server local --token-stdin"
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
             boot edit-server {} --token-stdin",
            args.name, args.name
        );
    }
    print_servers(&config, Some(&args.name)).await?;
    Ok(())
}

pub async fn show_server(args: ShowServerArgs, cli_db: Option<&Path>) -> Result<()> {
    let name = args.name.as_deref().unwrap_or("local");
    let config = CliDb::open_default(cli_db).await?;
    let server = config.get_server(name).await?;

    match &server {
        Some(server) => {
            print_field("Name:", &server.name);
            print_field("URL:", &server.url);
        }
        None if name == "local" => {
            print_field("Name:", format!("{name} (not configured)"));
        }
        None => anyhow::bail!("Server '{}' not found", name),
    }

    if name == "local" {
        let status = systemd_service::service_status()?.unwrap_or_else(|| "unknown".to_string());
        let bind = systemd_service::service_bind()?.unwrap_or_else(|| "unknown".to_string());
        print_field("Service:", status);
        print_field("Bind:", bind);
    }

    if let Some(server) = server {
        print_field("TLS:", if server.tls() { "https" } else { "http" });
        print_field("CA:", server.ca_cert.as_deref().unwrap_or("none"));
        print_field(
            "Token:",
            if server.token.is_some() {
                "stored"
            } else {
                "missing"
            },
        );
        print_auth_status(&server).await?;
    }

    Ok(())
}

fn print_field(label: &str, value: impl std::fmt::Display) {
    println!("{label:<9}{value}");
}

async fn print_auth_status(server: &ServerEntry) -> Result<()> {
    let token = auth::lookup_token(server)?;
    let base = server.url.trim_end_matches('/');
    let health_client = auth::build_http_client(server, None, Some(Duration::from_secs(5)))?;
    match health_client.get(format!("{base}/v1/health")).send().await {
        Ok(response) if response.status().is_success() => print_field("Server:", "reachable"),
        Ok(response) => {
            print_field(
                "Server:",
                format!("unreachable (health returned {})", response.status()),
            );
            return Ok(());
        }
        Err(error) => {
            print_field("Server:", format!("unreachable ({error})"));
            return Ok(());
        }
    }

    let Some(token) = token else {
        print_field("Read:", "missing token");
        print_field("Admin:", "missing token");
        return Ok(());
    };

    let client = auth::build_http_client(server, Some(&token), Some(Duration::from_secs(5)))?;
    match client.get(format!("{base}/v1/probes")).send().await {
        Ok(response) if response.status().is_success() => print_field("Read:", "ok"),
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            print_field("Read:", "invalid token")
        }
        Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
            print_field("Read:", "token lacks read scope")
        }
        Ok(response) => print_field("Read:", format!("unexpected status {}", response.status())),
        Err(error) => print_field("Read:", format!("request failed ({error})")),
    }

    match client.get(format!("{base}/v1/tokens")).send().await {
        Ok(response) if response.status().is_success() => print_field("Admin:", "ok"),
        Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED => {
            print_field("Admin:", "invalid token")
        }
        Ok(response) if response.status() == reqwest::StatusCode::FORBIDDEN => {
            print_field("Admin:", "token is not an admin token")
        }
        Ok(response) => print_field("Admin:", format!("unexpected status {}", response.status())),
        Err(error) => print_field("Admin:", format!("request failed ({error})")),
    }

    Ok(())
}

pub async fn edit_server(args: EditServerArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let entry = config
        .get_server(&args.name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", args.name))?;

    if !has_edits(&args) {
        return edit_server_interactive(&config, entry).await;
    }

    let url = args.url.as_deref();
    if let Some(url) = url {
        validate_url(url)?;
    }
    let ca_cert = if args.clear_ca_cert {
        Some(None)
    } else if let Some(path) = &args.ca_cert {
        validate_ca_path(path)?;
        Some(Some(path.display().to_string()))
    } else {
        None
    };
    let token = if args.clear_token {
        Some(None)
    } else {
        args.token_source.read()?.map(Some)
    };

    config
        .update_server(
            &args.name,
            url,
            ca_cert.as_ref().map(|value| value.as_deref()),
            token.as_ref().map(|value| value.as_deref()),
        )
        .await?;
    println!("Updated server '{}'", args.name);
    Ok(())
}

fn has_edits(args: &EditServerArgs) -> bool {
    args.url.is_some()
        || args.ca_cert.is_some()
        || args.clear_ca_cert
        || !args.token_source.is_empty()
        || args.clear_token
}

async fn edit_server_interactive(config: &CliDb, entry: ServerEntry) -> Result<()> {
    let document = EditableServer {
        url: entry.url.clone(),
        ca_cert: entry.ca_cert.clone(),
        token: None,
    };
    let yaml = serde_yaml::to_string(&document).context("Failed to render server YAML")?;
    let yaml = yaml.replace(
        "token: null",
        "# Set a value to replace the stored token, \"\" to clear it, or null to keep it.\ntoken: null",
    );
    let temp = TempFile::create(&yaml)?;
    run_editor(temp.path())?;
    let text = fs::read_to_string(temp.path())
        .with_context(|| format!("Failed to read {}", temp.path().display()))?;
    let parsed: EditableServer =
        serde_yaml::from_str(&text).context("Invalid server YAML document")?;

    validate_url(&parsed.url)?;
    let ca_cert = match parsed.ca_cert.as_deref() {
        None | Some("") => None,
        Some(path) => {
            validate_ca_path(Path::new(path))?;
            Some(path.to_string())
        }
    };
    let token = match parsed.token.as_deref() {
        None => None,
        Some("") => Some(None),
        Some(token) => Some(Some(auth::validate_token_input(token.to_string())?)),
    };

    let url_changed = parsed.url != entry.url;
    let ca_changed = ca_cert.as_deref() != entry.ca_cert.as_deref();
    let token_changed = match &token {
        None => false,
        Some(None) => entry.token.is_some(),
        Some(Some(token)) => entry.token.as_deref() != Some(token.as_str()),
    };
    if !url_changed && !ca_changed && !token_changed {
        println!("No changes.");
        return Ok(());
    }

    config
        .update_server(
            &entry.name,
            Some(&parsed.url),
            Some(ca_cert.as_deref()),
            token.as_ref().map(|value| value.as_deref()),
        )
        .await?;
    println!("Updated server '{}'", entry.name);
    Ok(())
}

fn validate_ca_path(path: &Path) -> Result<()> {
    fs::File::open(path)
        .with_context(|| format!("Failed to read CA certificate {}", path.display()))?;
    Ok(())
}

fn run_editor(path: &Path) -> Result<()> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() {
        anyhow::bail!("edit-server requires an interactive terminal");
    }
    let editor = env::var("EDITOR").unwrap_or_default();
    if editor.trim().is_empty() {
        anyhow::bail!(
            "EDITOR is not set; set it to an editor command such as 'vi' or 'code --wait'"
        );
    }
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{} \"$1\"", editor))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("Failed to run editor '{editor}'"))?;
    if !status.success() {
        anyhow::bail!("Editor exited with status {status}");
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditableServer {
    url: String,
    #[serde(default)]
    ca_cert: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create(contents: &str) -> Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let path = env::temp_dir().join(format!(
            ".boot-edit-server-{}-{nanos}.yaml",
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("Failed to write {}", path.display()))?;
        file.sync_all()?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn remove_server(args: RemoveServerArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    let server = config
        .get_server(&args.name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", args.name))?;
    eprintln!(
        "Removing server '{}' ({}). The server-side token is not revoked; use \
         boot revoke-token {}/<token-id> or boot revoke-token {} --all.",
        server.name, server.url, server.name, server.name
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
