//! Token administration. Commands use the HTTP admin API by default and can
//! open the local API database directly with `--direct`.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;

use boot_api::auth::{CreateTokenRequest, Scopes, TokenRecord, TokenStore};
use boot_api::store::{Store, resolve_api_db_path};

use crate::auth::{api_client, map_error};
use crate::cli_db::{CliDb, ServerEntry};

#[derive(Args)]
pub struct CreateTokenArgs {
    /// Server name. Omit with --direct.
    #[arg(value_name = "SERVER")]
    pub server: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long, conflicts_with = "server")]
    pub direct: bool,
    /// Human-readable token name
    #[arg(long)]
    pub name: String,
    /// Create a read-only token
    #[arg(long)]
    pub read_only: bool,
    /// Scope to grant (repeatable: read, control, admin)
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Expiry duration such as 30d, 12h, or 3600s
    #[arg(long, value_name = "DURATION")]
    pub expires_in: Option<String>,
    /// Replace an existing token with the same name
    #[arg(long)]
    pub replace: bool,
    /// Store the new token in the CLI DB for this server entry
    #[arg(long, value_name = "SERVER")]
    pub save_to: Option<String>,
}

#[derive(Args, Default)]
pub struct ListTokensArgs {
    /// Only list tokens for this server (defaults to all configured servers)
    #[arg(long, value_name = "SERVER")]
    pub server: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long, conflicts_with = "server")]
    pub direct: bool,
}

#[derive(Args)]
pub struct RevokeTokenArgs {
    /// Server/token selector, or the server name together with --all
    #[arg(value_name = "SERVER/TOKEN")]
    pub target: Option<String>,
    /// Open the local API database directly instead of using the HTTP API
    #[arg(long)]
    pub direct: bool,
    /// Revoke every token
    #[arg(long)]
    pub all: bool,
    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,
}

fn target_scopes(read_only: bool, names: &[String]) -> Result<Scopes> {
    if read_only && !names.is_empty() {
        anyhow::bail!("--read-only cannot be combined with --scope");
    }
    if read_only {
        return Ok(Scopes::read());
    }
    let scopes = Scopes::from_names(names.iter().map(String::as_str))
        .map_err(|message| anyhow::anyhow!(message))?;
    Ok(scopes)
}

fn parse_duration(value: &str) -> Result<i64> {
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("expiry duration must not be empty");
    }
    let (digits, unit) = value.split_at(
        value
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    if digits.is_empty() {
        anyhow::bail!("invalid expiry duration '{}'", value);
    }
    let amount: i64 = digits
        .parse()
        .with_context(|| format!("invalid expiry duration '{}'", value))?;
    let multiplier = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 60 * 60,
        "d" => 24 * 60 * 60,
        "w" => 7 * 24 * 60 * 60,
        _ => anyhow::bail!("unknown expiry unit '{}'", unit),
    };
    let seconds = amount
        .checked_mul(multiplier)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| anyhow::anyhow!("expiry must be positive"))?;
    Ok(seconds)
}

pub async fn create_token(
    args: CreateTokenArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    let scopes = target_scopes(args.read_only, &args.scopes)?;
    if scopes.has(boot_api::auth::Capability::Admin) {
        eprintln!(
            "Warning: this token has the admin scope and can mint or revoke every other token."
        );
    }
    let expires_in = args.expires_in.as_deref().map(parse_duration).transpose()?;

    let (token, record) = if args.direct {
        let path = resolve_api_db_path(api_db)?;
        let db = Store::open(&path).await?;
        let expires_at = match expires_in {
            Some(seconds) => Some(
                unix_now()
                    .checked_add(seconds)
                    .ok_or_else(|| anyhow::anyhow!("expiry is out of range"))?,
            ),
            None => None,
        };
        let created = db
            .create_token(CreateTokenRequest {
                name: args.name.clone(),
                scopes,
                expires_at,
                replace: args.replace,
            })
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
        (created.token, created.record)
    } else {
        let server_name = args
            .server
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("specify a server name or use --direct"))?;
        if api_db.is_some() {
            anyhow::bail!("--api-db requires --direct");
        }
        let config = CliDb::open_default(cli_db).await?;
        let server = config
            .get_server(server_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
        let client = api_client(&server)?;
        let response = client
            .create_token(&boot_api_client::types::TokenCreateRequest {
                name: args.name.clone(),
                scopes: Some(scopes.names()),
                expires_in_seconds: expires_in,
                replace: args.replace,
            })
            .await
            .map_err(|error| {
                let error = map_error(&server, error.status(), &format_args!("{error:#}"));
                if error.to_string().contains("lacks the required scope") {
                    anyhow::anyhow!(
                        "The stored token for '{}' is not an admin token. Store one with \
                         boot edit-server {} --token-stdin, or use boot create-token --direct \
                         --api-db on the server host.",
                        server_name,
                        server_name
                    )
                } else {
                    error
                }
            })?;
        let body = response.into_inner();
        (
            body.token,
            TokenRecord {
                id: body.id,
                name: body.name,
                token_hash: [0u8; 32],
                scopes: Scopes::from_names(body.scopes)
                    .map_err(|message| anyhow::anyhow!(message))?,
                created_at: body.created_at,
                expires_at: body.expires_at,
            },
        )
    };

    println!("{token}");
    eprintln!(
        "Token '{}' ({}) created with scopes: {}. It will not be shown again.",
        record.name,
        record.id,
        record.scopes.display()
    );

    if let Some(save_to) = args.save_to {
        if let Err(error) = save_token(cli_db, &save_to, &token).await {
            eprintln!(
                "Warning: could not store the token for '{}': {}. The token is valid; run: \
                 boot edit-server {} --token-stdin",
                save_to, error, save_to
            );
        }
    }
    Ok(())
}

