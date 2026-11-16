// SPDX-License-Identifier: AGPL-3.0-or-later

//! Build-time source archive generation.
//!
//! Release builds provide a deterministic archive made with `git archive`.
//! Local builds fall back to a working-tree archive so the binary still offers
//! the source that was actually compiled.

use std::collections::BTreeSet;
use std::env;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::Cursor;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

#[derive(Debug, Default)]
struct SourceMeta {
    version: Option<String>,
    commit: Option<String>,
    dirty: Option<bool>,
    sha256: Option<String>,
}

#[derive(Debug, Default)]
struct DependencyMeta {
    version: Option<String>,
    filename: Option<String>,
    url: Option<String>,
    sha256: Option<String>,
    targets: Option<String>,
    packages: Option<usize>,
}

fn main() {
    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let workspace = manifest_dir
        .parent()
        .expect("boot-api lives in the workspace root")
        .to_path_buf();
    emit_rerun_directives(&workspace);

    if env::var_os("CARGO_FEATURE_RUN").is_none() {
        return;
    }

    let version = env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION is set by Cargo");
    let required = env_flag("BOOT_SOURCE_REQUIRED");

    let archive = match find_provided_archive(&workspace, required) {
        Some(path) => {
            let bytes = fs::read(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            validate_archive(&bytes, &version, &path);
            let meta = read_meta(&path);
            if let Some(meta_version) = &meta.version {
                if meta_version != &version {
                    panic!(
                        "source archive {} is version {meta_version}, expected {version}",
                        path.display()
                    );
                }
            }
            let sha256 = sha256_hex(&bytes);
            if let Some(expected) = &meta.sha256 {
                if expected != &sha256 {
                    panic!(
                        "source archive {} has SHA-256 {sha256}, metadata says {expected}",
                        path.display()
                    );
                }
            }
            let (git_commit, git_dirty) = git_state(&workspace);
            write_metadata(
                &version,
                meta.commit.as_deref().unwrap_or(&git_commit),
                meta.dirty.unwrap_or(git_dirty),
                &sha256,
            );
            bytes
        }
        None if required => {
            panic!(
                "BOOT_SOURCE_REQUIRED is set but no source archive was found; \
                 run tools/source-archive.sh or set BOOT_SOURCE_TARBALL"
            );
        }
        None => {
            let bytes = build_workspace_archive(&workspace, &version)
                .unwrap_or_else(|error| panic!("failed to build source archive: {error}"));
            validate_archive(&bytes, &version, Path::new("working tree"));
            let (commit, dirty) = git_state(&workspace);
            let sha256 = sha256_hex(&bytes);
            write_metadata(&version, &commit, dirty, &sha256);
            bytes
        }
    };

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    fs::write(out_dir.join("source.tar"), archive).expect("write embedded source archive");
    write_dependency_metadata(&workspace, &version);
}

fn emit_rerun_directives(workspace: &Path) {
    for variable in [
        "BOOT_DEPENDENCY_SOURCE_META",
        "BOOT_DEPENDENCY_SOURCE_REQUIRED",
        "BOOT_SOURCE_TARBALL",
        "BOOT_SOURCE_REQUIRED",
        "CARGO_TARGET_DIR",
        "GITHUB_SHA",
        "SOURCE_DATE_EPOCH",
    ] {
        println!("cargo:rerun-if-env-changed={variable}");
    }

    for relative in [
        "Cargo.toml",
        "Cargo.lock",
        "Cross.toml",
        "LICENSE",
        "NOTICE",
        "README.md",
        "AGENTS.md",
        "DEPENDENCY-SOURCES.md",
        "deny.toml",
        ".gitignore",
        ".cargo",
        ".github",
        ".sqlx",
        "boot-api",
        "boot-api-client",
        "boot-cli",
        "boot-protocol",
        "docs",
        "tools",
    ] {
        println!(
            "cargo:rerun-if-changed={}",
            workspace.join(relative).display()
        );
    }

    for candidate in [
        workspace.join("target/source/boot-source.tar"),
        workspace.join("target/source/boot-source.meta"),
        workspace.join("target/source/boot-dependencies.meta"),
    ] {
        println!("cargo:rerun-if-changed={}", candidate.display());
    }
}

fn env_flag(name: &str) -> bool {
    env::var(name)
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

fn find_provided_archive(workspace: &Path, required: bool) -> Option<PathBuf> {
    if let Some(value) = env::var_os("BOOT_SOURCE_TARBALL") {
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        if !path.is_file() {
            panic!("BOOT_SOURCE_TARBALL does not exist: {}", path.display());
        }
        return Some(path);
    }

    // Do not silently pick up a stale release archive for a local build. The
    // target-directory fallback is only used when release mode requires it.
    if !required {
        return None;
    }

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.join("target"));

    let path = target_dir.join("source/boot-source.tar");
    path.is_file().then_some(path)
}

fn read_meta(archive: &Path) -> SourceMeta {
    let candidates = [
        archive.with_extension("meta"),
        archive
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("boot-source.meta"),
    ];
    let Some(path) = candidates.into_iter().find(|path| path.is_file()) else {
        return SourceMeta::default();
    };

    let text = fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let mut meta = SourceMeta::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "version" => meta.version = Some(value.to_string()),
            "commit" => meta.commit = Some(value.to_string()),
            "dirty" => meta.dirty = Some(value == "true"),
            "sha256" => meta.sha256 = Some(value.to_string()),
            _ => {}
        }
    }
    meta
}

