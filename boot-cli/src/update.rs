//! `boot admin update` — replace the locally installed binary with a release
//! published on GitHub.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Args;
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::admin::confirm;
use crate::systemd_service;

const DEFAULT_REPO: &str = "orndorffgrant/boot";
const API_BASE: &str = "https://api.github.com";
const MAX_ASSET_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Args)]
pub struct UpdateArgs {
    /// Report the installed and available versions without changing anything
    #[arg(long)]
    pub check: bool,
    /// Install a specific release tag instead of the latest release
    #[arg(long, value_name = "TAG")]
    pub version: Option<String>,
    /// Reinstall the same version or allow a downgrade
    #[arg(long)]
    pub force: bool,
    /// Skip the confirmation prompt
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Replace the binary but leave the running service on the old process
    #[arg(long)]
    pub no_restart: bool,
    /// Use a fork or mirror (OWNER/REPO)
    #[arg(long, value_name = "OWNER/REPO")]
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

pub async fn update(args: UpdateArgs) -> Result<()> {
    let repo = resolve_repo(args.repo.as_deref())?;
    let requested_tag = args.version.as_deref().map(normalize_tag);
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the compiled boot version is not valid semver")?;

    let api = api_client()?;
    let release = fetch_release(&api, &repo, requested_tag.as_deref()).await?;
    let available = parse_tag_version(&release.tag_name)?;

    println!("Installed: {current}");
    println!("Available: {available}");
    println!("Release:   {}", release.html_url);

    if args.check {
        if available > current {
            println!("Update available.");
        } else if available == current {
            println!("Already up to date.");
        } else {
            println!("Installed version is newer than the selected release.");
        }
        return Ok(());
    }

    if available < current && !args.force {
        anyhow::bail!(
            "refusing to downgrade from {current} to {available}; pass --force to allow it"
        );
    }
    if available == current && !args.force {
        anyhow::bail!("{current} is already installed; pass --force to reinstall it");
    }
    if release.prerelease {
        eprintln!("Warning: {available} is a pre-release.");
    }

    let executable = current_executable()?;
    let directory = executable
        .parent()
        .ok_or_else(|| anyhow::anyhow!("cannot determine the install directory"))?;
    check_writable(directory)?;

    let target = current_target()?;
    let binary_name = format!("boot-{target}");
    let checksum_name = format!("{binary_name}.sha256");
    let binary_asset = find_asset(&release, &binary_name)?;
    let checksum_asset = find_asset(&release, &checksum_name)?;

    let service_active = systemd_service::is_service_active()?;
    let service_executable = systemd_service::service_executable()?;
    if let Some(service_path) = &service_executable {
        let service_path = service_path
            .canonicalize()
            .unwrap_or_else(|_| service_path.clone());
        if service_path != executable {
            eprintln!(
                "Warning: the boot-api unit runs {}, which differs from the invoking binary {}. \
                 The service may not use the replaced file.",
                service_path.display(),
                executable.display()
            );
        }
    }

    eprintln!("This will replace {}.", executable.display());
    match service_active {
        Some(true) if args.no_restart => {
            eprintln!(
                "--no-restart: boot-api will keep running the old binary until it is restarted \
                 manually."
            );
        }
        Some(true) => {
            eprintln!(
                "boot-api will restart; in-flight flash, debug, and serial operations will be \
                 interrupted."
            );
        }
        Some(false) => {
            eprintln!("boot-api is not active; the binary will be replaced without a restart.");
        }
        None => {
            eprintln!(
                "systemd is unavailable; the binary will be replaced without restarting boot-api."
            );
        }
    }
    confirm("Continue with the update?", args.yes)?;

    let download = download_client()?;
    let checksum_text = String::from_utf8(download_asset(&download, checksum_asset).await?)
        .context("the checksum asset is not valid UTF-8")?;
    let expected = parse_checksum(&checksum_text)?;
    let bytes = download_asset(&download, binary_asset).await?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        anyhow::bail!(
            "checksum mismatch for {binary_name}: expected {expected}, got {actual}; \
             nothing was changed"
        );
    }

    let (new_file, mut file) = TempFile::create_with_file(directory, "new")?;
    file.write_all(&bytes)
        .with_context(|| format!("failed to write {}", new_file.path().display()))?;
    file.sync_all()
        .with_context(|| format!("failed to flush {}", new_file.path().display()))?;
    drop(file);

    let old_permissions = fs::metadata(&executable)
        .with_context(|| format!("failed to read metadata for {}", executable.display()))?
        .permissions();
    fs::set_permissions(new_file.path(), old_permissions)
        .with_context(|| format!("failed to set permissions on {}", new_file.path().display()))?;

