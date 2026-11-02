use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_db: PathBuf,
    pub cli_db: PathBuf,
    pub no_provision: bool,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub allow_insecure_http: bool,
    pub advertise_url: Option<String>,
}

pub fn start_service(config: &ServiceConfig) -> Result<()> {
    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;

    let mut args: Vec<OsString> = vec![
        "--unit".into(),
        "boot-api".into(),
        "--description".into(),
        "Boot API Service".into(),
        "--service-type".into(),
        "exec".into(),
        current_exe.into(),
        "run-server".into(),
        "--api-db".into(),
        config.api_db.clone().into(),
        "--cli-db".into(),
        config.cli_db.clone().into(),
    ];

    if let Some(host) = &config.host {
        args.push("--host".into());
        args.push(host.into());
    }
    if let Some(port) = config.port {
        args.push("--port".into());
        args.push(port.to_string().into());
    }
    if config.no_provision {
        args.push("--no-provision".into());
    }
    if let Some(cert) = &config.tls_cert {
        args.push("--tls-cert".into());
        args.push(cert.clone().into());
    }
    if let Some(key) = &config.tls_key {
        args.push("--tls-key".into());
        args.push(key.clone().into());
    }
    if config.allow_insecure_http {
        args.push("--allow-insecure-http".into());
    }
    if let Some(url) = &config.advertise_url {
        args.push("--advertise-url".into());
        args.push(url.into());
    }

    let output = Command::new("systemd-run")
        .args(args)
        .output()
        .context("Failed to execute systemd-run command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to start systemd service: {}", stderr);
    }

    Ok(())
}

pub fn stop_service() -> Result<()> {
    let output = Command::new("systemctl")
        .args(["stop", "boot-api"])
        .output()
        .context("Failed to execute systemctl stop command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to stop systemd service: {}", stderr);
    }

    Ok(())
}

pub fn restart_service() -> Result<()> {
    let output = Command::new("systemctl")
        .args(["restart", "boot-api"])
        .output()
        .context("Failed to execute systemctl restart command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Failed to restart systemd service: {}", stderr);
    }

    Ok(())
}
