use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use boot_protocol::Artifact;

use crate::runtime::{RuntimeError, RuntimeInner, new_id, now_rfc3339, now_unix};
use crate::store::ArtifactRecord;
use crate::store::RuntimeStore;

const ARTIFACT_TTL_SECONDS: i64 = 3600;
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;

pub async fn create(inner: &RuntimeInner, bytes: &[u8]) -> Result<Artifact, RuntimeError> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(RuntimeError::new(
            "invalid_request",
            "artifact exceeds 64 MiB limit",
        ));
    }
    let id = new_id("art");
    let dir = &inner.config.artifact_dir;
    fs::create_dir_all(dir)
        .with_context(|| format!("failed to create {}", dir.display()))
        .map_err(|error| RuntimeError::internal(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("{id}.bin"));
    let mut file =
        fs::File::create(&path).map_err(|error| RuntimeError::internal(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    file.write_all(bytes)
        .map_err(|error| RuntimeError::internal(error.to_string()))?;
    let digest = Sha256::digest(bytes);
    let sha256: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let created_at = now_unix();
    let expires_at = created_at + ARTIFACT_TTL_SECONDS;
    let record = ArtifactRecord {
        id: id.clone(),
        path: path.to_string_lossy().to_string(),
        size: bytes.len() as i64,
        sha256: sha256.clone(),
        created_at,
        expires_at,
    };
    inner.store.insert_artifact(&record).await?;
    Ok(Artifact {
        id,
        size: bytes.len() as u64,
        sha256,
        created_at: now_rfc3339(),
        expires_at: crate::runtime::rfc3339_from_unix(expires_at),
    })
}

pub async fn path(inner: &RuntimeInner, id: &str) -> Result<PathBuf, RuntimeError> {
    let record = inner
        .store
        .load_artifact(id)
        .await?
        .ok_or_else(|| RuntimeError::not_found(format!("artifact '{id}' not found")))?;
    if record.expires_at <= now_unix() {
        return Err(RuntimeError::new(
            "artifact_expired",
            format!("artifact '{id}' has expired"),
        ));
    }
    Ok(PathBuf::from(record.path))
}

pub async fn get(inner: &RuntimeInner, id: &str) -> Result<Artifact, RuntimeError> {
    let record = inner
        .store
        .load_artifact(id)
        .await?
        .ok_or_else(|| RuntimeError::not_found(format!("artifact '{id}' not found")))?;
    Ok(Artifact {
        id: record.id,
        size: record.size as u64,
        sha256: record.sha256,
        created_at: crate::runtime::rfc3339_from_unix(record.created_at),
        expires_at: crate::runtime::rfc3339_from_unix(record.expires_at),
    })
}

pub async fn delete(inner: &RuntimeInner, id: &str) -> Result<(), RuntimeError> {
    if let Some(record) = inner.store.load_artifact(id).await? {
        let _ = fs::remove_file(&record.path);
        inner.store.delete_artifact(id).await?;
    }
    Ok(())
}

pub async fn cleanup(inner: &RuntimeInner) -> Result<(), RuntimeError> {
    let records = inner.store.expired_artifacts(now_unix()).await?;
    for record in records {
        let _ = fs::remove_file(&record.path);
        inner.store.delete_artifact(&record.id).await?;
    }
    Ok(())
}
