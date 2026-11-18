// SPDX-License-Identifier: AGPL-3.0-or-later
//! Direct API-database token administration used by `--direct`.

use std::path::Path;

use boot_api::auth::{CreateTokenRequest, CreatedToken, TokenRecord, TokenStore};
use boot_api::store::Store;

use crate::error::CliError;

async fn open(path: &Path) -> Result<Store, CliError> {
    Store::open(path)
        .await
        .map_err(|error| CliError::config(format!("Failed to open API database: {error}")))
}

pub async fn create_token(
    path: &Path,
    request: CreateTokenRequest,
) -> Result<CreatedToken, CliError> {
    let db = open(path).await?;
    db.create_token(request)
        .await
        .map_err(|error| CliError::config(error.to_string()))
}

pub async fn list_tokens(path: &Path) -> Result<Vec<TokenRecord>, CliError> {
    let db = open(path).await?;
    db.list_tokens()
        .await
        .map_err(|error| CliError::config(error.to_string()))
}

pub async fn revoke_token(path: &Path, selector: &str) -> Result<u64, CliError> {
    let db = open(path).await?;
    Ok(db
        .revoke_token(selector)
        .await
        .map_err(|error| CliError::config(error.to_string()))? as u64)
}

pub async fn revoke_all(path: &Path) -> Result<u64, CliError> {
    let db = open(path).await?;
    Ok(db
        .revoke_all()
        .await
        .map_err(|error| CliError::config(error.to_string()))? as u64)
}