async fn save_token(cli_db: Option<&Path>, server_name: &str, token: &str) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    config.set_token(server_name, token).await
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

struct ListedToken {
    server: Option<String>,
    record: TokenRecord,
    current: bool,
}

pub async fn list_tokens(
    args: ListTokensArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    if args.direct {
        let path = resolve_api_db_path(api_db)?;
        let db = Store::open(&path).await?;
        let rows = db
            .list_tokens()
            .await
            .map_err(|error| anyhow::anyhow!(error))?
            .into_iter()
            .map(|record| ListedToken {
                server: None,
                record,
                current: false,
            })
            .collect::<Vec<_>>();
        print_token_table(&rows, false);
        return Ok(());
    }

    if api_db.is_some() {
        anyhow::bail!("--api-db requires --direct");
    }
    let config = CliDb::open_default(cli_db).await?;
    let servers = match args.server.as_deref() {
        Some(name) => {
            let server = config
                .get_server(name)
                .await?
                .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", name))?;
            vec![server]
        }
        None => config.list_servers().await?,
    };
    if servers.is_empty() {
        println!("No servers configured.");
        return Ok(());
    }

    let mut rows = Vec::new();
    for server in servers {
        let client = match api_client(&server) {
            Ok(client) => client,
            Err(error) => {
                eprintln!("Error connecting to server '{}': {}", server.name, error);
                continue;
            }
        };
        match client.list_tokens().await {
            Ok(response) => {
                for summary in response.into_inner() {
                    rows.push(ListedToken {
                        server: Some(server.name.clone()),
                        record: TokenRecord {
                            id: summary.id,
                            name: summary.name,
                            token_hash: [0u8; 32],
                            scopes: Scopes::from_names(summary.scopes)
                                .unwrap_or_else(|_| Scopes::read()),
                            created_at: summary.created_at,
                            expires_at: summary.expires_at,
                        },
                        current: summary.current,
                    });
                }
            }
            Err(error) => {
                let message = map_error(&server, error.status(), &format_args!("{error:#}"));
                eprintln!(
                    "Error listing tokens on server '{}' ({}): {}",
                    server.name, server.url, message
                );
            }
        }
    }

    print_token_table(&rows, true);
    Ok(())
}

fn print_token_table(rows: &[ListedToken], show_server: bool) {
    if rows.is_empty() {
        println!("No tokens.");
        return;
    }
    let mut table = comfy_table::Table::new();
    table.load_style(comfy_table::presets::NOTHING);
    if show_server {
        table.set_header(vec![
            "SERVER", "ID", "NAME", "SCOPES", "CREATED", "EXPIRES", "",
        ]);
    } else {
        table.set_header(vec!["ID", "NAME", "SCOPES", "CREATED", "EXPIRES"]);
    }
    for row in rows {
        let mut cells = Vec::new();
        if show_server {
            cells.push(comfy_table::Cell::new(
                row.server.as_deref().unwrap_or("direct"),
            ));
        }
        cells.extend([
            comfy_table::Cell::new(&row.record.id),
            comfy_table::Cell::new(&row.record.name),
            comfy_table::Cell::new(row.record.scopes.display()),
            comfy_table::Cell::new(format_timestamp(row.record.created_at)),
            comfy_table::Cell::new(
                row.record
                    .expires_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "never".to_string()),
            ),
        ]);
        if show_server {
            cells.push(comfy_table::Cell::new(if row.current {
                "current"
            } else {
                ""
            }));
        }
        table.add_row(cells);
    }
    println!("{table}");
}

