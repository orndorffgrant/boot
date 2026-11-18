// SPDX-License-Identifier: AGPL-3.0-or-later
//! Self-update mechanics: release lookup, download, verification, replacement.

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Client, StatusCode, Url};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::CliError;
use crate::local::systemd;
use crate::plan::{ReleaseAsset, ReleaseInfo};

const DEFAULT_REPO: &str = "orndorffgrant/boot";
const API_BASE: &str = "https://api.github.com";
const MAX_ASSET_BYTES: u64 = 200 * 1024 * 1024;

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

pub async fn fetch_release(
    repo: Option<&str>,
    version: Option<&str>,
) -> Result<ReleaseInfo, CliError> {
    let repo = resolve_repo(repo)?;
    let requested_tag = version.map(normalize_tag);
    let current = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("the compiled boot version is not valid semver")?;
    let api = api_client()?;
    let release = fetch_release_json(&api, &repo, requested_tag.as_deref()).await?;
    let available = parse_tag_version(&release.tag_name)?;
    Ok(ReleaseInfo {
        current: current.to_string(),
        available: available.to_string(),
        tag: release.tag_name,
        html_url: release.html_url,
        prerelease: release.prerelease,
        assets: release
            .assets
            .into_iter()
            .map(|asset| ReleaseAsset {
                name: asset.name,
                url: asset.browser_download_url,
                size: asset.size,
            })
            .collect(),
    })
}

