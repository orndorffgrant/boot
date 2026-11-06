use std::ffi::OsString;
use std::io;
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

/// Report whether the `boot-api` unit is active.
///
/// Returns `None` when systemd is not available, which lets the updater work on
/// hosts that do not run the service under systemd.
pub fn is_service_active() -> Result<Option<bool>> {
    match Command::new("systemctl")
        .args(["is-active", "--quiet", "boot-api"])
        .status()
    {
        Ok(status) => Ok(Some(status.success())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to execute systemctl is-active"),
    }
}

/// Return the executable path configured in the `boot-api` unit, if it can be
/// determined.
pub fn service_executable() -> Result<Option<PathBuf>> {
    let output = match Command::new("systemctl")
        .args(["show", "boot-api", "--property=ExecStart", "--value"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Failed to execute systemctl show"),
    };

    if !output.status.success() {
        return Ok(None);
    }

    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_exec_start(&text))
}

fn parse_exec_start(text: &str) -> Option<PathBuf> {
    // Example: `{ path=/usr/local/bin/boot ; argv[]=/usr/local/bin/boot run-server ; ... }`
    let start = text.find("path=")? + "path=".len();
    let rest = &text[start..];
    let end = rest.find(';').unwrap_or(rest.len());
    let path = rest[..end].trim();
    if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exec_start_path() {
        let text = "{ path=/usr/local/bin/boot ; argv[]=/usr/local/bin/boot run-server ; \
                    ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; \
                    code=(null) ; status=0/0 }";
        assert_eq!(
            parse_exec_start(text),
            Some(PathBuf::from("/usr/local/bin/boot"))
        );
        assert_eq!(parse_exec_start(""), None);
        assert_eq!(parse_exec_start("{ path= ; }"), None);
    }
}