fn format_timestamp(unix: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(unix)
        .ok()
        .and_then(|value| {
            value
                .format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .map(|value| value.replace('T', " ").trim_end_matches('Z').to_string())
        .unwrap_or_else(|| unix.to_string())
}

pub(crate) fn confirm(prompt: &str, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        anyhow::bail!("aborted");
    }
    Ok(())
}

pub async fn revoke_token(
    args: RevokeTokenArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    if args.direct {
        return revoke_token_direct(args, api_db).await;
    }
    if api_db.is_some() {
        anyhow::bail!("--api-db requires --direct");
    }
    revoke_token_api(args, cli_db).await
}

async fn revoke_token_direct(args: RevokeTokenArgs, api_db: Option<&Path>) -> Result<()> {
    if args.all && args.target.is_some() {
        anyhow::bail!("specify either a token selector or --all, not both");
    }
    if !args.all && args.target.is_none() {
        anyhow::bail!("specify a token id/name or --all");
    }
    if let Some(target) = args.target.as_deref() {
        if target.contains('/') {
            anyhow::bail!("with --direct, pass a token id or name without a server prefix");
        }
    }
    if args.all {
        eprintln!("This removes the local API database's tokens. The server may still be running.");
        confirm("Revoke all tokens from the local API database?", args.yes)?;
    }

    let path = resolve_api_db_path(api_db)?;
    let db = Store::open(&path).await?;
    let revoked = if args.all {
        db.revoke_all()
            .await
            .map_err(|error| anyhow::anyhow!(error))? as u32
    } else {
        db.revoke_token(args.target.as_deref().unwrap())
            .await
            .map_err(|error| anyhow::anyhow!(error))? as u32
    };
    println!("Revoked {revoked} token(s)");
    Ok(())
}

async fn revoke_token_api(args: RevokeTokenArgs, cli_db: Option<&Path>) -> Result<()> {
    let config = CliDb::open_default(cli_db).await?;
    if args.all {
        let server_name = args
            .target
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("specify a server name with --all"))?;
        if server_name.contains('/') {
            anyhow::bail!("with --all, pass the server name without a token selector");
        }
        let server = config
            .get_server(server_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
        let client = api_client(&server)?;
        eprintln!("This removes every API token on '{}'.", server.name);
        confirm("Revoke every token? This cannot be undone.", args.yes)?;
        let revoked = client
            .revoke_all_tokens(Some(true))
            .await
            .map_err(|error| {
                admin_scope_error(&server, error.status(), &format_args!("{error:#}"))
            })?
            .into_inner()
            .revoked as u32;
        println!("Revoked {revoked} token(s)");
        return Ok(());
    }

    let target = args
        .target
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("specify <server>/<token-id> or use --all"))?;
    let (server_name, selector) = target
        .split_once('/')
        .filter(|(server, token)| !server.is_empty() && !token.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected <server>/<token-id>"))?;
    let server = config
        .get_server(server_name)
        .await?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
    let client = api_client(&server)?;
    let current = current_token(&client, &server).await.ok().flatten();
    if current
        .as_ref()
        .is_some_and(|(id, name)| selector == id || selector == name)
    {
        eprintln!(
            "Warning: '{}' is the token used by this command. Revoking it will immediately end this admin session.",
            selector
        );
        if !args.yes {
            confirm(
                "The token you are revoking is the one used by this command. Continue?",
                false,
            )?;
        }
    }
    let revoked = client
        .revoke_token(selector)
        .await
        .map_err(|error| {
            if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                return anyhow::anyhow!(
                    "No token matches '{}'. Run: boot list-tokens --server {}",
                    selector,
                    server_name
                );
            }
            admin_scope_error(&server, error.status(), &format_args!("{error:#}"))
        })?
        .into_inner()
        .revoked as u32;
    println!("Revoked {revoked} token(s)");
    Ok(())
}

fn admin_scope_error(
    server: &ServerEntry,
    status: Option<reqwest::StatusCode>,
    error: &dyn std::fmt::Display,
) -> anyhow::Error {
    let error = map_error(server, status, error);
    if error.to_string().contains("lacks the required scope") {
        anyhow::anyhow!(
            "The stored token for '{}' is not an admin token. Store one with \
             boot edit-server {} --token-stdin, or use --direct on the server host.",
            server.name,
            server.name
        )
    } else {
        error
    }
}

async fn current_token(
    client: &boot_api_client::Client,
    server: &ServerEntry,
) -> Result<Option<(String, String)>> {
    let response = client
        .list_tokens()
        .await
        .map_err(|error| map_error(server, error.status(), &format_args!("{error:#}")))?;
    Ok(response
        .into_inner()
        .into_iter()
        .find(|summary| summary.current)
        .map(|summary| (summary.id, summary.name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_expiry_durations() {
        assert_eq!(parse_duration("30d").unwrap(), 30 * 24 * 60 * 60);
        assert_eq!(parse_duration("12h").unwrap(), 12 * 60 * 60);
        assert_eq!(parse_duration("60").unwrap(), 60);
        assert!(parse_duration("0d").is_err());
        assert!(parse_duration("1y").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn read_only_and_scope_conflict() {
        assert!(target_scopes(true, &["control".to_string()]).is_err());
        assert!(
            target_scopes(false, &["admin".to_string()])
                .unwrap()
                .has(boot_api::auth::Capability::Admin)
        );
    }
}
