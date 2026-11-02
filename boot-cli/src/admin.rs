//! `boot admin` token administration, using the local API DB by default or the
//! HTTP admin API when `--server` is supplied.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use boot_api::api_db::{ApiDb, resolve_api_db_path};
use boot_api::auth::{CreateTokenRequest, Scopes, TokenRecord, TokenStore};

use crate::auth::{api_client, map_error};
use crate::cli_db::{CliDb, ServerEntry};

#[derive(Args)]
pub struct AdminArgs {
    /// Manage tokens through this server's HTTP admin API instead of the local API DB
    #[arg(long)]
    pub server: Option<String>,
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand)]
pub enum AdminCommand {
    /// Create a new API token
    #[command(name = "create-token")]
    CreateToken(CreateTokenArgs),
    /// List API token metadata
    #[command(name = "list-tokens")]
    ListTokens,
    /// Revoke one token or all tokens
    #[command(name = "revoke-token")]
    RevokeToken(RevokeTokenArgs),
}

#[derive(Args)]
pub struct CreateTokenArgs {
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

#[derive(Args)]
pub struct RevokeTokenArgs {
    /// Token id or name to revoke
    #[arg(value_name = "ID_OR_NAME")]
    pub selector: Option<String>,
    /// Revoke every token
    #[arg(long)]
    pub all: bool,
    /// Skip the interactive confirmation
    #[arg(long)]
    pub yes: bool,
}

pub async fn admin(args: AdminArgs, cli_db: Option<&Path>, api_db: Option<&Path>) -> Result<()> {
    match args.command {
        AdminCommand::CreateToken(create) => {
            create_token(args.server.as_deref(), create, cli_db, api_db).await
        }
        AdminCommand::ListTokens => list_tokens(args.server.as_deref(), cli_db, api_db).await,
        AdminCommand::RevokeToken(revoke) => {
            revoke_token(args.server.as_deref(), revoke, cli_db, api_db).await
        }
    }
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

async fn create_token(
    server_name: Option<&str>,
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

    let (token, record) = if let Some(server_name) = server_name {
        if api_db.is_some() {
            anyhow::bail!("--server and --api-db cannot be used together");
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
                         boot set-token {} --token-stdin, or use boot admin --api-db on the server host.",
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
    } else {
        let path = resolve_api_db_path(api_db)?;
        let db = ApiDb::open(&path).await?;
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
                 boot set-token {} --token-stdin",
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

async fn list_tokens(
    server_name: Option<&str>,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    let tokens: Vec<TokenRecord>;
    let mut current: Option<String> = None;
    if let Some(server_name) = server_name {
        if api_db.is_some() {
            anyhow::bail!("--server and --api-db cannot be used together");
        }
        let config = CliDb::open_default(cli_db).await?;
        let server = config
            .get_server(server_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
        let client = api_client(&server)?;
        let response = client.list_tokens().await.map_err(|error| {
            let error = map_error(&server, error.status(), &format_args!("{error:#}"));
            if error.to_string().contains("lacks the required scope") {
                anyhow::anyhow!(
                    "The stored token for '{}' is not an admin token. Store one with \
                     boot set-token {} --token-stdin, or use boot admin --api-db on the server host.",
                    server_name,
                    server_name
                )
            } else {
                error
            }
        })?;
        tokens = response
            .into_inner()
            .into_iter()
            .map(|summary| {
                if summary.current {
                    current = Some(summary.id.clone());
                }
                TokenRecord {
                    id: summary.id,
                    name: summary.name,
                    token_hash: [0u8; 32],
                    scopes: Scopes::from_names(summary.scopes).unwrap_or_else(|_| Scopes::read()),
                    created_at: summary.created_at,
                    expires_at: summary.expires_at,
                }
            })
            .collect();
    } else {
        let path = resolve_api_db_path(api_db)?;
        let db = ApiDb::open(&path).await?;
        tokens = db
            .list_tokens()
            .await
            .map_err(|error| anyhow::anyhow!(error))?;
    }

    print_token_table(&tokens, current.as_deref());
    Ok(())
}

fn print_token_table(tokens: &[TokenRecord], current: Option<&str>) {
    if tokens.is_empty() {
        println!("No tokens.");
        return;
    }
    let mut table = comfy_table::Table::new();
    table.load_style(comfy_table::presets::NOTHING);
    table.set_header(vec!["ID", "NAME", "SCOPES", "CREATED", "EXPIRES", ""]);
    for token in tokens {
        let marker = if Some(token.id.as_str()) == current {
            "current"
        } else {
            ""
        };
        table.add_row(vec![
            comfy_table::Cell::new(&token.id),
            comfy_table::Cell::new(&token.name),
            comfy_table::Cell::new(token.scopes.display()),
            comfy_table::Cell::new(format_timestamp(token.created_at)),
            comfy_table::Cell::new(
                token
                    .expires_at
                    .map(format_timestamp)
                    .unwrap_or_else(|| "never".to_string()),
            ),
            comfy_table::Cell::new(marker),
        ]);
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

fn confirm(prompt: &str, yes: bool) -> Result<()> {
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

async fn revoke_token(
    server_name: Option<&str>,
    args: RevokeTokenArgs,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<()> {
    if args.all && args.selector.is_some() {
        anyhow::bail!("specify either a selector or --all, not both");
    }
    if !args.all && args.selector.is_none() {
        anyhow::bail!("specify a token id/name or --all");
    }
    if args.all {
        eprintln!("Warning: this revokes every API token, including admin tokens.");
        confirm("Revoke every token? This cannot be undone.", args.yes)?;
    }

    let revoked = if let Some(server_name) = server_name {
        if api_db.is_some() {
            anyhow::bail!("--server and --api-db cannot be used together");
        }
        let config = CliDb::open_default(cli_db).await?;
        let server = config
            .get_server(server_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Server '{}' not found", server_name))?;
        let client = api_client(&server)?;

        if args.all {
            client
                .revoke_all_tokens(Some(true))
                .await
                .map_err(|error| {
                    let error = map_error(&server, error.status(), &format_args!("{error:#}"));
                    if error.to_string().contains("lacks the required scope") {
                        anyhow::anyhow!(
                            "The stored token for '{}' is not an admin token. Store one with \
                             boot set-token {} --token-stdin, or use boot admin --api-db on the server host.",
                            server_name,
                            server_name
                        )
                    } else {
                        error
                    }
                })?
                .into_inner()
                .revoked as u32
        } else {
            let selector = args.selector.as_deref().unwrap();
            let current = current_token(&client, &server).await.ok().flatten();
            if current
                .as_ref()
                .is_some_and(|(id, name)| selector == id || selector == name)
            {
                eprintln!(
                    "Warning: '{}' is the token used by this command. Revoking it will                      immediately end this admin session.",
                    selector
                );
                if !args.yes {
                    confirm(
                        "The token you are revoking is the one used by this command. Continue?",
                        false,
                    )?;
                }
            }
            client
                .revoke_token(selector)
                .await
                .map_err(|error| {
                    if error.status() == Some(reqwest::StatusCode::NOT_FOUND) {
                        return anyhow::anyhow!(
                            "No token matches '{}'. Run: boot admin list-tokens",
                            selector
                        );
                    }
                    let error = map_error(&server, error.status(), &format_args!("{error:#}"));
                    if error.to_string().contains("lacks the required scope") {
                        anyhow::anyhow!(
                            "The stored token for '{}' is not an admin token. Store one with \
                             boot set-token {} --token-stdin, or use boot admin --api-db on the server host.",
                            server_name,
                            server_name
                        )
                    } else {
                        error
                    }
                })?
                .into_inner()
                .revoked as u32
        }
    } else {
        let path = resolve_api_db_path(api_db)?;
        let db = ApiDb::open(&path).await?;
        let result = if args.all {
            db.revoke_all().await
        } else {
            db.revoke_token(args.selector.as_deref().unwrap()).await
        };
        result.map_err(|error| anyhow::anyhow!(error))? as u32
    };

    println!("Revoked {} token(s)", revoked);
    Ok(())
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
