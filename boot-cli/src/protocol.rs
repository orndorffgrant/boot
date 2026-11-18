// SPDX-License-Identifier: AGPL-3.0-or-later
//! Conversions between generated client DTOs and shared protocol DTOs.

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::CliError;

pub fn convert<T: DeserializeOwned>(value: impl Serialize) -> Result<T, CliError> {
    let value = serde_json::to_value(value)?;
    serde_json::from_value(value).map_err(CliError::from)
}
