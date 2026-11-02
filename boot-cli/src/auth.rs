//! Client-side credential handling and authentication checks.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};

use crate::cli_db::{CliDb, ServerEntry};

#[derive(Args, Debug, Clone, Default)]
pub struct TokenSourceArgs {
    /// Read the token from standard input
    #[arg(long)]
    pub token_stdin: bool,
    /// Read the token from a file
    #[arg(long, value_name = "PATH")]
    pub token_file: Option<PathBuf>,
    /// Token value (prefer --token-stdin or --token-file)
    #[arg(long, value_name = "TOKEN")]
    pub token: Option<String>,
}

impl TokenSourceArgs {
    pub fn read(&self) -> Result<Option<String>> {
        let mut sources = 0;
        if self.token_stdin {
            sources += 1;
        }
        if self.token_file.is_some() {
            sources += 1;
        }
        if self.token.is_some() {
            sources += 1;
        }
        if sources > 1 {
            anyhow::bail!("use only one of --token-stdin, --token-file, or --token");
        }

        if let Some(token) = &self.token {
            return validate_token_input(token.clone()).map(Some);
        }
        if let Some(path) = &self.token_file {
            return read_token_file(path).map(Some);
        }
        if self.token_stdin {
            let mut value = String::new();
            std::io::stdin()
                .read_to_string(&mut value)
                .context("Failed to read token from standard input")?;
            return validate_token_input(value).map(Some);
        }
        Ok(None)
    }
}

fn validate_token_input(mut token: String) -> Result<String> {
    while token.ends_with('\n') || token.ends_with('\r') {
        token.pop();
    }
    if token.trim().is_empty() {
        anyhow::bail!("token must not be empty");
    }
    if token.chars().any(char::is_whitespace) {
        anyhow::bail!("token must not contain whitespace");
    }
    Ok(token)
}

fn read_token_file(path: &Path) -> Result<String> {
    let mut value = String::new();
    std::fs::File::open(path)
        .with_context(|| format!("Failed to open token file {}", path.display()))?
        .read_to_string(&mut value)
        .with_context(|| format!("Failed to read token file {}", path.display()))?;
    validate_token_input(value)
}

fn environment_token() -> Result<Option<String>> {
    if let Ok(token) = std::env::var("BOOT_TOKEN") {
        if !token.is_empty() {
            return validate_token_input(token).map(Some);
        }
    }
    if let Ok(path) = std::env::var("BOOT_TOKEN_FILE") {
        if !path.is_empty() {
            return read_token_file(Path::new(&path)).map(Some);
        }
    }
    Ok(None)
}

/// Token used for a command: `BOOT_TOKEN` override first, then the stored
/// credential.
pub fn lookup_token(server: &ServerEntry) -> Result<Option<String>> {
    if let Some(token) = environment_token()? {
        return Ok(Some(token));
    }
    Ok(server.token.clone())
}

fn missing_token_error(server: &ServerEntry) -> anyhow::Error {
    anyhow::anyhow!(
        "Server '{}' has no token. Run: boot set-token {} --token-stdin",
        server.name,
        server.name
    )
}

pub(crate) fn build_http_client(
    server: &ServerEntry,
    token: Option<&str>,
    timeout: Option<Duration>,
) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    if let Some(token) = token {
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .context("Token contains characters that are not valid in an HTTP header")?;
        headers.insert(AUTHORIZATION, value);
    }

    let mut builder = reqwest::ClientBuilder::new()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none());
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(ca) = &server.ca_cert {
        let pem =
            std::fs::read(ca).with_context(|| format!("Failed to read CA certificate {}", ca))?;
        let certificate = reqwest::Certificate::from_pem(&pem)
            .with_context(|| format!("Failed to parse CA certificate {}", ca))?;
        builder = builder.add_root_certificate(certificate);
    }
    builder.build().context("Failed to build HTTP client")
}

pub fn http_client(server: &ServerEntry) -> Result<reqwest::Client> {
    let token = lookup_token(server)?.ok_or_else(|| missing_token_error(server))?;
    build_http_client(server, Some(&token), None)
}

pub fn http_client_with_timeout(
    server: &ServerEntry,
    timeout: Duration,
) -> Result<reqwest::Client> {
    let token = lookup_token(server)?.ok_or_else(|| missing_token_error(server))?;
    build_http_client(server, Some(&token), Some(timeout))
}

pub fn api_client(server: &ServerEntry) -> Result<boot_api_client::Client> {
    Ok(boot_api_client::Client::new_with_client(
        &server.url,
        http_client(server)?,
    ))
}

pub fn api_client_with_timeout(
    server: &ServerEntry,
    timeout: Duration,
) -> Result<boot_api_client::Client> {
    Ok(boot_api_client::Client::new_with_client(
        &server.url,
        http_client_with_timeout(server, timeout)?,
    ))
}

pub fn map_error(
    server: &ServerEntry,
    status: Option<reqwest::StatusCode>,
    error: &dyn std::fmt::Display,
) -> anyhow::Error {
    match status {
        Some(reqwest::StatusCode::UNAUTHORIZED) => anyhow::anyhow!(
            "Server '{}' rejected the token. Run: boot set-token {} --token-stdin",
            server.name,
            server.name
        ),
        Some(reqwest::StatusCode::FORBIDDEN) => anyhow::anyhow!(
            "Token for '{}' lacks the required scope ({}). Create a new token on the server \
             and update this client.",
            server.name,
            error
        ),
        Some(reqwest::StatusCode::NOT_FOUND) => {
            anyhow::anyhow!("No token matches the requested selector")
        }
        Some(reqwest::StatusCode::CONFLICT) => {
            anyhow::anyhow!("A token with that name already exists. Use --replace to rotate it.")
        }
        _ => anyhow::anyhow!("{}", error),
    }
}

#[derive(Args)]
pub struct SetTokenArgs {
    /// Server name
    pub server: String,
    #[command(flatten)]
    pub token_source: TokenSourceArgs,
}

pub async fn set_token(args: SetTokenArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    if config.get_server(&args.server).await?.is_none() {
        anyhow::bail!("Server '{}' not found", args.server);
    }
    let token = args
        .token_source
        .read()?
        .ok_or_else(|| anyhow::anyhow!("No token supplied; use --token-stdin or --token-file"))?;
    config.set_token(&args.server, &token).await?;
    println!("Stored token for server '{}'", args.server);
    Ok(())
}
