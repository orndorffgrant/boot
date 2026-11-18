// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local server provisioning and foreground startup.

use std::path::Path;

use boot_api::auth::{CreateTokenRequest, Scopes};
use boot_api::store::{Store, resolve_api_db_path};

use crate::domain::ServerOptions;
use crate::error::CliError;
use crate::local::cli_db::{CliDb, resolve_cli_db_path};
use crate::local::systemd::ServiceConfig;
use crate::plan::LocalServer;

pub struct Prepared {
    pub local: LocalServer,
    pub warnings: Vec<String>,
}

pub async fn prepare(
    options: &ServerOptions,
    cli_db: Option<&Path>,
    api_db: Option<&Path>,
) -> Result<Prepared, CliError> {
    let host = options.host.as_deref().unwrap_or("127.0.0.1");
    if !boot_api::is_loopback_host(host)
        && options.tls_cert.is_none()
        && !options.allow_insecure_http
    {
        return Err(CliError::config(format!(
            "refusing to bind {host} without TLS; pass --tls-cert/--tls-key or --allow-insecure-http (only on an encrypted network)"
        )));
    }

    let api_db_path = resolve_api_db_path(api_db)?;
    let cli_db_path = resolve_cli_db_path(cli_db)?;
    let db = Store::open(&api_db_path).await.map_err(|error| {
        CliError::config(format!(
            "Failed to open API database {}: {error}",
            api_db_path.display()
        ))
    })?;
    let config = CliDb::open(&cli_db_path).await?;
    let mut warnings = Vec::new();

    if !options.no_provision {
        let url = advertised_url(
            options.host.as_deref(),
            options.port.unwrap_or(46673),
            options.tls_cert.is_some(),
            options.advertise_url.as_deref(),
        );
        config.upsert_server("local", &url, None, None).await?;

        let has_credential = config
            .get_server("local")
            .await?
            .and_then(|server| server.token)
            .is_some();

        if !db.has_tokens().await.map_err(boot_error)? {
            let created = db
                .create_token(CreateTokenRequest {
                    name: "local".to_string(),
                    scopes: Scopes::admin(),
                    expires_at: None,
                    replace: false,
                })
                .await
                .map_err(boot_error)?;
            match config.set_token("local", &created.token).await {
                Ok(()) => warnings.push(format!(
                    "Provisioned admin token 'local' and stored it in {}",
                    cli_db_path.display()
                )),
                Err(error) => warnings.push(format!(
                    "Created the local admin token but could not store it: {error}. Recover with: boot create-token --name local-admin --scope admin --save-to local"
                )),
            }
        } else if !db.has_admin_token().await.map_err(boot_error)? {
            warnings.push(
                "The API database has tokens but no admin token. Create one with: boot create-token --name local-admin --scope admin"
                    .to_string(),
            );
        } else if !has_credential {
            warnings.push(
                "The local server entry has no stored credential. Use: boot create-token --name local-admin --scope admin --save-to local, or boot edit-server local --token-stdin"
                    .to_string(),
            );
        }
    }

    Ok(Prepared {
        local: LocalServer {
            cli_db: cli_db_path,
            api_db: api_db_path,
        },
        warnings,
    })
}

pub fn service_config(options: &ServerOptions, local: &LocalServer) -> ServiceConfig {
    ServiceConfig {
        host: options.host.clone(),
        port: options.port,
        api_db: local.api_db.clone(),
        cli_db: local.cli_db.clone(),
        no_provision: options.no_provision,
        tls_cert: options.tls_cert.clone(),
        tls_key: options.tls_key.clone(),
        allow_insecure_http: options.allow_insecure_http,
        advertise_url: options.advertise_url.clone(),
    }
}

pub async fn run_server(options: &ServerOptions, local: &LocalServer) -> Result<(), CliError> {
    boot_api::run(boot_api::RunOptions {
        host: options.host.clone(),
        port: options.port,
        api_db: Some(local.api_db.clone()),
        tls_cert: options.tls_cert.clone(),
        tls_key: options.tls_key.clone(),
        allow_insecure_http: options.allow_insecure_http,
    })
    .await
    .map_err(CliError::from)
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
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("{}://{}:{}", if tls { "https" } else { "http" }, host, port)
}

fn boot_error(error: impl std::fmt::Display) -> CliError {
    CliError::config(error.to_string())
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
    }
}