fn write_dependency_metadata(workspace: &Path, version: &str) {
    let required = env_flag("BOOT_DEPENDENCY_SOURCE_REQUIRED");
    let meta = find_dependency_meta(workspace, version, required)
        .map(|path| read_dependency_meta(&path))
        .unwrap_or_default();

    if let Some(meta_version) = &meta.version {
        if meta_version != version {
            panic!("dependency source metadata is version {meta_version}, expected {version}");
        }
    }

    let filename = meta
        .filename
        .unwrap_or_else(|| format!("boot-{version}-dependencies.tar.xz"));
    let url = meta.url.unwrap_or_default();
    let sha256 = meta.sha256.unwrap_or_default();
    let targets = meta.targets.unwrap_or_default();
    let packages = meta.packages.unwrap_or_default();

    if !sha256.is_empty()
        && (sha256.len() != 64 || !sha256.chars().all(|ch| ch.is_ascii_hexdigit()))
    {
        panic!("dependency source SHA-256 is malformed: {sha256}");
    }
    if !url.is_empty() && sha256.is_empty() {
        panic!("dependency source URL is set but its SHA-256 is missing");
    }
    if required && (url.is_empty() || sha256.is_empty()) {
        panic!(
            "BOOT_DEPENDENCY_SOURCE_REQUIRED is set but no dependency source metadata \
             was found; run tools/dependency-source.sh or set BOOT_DEPENDENCY_SOURCE_META"
        );
    }

    println!("cargo:rustc-env=BOOT_DEPENDENCY_SOURCE_URL={url}");
    println!("cargo:rustc-env=BOOT_DEPENDENCY_SOURCE_SHA256={sha256}");
    println!("cargo:rustc-env=BOOT_DEPENDENCY_SOURCE_FILENAME={filename}");
    println!("cargo:rustc-env=BOOT_DEPENDENCY_SOURCE_TARGETS={targets}");
    println!("cargo:rustc-env=BOOT_DEPENDENCY_SOURCE_PACKAGES={packages}");
}

fn find_dependency_meta(workspace: &Path, version: &str, required: bool) -> Option<PathBuf> {
    if let Some(value) = env::var_os("BOOT_DEPENDENCY_SOURCE_META") {
        let path = PathBuf::from(value);
        let path = if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        };
        if !path.is_file() {
            panic!(
                "BOOT_DEPENDENCY_SOURCE_META does not exist: {}",
                path.display()
            );
        }
        return Some(path);
    }

    if !required {
        return None;
    }

    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| {
            if path.is_absolute() {
                path
            } else {
                workspace.join(path)
            }
        })
        .unwrap_or_else(|| workspace.join("target"));

    for candidate in [
        target_dir.join("source/boot-dependencies.meta"),
        target_dir
            .join("source")
            .join(format!("boot-{version}-dependencies.meta")),
        workspace
            .join("target/source")
            .join("boot-dependencies.meta"),
    ] {
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn read_dependency_meta(path: &Path) -> DependencyMeta {
    let text = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    let mut meta = DependencyMeta::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "version" => meta.version = Some(value.to_string()),
            "filename" => meta.filename = Some(value.to_string()),
            "url" => meta.url = Some(value.to_string()),
            "sha256" => meta.sha256 = Some(value.to_string()),
            "targets" => meta.targets = Some(value.to_string()),
            "packages" => meta.packages = value.parse().ok(),
            _ => {}
        }
    }
    meta
}

fn write_metadata(version: &str, commit: &str, dirty: bool, sha256: &str) {
    let commit = if commit.trim().is_empty() {
        "unknown"
    } else {
        commit.trim()
    };
    println!("cargo:rustc-env=BOOT_SOURCE_FILENAME=boot-{version}-source.tar");
    println!("cargo:rustc-env=BOOT_SOURCE_SHA256={sha256}");
    println!("cargo:rustc-env=BOOT_SOURCE_REVISION={commit}");
    println!(
        "cargo:rustc-env=BOOT_SOURCE_DIRTY={}",
        if dirty { "true" } else { "false" }
    );
}