pub async fn install_release(
    release: &ReleaseInfo,
    force: bool,
    no_restart: bool,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<String, CliError> {
    let current = Version::parse(&release.current)
        .context("the installed boot version is not valid semver")?;
    let available = Version::parse(&release.available)
        .context("the selected release version is not valid semver")?;
    if available < current && !force {
        return Err(CliError::config(format!(
            "refusing to downgrade from {current} to {available}; pass --force to allow it"
        )));
    }
    if available == current && !force {
        return Err(CliError::config(format!(
            "{current} is already installed; pass --force to reinstall it"
        )));
    }
    if release.prerelease {
        eprintln!("warning: {available} is a pre-release.");
    }

    let executable = current_executable()?;
    let directory = executable
        .parent()
        .ok_or_else(|| CliError::config("cannot determine the install directory"))?;
    check_writable(directory)?;

    let target = current_target()?;
    let binary_name = format!("boot-{target}");
    let checksum_name = format!("{binary_name}.sha256");
    let binary_asset = find_asset(release, &binary_name)?;
    let checksum_asset = find_asset(release, &checksum_name)?;

    let service_active = systemd::is_service_active()?;
    let service_executable = systemd::service_executable()?;
    if let Some(service_path) = &service_executable {
        let service_path = service_path
            .canonicalize()
            .unwrap_or_else(|_| service_path.clone());
        if service_path != executable {
            eprintln!(
                "warning: the boot-api unit runs {}, which differs from the invoking binary {}.",
                service_path.display(),
                executable.display()
            );
        }
    }

    eprintln!("This will replace {}.", executable.display());
    match service_active {
        Some(true) if no_restart => {
            eprintln!("--no-restart: boot-api will keep running the old binary until restarted.");
        }
        Some(true) => {
            eprintln!(
                "boot-api will restart; in-flight flash, debug, and serial operations will be interrupted."
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

    let download = download_client()?;
    let checksum_text =
        String::from_utf8(download_asset(&download, checksum_asset, progress).await?)
            .context("the checksum asset is not valid UTF-8")?;
    let expected = parse_checksum(&checksum_text)?;
    let bytes = download_asset(&download, binary_asset, progress).await?;
    let actual = sha256_hex(&bytes);
    if actual != expected {
        return Err(CliError::config(format!(
            "checksum mismatch for {binary_name}: expected {expected}, got {actual}; nothing was changed"
        )));
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

    if service_active == Some(true) && !no_restart {
        if let Err(restart_error) = systemd::restart_service() {
            return rollback(
                backup,
                &executable,
                restart_error,
                service_active == Some(true),
            );
        }
    }

    backup.disarm();
    Ok(available.to_string())
}

fn rollback(
    backup: TempFile,
    executable: &Path,
    restart_error: anyhow::Error,
    service_was_active: bool,
) -> Result<String, CliError> {
    match fs::rename(backup.path(), executable) {
        Ok(()) => {
            backup.disarm();
            if service_was_active {
                match systemd::restart_service() {
                    Ok(()) => {
                        return Err(CliError::config(format!(
                            "boot-api failed to restart: {restart_error}. Rolled back and restarted the previous binary."
                        )));
                    }
                    Err(old_error) => {
                        return Err(CliError::config(format!(
                            "boot-api failed to restart: {restart_error}. Rolled back, but restarting the old service also failed: {old_error}. Restart boot-api manually."
                        )));
                    }
                }
            }
            Err(CliError::config(format!(
                "boot-api failed to restart: {restart_error}. Rolled back to the previous binary."
            )))
        }
        Err(rollback_error) => {
            let backup_path = backup.disarm();
            Err(CliError::config(format!(
                "boot-api failed to restart: {restart_error}. Rollback also failed: {rollback_error}. The previous binary is saved at {}. Restore it with: mv {} {}",
                backup_path.display(),
                backup_path.display(),
                executable.display()
            )))
        }
    }
}

fn resolve_repo(explicit: Option<&str>) -> Result<String, CliError> {
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
        return Err(CliError::config(format!(
            "invalid repository '{repo}'; expected OWNER/REPO"
        )));
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

fn parse_tag_version(tag: &str) -> Result<Version, CliError> {
    let version = tag.strip_prefix('v').unwrap_or(tag);
    Version::parse(version)
        .map_err(|_| CliError::config(format!("release tag '{tag}' is not valid semver")))
}

fn current_executable() -> Result<PathBuf, CliError> {
    let executable = env::current_exe().map_err(|error| {
        CliError::config(format!(
            "failed to determine the current executable: {error}"
        ))
    })?;
    Ok(executable.canonicalize().unwrap_or(executable))
}

fn current_target() -> Result<&'static str, CliError> {
    if !cfg!(target_env = "gnu") {
        return Err(CliError::config("boot update only supports glibc targets"));
    }
    match (env::consts::ARCH, env::consts::OS) {
        ("x86_64", "linux") => Ok("x86_64-unknown-linux-gnu"),
        ("aarch64", "linux") => Ok("aarch64-unknown-linux-gnu"),
        (arch, os) => Err(CliError::config(format!(
            "unsupported platform {arch}-{os}; supported targets are x86_64-unknown-linux-gnu and aarch64-unknown-linux-gnu"
        ))),
    }
}

fn check_writable(directory: &Path) -> Result<(), CliError> {
    let probe = TempFile::create(directory, "probe").map_err(|error| {
        CliError::config(format!(
            "{} is not writable; run the update with sudo or install boot in a writable directory: {error}",
            directory.display()
        ))
    })?;
    drop(probe);
    Ok(())
}

fn api_client() -> Result<Client, CliError> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        if !token.trim().is_empty() {
            let value = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
                .map_err(|_| CliError::config("GITHUB_TOKEN contains invalid header characters"))?;
            headers.insert(AUTHORIZATION, value);
        }
    }
    Client::builder()
        .user_agent(format!("boot/{}", env!("CARGO_PKG_VERSION")))
        .default_headers(headers)
        .build()
        .map_err(|error| {
            CliError::config(format!("failed to build the GitHub API client: {error}"))
        })
}

fn download_client() -> Result<Client, CliError> {
    Client::builder()
        .user_agent(format!("boot/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| CliError::config(format!("failed to build the download client: {error}")))
}

async fn fetch_release_json(
    client: &Client,
    repo: &str,
    tag: Option<&str>,
) -> Result<Release, CliError> {
    let url = match tag {
        Some(tag) => format!("{API_BASE}/repos/{repo}/releases/tags/{tag}"),
        None => format!("{API_BASE}/repos/{repo}/releases/latest"),
    };
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|error| CliError::config(format!("failed to query {url}: {error}")))?;
    let status = response.status();
    let body = response.text().await.map_err(|error| {
        CliError::config(format!("failed to read the GitHub API response: {error}"))
    })?;
    if status.is_success() {
        return serde_json::from_str(&body).map_err(|error| {
            CliError::config(format!("failed to parse the GitHub API response: {error}"))
        });
    }
    match status {
        StatusCode::NOT_FOUND => match tag {
            Some(tag) => Err(CliError::config(format!(
                "release tag '{tag}' was not found in {repo}"
            ))),
            None => Err(CliError::config(format!(
                "no releases were found in {repo}"
            ))),
        },
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => Err(CliError::config(
            "GitHub API request failed; the rate limit may be exhausted. Set GITHUB_TOKEN to raise the limit.",
        )),
        _ => Err(CliError::config(format!(
            "GitHub API request failed with {status}: {}",
            body.trim().chars().take(300).collect::<String>()
        ))),
    }
}

fn find_asset<'a>(release: &'a ReleaseInfo, name: &str) -> Result<&'a ReleaseAsset, CliError> {
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
            CliError::config(format!(
                "release {} does not contain asset '{name}'; available assets: {}",
                release.tag,
                if available.is_empty() {
                    "(none)"
                } else {
                    available.as_str()
                }
            ))
        })
}