    validate_binary(new_file.path(), &available)?;

    let backup = TempFile::create(directory, "backup")?;
    fs::copy(&executable, backup.path()).with_context(|| {
        format!(
            "failed to back up {} to {}",
            executable.display(),
            backup.path().display()
        )
    })?;

    fs::rename(new_file.path(), &executable).with_context(|| {
        format!(
            "failed to replace {} with {}",
            executable.display(),
            new_file.path().display()
        )
    })?;
    new_file.disarm();

    if service_active == Some(true) && !args.no_restart {
        if let Err(restart_error) = systemd_service::restart_service() {
            return rollback(
                backup,
                &executable,
                restart_error,
                service_active == Some(true),
            );
        }
    }

    backup.disarm();
    println!("Updated boot to {available}.");
    if service_active == Some(true) && !args.no_restart {
        println!("boot-api restarted.");
    } else if service_active == Some(true) {
        println!("boot-api is still running the old binary; restart it to activate {available}.");
    }
    Ok(())
}

fn rollback(
    backup: TempFile,
    executable: &Path,
    restart_error: anyhow::Error,
    service_was_active: bool,
) -> Result<()> {
    match fs::rename(backup.path(), executable) {
        Ok(()) => {
            backup.disarm();
            if service_was_active {
                match systemd_service::restart_service() {
                    Ok(()) => anyhow::bail!(
                        "boot-api failed to restart: {restart_error}. Rolled back to the previous \
                         binary and restarted it."
                    ),
                    Err(old_error) => anyhow::bail!(
                        "boot-api failed to restart: {restart_error}. Rolled back to the previous \
                         binary, but restarting the old service also failed: {old_error}. Restart \
                         boot-api manually."
                    ),
                }
            }
            anyhow::bail!(
                "boot-api failed to restart: {restart_error}. Rolled back to the previous binary."
            )
        }
        Err(rollback_error) => {
            let backup_path = backup.disarm();
            anyhow::bail!(
                "boot-api failed to restart: {restart_error}. Rollback also failed: \
                 {rollback_error}. The previous binary is saved at {}. Restore it manually with: \
                 mv {} {}",
                backup_path.display(),
                backup_path.display(),
                executable.display()
            )
        }
    }
}

fn resolve_repo(explicit: Option<&str>) -> Result<String> {
    let repo = explicit
        .map(str::to_string)
        .or_else(|| env::var("BOOT_RELEASE_REPO").ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string());
    let repo = repo.trim();
    let mut parts = repo.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        anyhow::bail!("invalid repository '{repo}'; expected OWNER/REPO");
    }
    Ok(repo.to_string())
}

fn normalize_tag(tag: &str) -> String {
    if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    }
}

fn parse_tag_version(tag: &str) -> Result<Version> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(version).with_context(|| format!("release tag '{tag}' is not valid semver"))
}

fn current_executable() -> Result<PathBuf> {
    let executable = env::current_exe().context("failed to determine the current executable")?;
    Ok(executable.canonicalize().unwrap_or(executable))
}

fn current_target() -> Result<&'static str> {
    if !cfg!(target_env = "gnu") {
        anyhow::bail!("boot admin update only supports glibc targets");
    }
    match (env::consts::ARCH, env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        (arch, os) => anyhow::bail!(
            "unsupported platform {arch}-{os}; supported targets are \
             x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu"
        ),
    }
}

fn check_writable(directory: &Path) -> Result<()> {
    let probe = TempFile::create(directory, "probe").with_context(|| {
        format!(
            "{} is not writable; run the update with sudo or install boot in a writable directory",
            directory.display()
        )
    })?;
    drop(probe);
    Ok(())
}

fn api_client() -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
                .context("GITHUB_TOKEN contains invalid header characters")?;
            headers.insert(AUTHORIZATION, value);
        }
    }
    Client::builder()
        .user_agent(format!("boot/{}", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()
        .context("failed to build the GitHub API client")
}

fn download_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("boot/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .context("failed to build the download client")
}

async fn fetch_release(client: &Client, repo: &str, tag: Option<&str>) -> Result<Release> {
    let url = match tag {
        Some(tag) => format!("{API_BASE}/repos/{repo}/releases/tags/{tag}"),
        None => format!("{API_BASE}/repos/{repo}/releases/latest"),
    };
    let response = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("failed to query {url}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("failed to read the GitHub API response")?;

    if status.is_success() {
        return serde_json::from_str(&body)
            .with_context(|| format!("failed to parse the GitHub API response from {url}"));
    }

    match status {
        StatusCode::NOT_FOUND => match tag {
            Some(tag) => anyhow::bail!("release tag '{tag}' was not found in {repo}"),
            None => anyhow::bail!("no releases were found in {repo}"),
        },
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => anyhow::bail!(
            "GitHub API request failed with {status}; the rate limit may be exhausted. \
             Set GITHUB_TOKEN to raise the limit."
        ),
        _ => anyhow::bail!(
            "GitHub API request failed with {status}: {}",
            body.trim().chars().take(300).collect::<String>()
        ),
    }
}

fn find_asset<'a>(release: &'a Release, name: &str) -> Result<&'a Asset> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .ok_or_else(|| {
            let available = release
                .assets
                .iter()
                .map(|asset| asset.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::anyhow!(
                "release {} does not contain asset '{name}'; available assets: {}",
                release.tag_name,
                if available.is_empty() {
                    "(none)"
                } else {
                    available.as_str()
                }
            )
        })
}