fn git_state(workspace: &Path) -> (String, bool) {
    let commit = git_output(workspace, &["rev-parse", "HEAD"])
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let status = git_output(workspace, &["status", "--porcelain"]);
    match (commit, status) {
        (Some(commit), Some(status)) => (commit, !status.trim().is_empty()),
        (Some(commit), None) => (commit, false),
        (None, _) => ("unknown".to_string(), true),
    }
}

fn git_output(workspace: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn build_workspace_archive(workspace: &Path, version: &str) -> std::io::Result<Vec<u8>> {
    let paths = source_paths(workspace)?;
    if paths.is_empty() {
        return Err(std::io::Error::other("no source files found"));
    }

    let mtime = env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let prefix = PathBuf::from(format!("boot-{version}"));
    let mut builder = tar::Builder::new(Vec::new());

    for relative in paths {
        let full = workspace.join(&relative);
        let metadata = fs::symlink_metadata(&full)?;
        if !metadata.file_type().is_file() {
            continue;
        }

        let mut header = tar::Header::new_ustar();
        header.set_size(metadata.len());
        header.set_mode(file_mode(&metadata));
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(mtime);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();

        let archive_path = prefix.join(&relative);
        let mut file = File::open(&full)?;
        builder.append_data(&mut header, &archive_path, &mut file)?;
    }

    builder.into_inner()
}

fn source_paths(workspace: &Path) -> std::io::Result<BTreeSet<PathBuf>> {
    if let Some(paths) = git_source_paths(workspace) {
        return Ok(paths);
    }

    let mut paths = BTreeSet::new();
    walk_source_tree(workspace, workspace, &mut paths)?;
    Ok(paths)
}

fn git_source_paths(workspace: &Path) -> Option<BTreeSet<PathBuf>> {
    let tracked = git_paths(workspace, &["ls-files", "-z", "--cached"])?;
    let untracked = git_paths(
        workspace,
        &["ls-files", "-z", "--others", "--exclude-standard"],
    )
    .unwrap_or_default();

    let mut paths = BTreeSet::new();
    for relative in tracked {
        if workspace.join(&relative).is_file() {
            paths.insert(relative);
        }
    }
    for relative in untracked {
        if !is_forbidden(&relative) && workspace.join(&relative).is_file() {
            paths.insert(relative);
        }
    }
    Some(paths)
}

fn git_paths(workspace: &Path, args: &[&str]) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    Some(
        output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|bytes| !bytes.is_empty())
            .map(|bytes| PathBuf::from(String::from_utf8_lossy(bytes).into_owned()))
            .collect(),
    )
}

fn walk_source_tree(
    root: &Path,
    directory: &Path,
    paths: &mut BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .expect("walked path is inside the workspace")
            .to_path_buf();
        let file_type = entry.file_type()?;

        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            if !is_forbidden(&relative) {
                walk_source_tree(root, &path, paths)?;
            }
        } else if file_type.is_file() && !is_forbidden(&relative) {
            paths.insert(relative);
        }
    }
    Ok(())
}

fn is_forbidden(relative: &Path) -> bool {
    for component in relative.components() {
        if let Component::Normal(name) = component {
            if name == "target" || name == ".git" || name == "dist" || name == "ext" {
                return true;
            }
        }
    }

    let Some(name) = relative.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if name == ".DS_Store" || name == "pax_global_header" || name.starts_with(".env") {
        return true;
    }
    if name.ends_with("-wal") || name.ends_with("-shm") {
        return true;
    }

    let extension = relative
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "sqlite3" | "sqlite" | "db" | "key" | "pem" | "crt" | "log" | "cast"
    )
}

fn file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 != 0 {
            return 0o755;
        }
    }
    0o644
}

fn validate_archive(bytes: &[u8], version: &str, source: &Path) {
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let prefix = format!("boot-{version}/");
    let mut entries = BTreeSet::new();

    let archive_entries = archive.entries().unwrap_or_else(|error| {
        panic!(
            "source archive {} is not a readable tar: {error}",
            source.display()
        )
    });
    for entry in archive_entries {
        let entry = entry.unwrap_or_else(|error| {
            panic!(
                "source archive {} has an unreadable entry: {error}",
                source.display()
            )
        });
        let path = entry.path().unwrap_or_else(|error| {
            panic!(
                "source archive {} has an invalid path: {error}",
                source.display()
            )
        });
        let path = path.to_string_lossy().replace('\\', "/");
        if let Some(rest) = path.strip_prefix(&prefix) {
            entries.insert(rest.to_string());
        }
    }

    for required in [
        "LICENSE",
        "NOTICE",
        "Cargo.toml",
        "Cargo.lock",
        "boot-api/build.rs",
        "boot-cli/src/main.rs",
    ] {
        if !entries.contains(required) {
            panic!(
                "source archive {} is missing required file {prefix}{required}",
                source.display()
            );
        }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(output, "{byte:02x}").expect("write to string");
    }
    output
}