async fn download_asset(
    client: &Client,
    asset: &ReleaseAsset,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<Vec<u8>, CliError> {
    let url = Url::parse(&asset.url)
        .map_err(|_| CliError::config(format!("invalid asset URL '{}'", asset.url)))?;
    if url.scheme() != "https" {
        return Err(CliError::config(format!(
            "refusing to download {} over {}",
            asset.name,
            url.scheme()
        )));
    }
    if asset.size > MAX_ASSET_BYTES {
        return Err(CliError::config(format!(
            "asset {} is {} bytes, larger than the {} byte limit",
            asset.name, asset.size, MAX_ASSET_BYTES
        )));
    }
    let response =
        client.get(url).send().await.map_err(|error| {
            CliError::config(format!("failed to download {}: {error}", asset.name))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(CliError::config(format!(
            "failed to download {}: HTTP {status}",
            asset.name
        )));
    }
    let total = response.content_length().or(Some(asset.size));
    progress(0, total);
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            CliError::config(format!("failed while downloading {}: {error}", asset.name))
        })?;
        bytes.extend_from_slice(&chunk);
        progress(bytes.len() as u64, total);
        if bytes.len() as u64 > MAX_ASSET_BYTES {
            return Err(CliError::config(format!(
                "asset {} exceeded the {} byte limit",
                asset.name, MAX_ASSET_BYTES
            )));
        }
    }
    Ok(bytes)
}

fn parse_checksum(text: &str) -> Result<String, CliError> {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .ok_or_else(|| CliError::config("the checksum asset is empty"))?;
    let checksum = line
        .split_whitespace()
        .next()
        .ok_or_else(|| CliError::config("the checksum asset is malformed"))?
        .to_ascii_lowercase();
    if checksum.len() != 64
        || !checksum
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Err(CliError::config(
            "the checksum asset is malformed: expected a 64-character SHA-256 digest",
        ));
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

fn validate_binary(path: &Path, expected: &Version) -> Result<(), CliError> {
    let output = Command::new(path)
        .arg("--version")
        .output()
        .map_err(|error| {
            CliError::config(format!("failed to execute {}: {error}", path.display()))
        })?;
    if !output.status.success() {
        return Err(CliError::config(format!(
            "the downloaded binary failed to run --version: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let reported = stdout
        .split_whitespace()
        .last()
        .ok_or_else(|| CliError::config("the downloaded binary printed no version"))?;
    let reported = parse_tag_version(reported)?;
    if &reported != expected {
        return Err(CliError::config(format!(
            "the downloaded binary reports {reported}, but the release is {expected}"
        )));
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
