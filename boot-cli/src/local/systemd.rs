// SPDX-License-Identifier: AGPL-3.0-or-later
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

use crate::error::CliError;

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

/// Return a human-readable state for the `boot-api` unit.
///
/// Returns `None` when systemd is unavailable.
pub fn service_status() -> Result<Option<String>> {
    let output = match Command::new("systemctl")
        .args(["is-active", "boot-api"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Failed to execute systemctl is-active"),
    };
    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if state.is_empty() {
        return Ok(None);
    }
    Ok(Some(match state.as_str() {
        "active" => "Running".to_string(),
        "inactive" => "Stopped".to_string(),
        "failed" => "Failure".to_string(),
        "activating" => "Starting".to_string(),
        "deactivating" => "Stopping".to_string(),
        other => other.to_string(),
    }))
}

/// Return the host and port configured in the `boot-api` unit's `ExecStart`.
///
/// Returns `None` when systemd or the unit is unavailable.
pub fn service_bind() -> Result<Option<String>> {
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
    Ok(parse_exec_start_bind(&text))
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

fn parse_exec_start_bind(text: &str) -> Option<String> {
    let argv = text.split("argv[]=").nth(1)?.split(" ;").next()?.trim();
    if argv.is_empty() {
        return None;
    }
    let mut host = None;
    let mut port = None;
    let mut args = argv.split_whitespace();
    while let Some(arg) = args.next() {
        if arg == "--host" {
            host = args.next();
        } else if let Some(value) = arg.strip_prefix("--host=") {
            host = Some(value);
        } else if arg == "--port" {
            port = args.next();
        } else if let Some(value) = arg.strip_prefix("--port=") {
            port = Some(value);
        } else if arg == "-p" {
            port = args.next();
        }
    }
    Some(format_host_port(
        host.unwrap_or("127.0.0.1"),
        port.unwrap_or("46673"),
    ))
}

fn format_host_port(host: &str, port: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
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

pub fn edit_document(initial: &str) -> Result<String, CliError> {
    let temp = TempFile::create(initial)?;
    run_editor(temp.path())?;
    Ok(fs::read_to_string(temp.path())
        .with_context(|| format!("Failed to read {}", temp.path().display()))?)
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

    #[test]
    fn parses_exec_start_bind() {
        let text = "{ path=/usr/local/bin/boot ; argv[]=/usr/local/bin/boot run-server \
                    --host 0.0.0.0 --port 46674 ; ignore_errors=no ; }";
        assert_eq!(
            parse_exec_start_bind(text),
            Some("0.0.0.0:46674".to_string())
        );
        let ipv6 = "{ path=/usr/local/bin/boot ; argv[]=/usr/local/bin/boot run-server \
                     --host ::1 ; }";
        assert_eq!(parse_exec_start_bind(ipv6), Some("[::1]:46673".to_string()));
        let defaults = "{ path=/usr/local/bin/boot ; argv[]=/usr/local/bin/boot run-server ; }";
        assert_eq!(
            parse_exec_start_bind(defaults),
            Some("127.0.0.1:46673".to_string())
        );
        assert_eq!(parse_exec_start_bind(""), None);
    }
}