async fn download_asset(client: &Client, asset: &Asset) -> Result<Vec<u8>> {
    let url = Url::parse(&asset.browser_download_url)
        .with_context(|| format!("invalid asset URL '{}'", asset.browser_download_url))?;
    if url.scheme() != "https" {
        anyhow::bail!("refusing to download {} over {}", asset.name, url.scheme());
    }
    if asset.size > MAX_ASSET_BYTES {
        anyhow::bail!(
            "asset {} is {} bytes, larger than the {} byte limit",
            asset.name,
            asset.size,
            MAX_ASSET_BYTES
        );
    }

    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("failed to download {}", asset.name))?;
    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("failed to download {}: HTTP {status}", asset.name);
    }
    if let Some(length) = response.content_length() {
        if length > MAX_ASSET_BYTES {
            anyhow::bail!(
                "asset {} is {} bytes, larger than the {} byte limit",
                asset.name,
                length,
                MAX_ASSET_BYTES
            );
        }
    }

    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("failed while downloading {}", asset.name))?;
        if bytes.len() as u64 + chunk.len() as u64 > MAX_ASSET_BYTES {
            anyhow::bail!(
                "asset {} exceeded the {} byte limit",
                asset.name,
                MAX_ASSET_BYTES
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn parse_checksum(text: &str) -> Result<String> {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("the checksum asset is empty"))?;
    let checksum = line
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow::anyhow!("the checksum asset is malformed"))?
        .to_ascii_lowercase();
    if checksum.len() != 64
        || !checksum
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        anyhow::bail!("the checksum asset is malformed: expected a 64-character SHA-256 digest");
    }
    Ok(checksum)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest.as_slice() {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn validate_binary(path: &Path, expected: &Version) -> Result<()> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to execute {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "the downloaded binary failed to run --version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout
        .split_whitespace()
        .last()
        .ok_or_else(|| anyhow::anyhow!("the downloaded binary printed no version"))?;
    let reported = parse_tag_version(reported)
        .with_context(|| format!("the downloaded binary reported an invalid version: {stdout}"))?;
    if &reported != expected {
        anyhow::bail!("the downloaded binary reports {reported}, but the release is {expected}");
    }
    Ok(())
}

fn temp_path(directory: &Path, kind: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    directory.join(format!(
        ".boot-update-{kind}-{}-{nanos}.tmp",
        std::process::id()
    ))
}

struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn create(directory: &Path, kind: &str) -> Result<Self> {
        let (temp, _file) = Self::create_with_file(directory, kind)?;
        Ok(temp)
    }

    fn create_with_file(directory: &Path, kind: &str) -> Result<(Self, File)> {
        let path = temp_path(directory, kind);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        Ok((Self { path, armed: true }, file))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(mut self) -> PathBuf {
        self.armed = false;
        self.path.clone()
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_tags() {
        assert_eq!(normalize_tag("2026.1103.0"), "v2026.1103.0");
        assert_eq!(normalize_tag("v2026.1103.0"), "v2026.1103.0");
        assert_eq!(
            parse_tag_version("v2026.1103.0").unwrap(),
            Version::new(2026, 1103, 0)
        );
    }

    #[test]
    fn parses_checksums() {
        let digest = "a".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{digest}  boot-x86_64-unknown-linux-gnu\n")).unwrap(),
            digest
        );
        assert!(parse_checksum("").is_err());
        assert!(parse_checksum("not-a-digest  file").is_err());
    }

    #[test]
    fn validates_repository_names() {
        assert_eq!(resolve_repo(Some("owner/repo")).unwrap(), "owner/repo");
        assert!(resolve_repo(Some("owner")).is_err());
        assert!(resolve_repo(Some("owner/repo/extra")).is_err());
    }

    #[test]
    fn hashes_bytes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
