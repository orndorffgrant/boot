// SPDX-License-Identifier: AGPL-3.0-or-later
//! DAP launch-file upload adapter.
//!
//! The debugger resolves paths on its own machine. The bridge uploads those
//! files through the normal artifact API and rewrites the launch request to
//! server-side artifact IDs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};

use super::Api;
use crate::domain::ServerConfig;
use crate::error::CliError;
use crate::protocol::to_frame;
use crate::ui::Ui;

pub struct LaunchUploader<'a> {
    api: &'a dyn Api,
    ui: &'a Arc<dyn Ui>,
    server: &'a ServerConfig,
}

impl<'a> LaunchUploader<'a> {
    pub fn new(api: &'a dyn Api, ui: &'a Arc<dyn Ui>, server: &'a ServerConfig) -> Self {
        Self { api, ui, server }
    }

    pub async fn rewrite(&self, frame: &[u8]) -> Result<Vec<u8>, CliError> {
        let mut message: Value = serde_json::from_slice(frame)
            .map_err(|error| CliError::config(format!("invalid DAP message: {error}")))?;
        if message.get("type").and_then(Value::as_str) != Some("request") {
            return Ok(to_frame(&message));
        }
        let command = message
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if command != "launch" && command != "attach" {
            return Ok(to_frame(&message));
        }
        let Some(args) = message.get_mut("arguments") else {
            return Ok(to_frame(&message));
        };
        if args.get("bootArtifacts").is_some() {
            return Ok(to_frame(&message));
        }
        let cwd = args
            .get("cwd")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let mut boot = serde_json::Map::new();
        if let Some(program) = args.get("program").and_then(Value::as_str) {
            if let Some(id) = self.upload(program, &cwd).await? {
                boot.insert("program".into(), Value::String(id));
            }
        }
        if let Some(svd) = args.get("svdFile").and_then(Value::as_str) {
            if let Some(id) = self.upload(svd, &cwd).await? {
                boot.insert("svd".into(), Value::String(id));
            }
        }
        if let Some(chip) = args.get("chipDescriptionPath").and_then(Value::as_str) {
            if let Some(id) = self.upload(chip, &cwd).await? {
                boot.insert("chipDescription".into(), Value::String(id));
            }
        }
        if let Some(cores) = args.get("coreConfigs").and_then(Value::as_array).cloned() {
            let mut rewritten = Vec::new();
            for mut core in cores {
                if let Some(program) = core.get("programBinary").and_then(Value::as_str) {
                    if let Some(id) = self.upload(program, &cwd).await? {
                        core["bootProgram"] = Value::String(id);
                    }
                }
                if let Some(svd) = core.get("svdFile").and_then(Value::as_str) {
                    if let Some(id) = self.upload(svd, &cwd).await? {
                        core["bootSvd"] = Value::String(id);
                    }
                }
                rewritten.push(core);
            }
            if !rewritten.is_empty() {
                boot.insert("coreConfigs".into(), Value::Array(rewritten));
            }
        }
        if !boot.is_empty() {
            args["bootArtifacts"] = Value::Object(boot);
        }
        Ok(to_frame(&message))
    }

    pub fn error_response(frame: &[u8], message: &str) -> Option<Vec<u8>> {
        let request = serde_json::from_slice::<Value>(frame).ok()?;
        Some(to_frame(&json!({
            "type": "response",
            "request_seq": request.get("seq").and_then(Value::as_i64).unwrap_or(0),
            "success": false,
            "command": request.get("command").and_then(Value::as_str).unwrap_or_default(),
            "message": message,
            "body": {"error": {"id": 1, "format": message}}
        })))
    }

    async fn upload(&self, value: &str, cwd: &Path) -> Result<Option<String>, CliError> {
        if value.starts_with("art_") {
            return Ok(Some(value.to_string()));
        }
        let path = if Path::new(value).is_absolute() {
            PathBuf::from(value)
        } else {
            cwd.join(value)
        };
        if !path.is_file() {
            return Err(CliError::config(format!(
                "DAP launch file '{}' does not exist",
                path.display()
            )));
        }
        self.ui.status(&format!("Uploading {}", path.display()));
        let artifact = self
            .api
            .upload_artifact(self.server, &path, Arc::new(|_, _| {}))
            .await?;
        Ok(Some(artifact.id))
    }
}
