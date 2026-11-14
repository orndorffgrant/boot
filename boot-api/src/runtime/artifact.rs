use std::fs;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use boot_protocol::{Artifact, ProbeArtifact};

use crate::runtime::decode;
use crate::runtime::{RuntimeError, RuntimeInner, new_id, now_rfc3339, now_unix};
use crate::store::{ArtifactRecord, ProbeArtifactRecord, RuntimeStore};

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
    let record = ArtifactRecord {
        id: id.clone(),
        path: path.to_string_lossy().to_string(),
        size: bytes.len() as i64,
        sha256: sha256.clone(),
        created_at,
    };
    inner.store.insert_artifact(&record).await?;
    Ok(Artifact {
        id,
        size: bytes.len() as u64,
        sha256,
        created_at: now_rfc3339(),
        in_use: false,
    })
}

pub async fn path(inner: &RuntimeInner, id: &str) -> Result<PathBuf, RuntimeError> {
    let record = inner
        .store
        .load_artifact(id)
        .await?
        .ok_or_else(|| RuntimeError::not_found(format!("artifact '{id}' not found")))?;
    Ok(PathBuf::from(record.path))
}

pub async fn get(inner: &RuntimeInner, id: &str) -> Result<Artifact, RuntimeError> {
    let record = inner
        .store
        .load_artifact(id)
        .await?
        .ok_or_else(|| RuntimeError::not_found(format!("artifact '{id}' not found")))?;
    let in_use = inner.store.artifact_in_use(id).await?;
    Ok(Artifact {
        id: record.id,
        size: record.size as u64,
        sha256: record.sha256,
        created_at: crate::runtime::rfc3339_from_unix(record.created_at),
        in_use,
    })
}

pub async fn delete(inner: &RuntimeInner, id: &str) -> Result<(), RuntimeError> {
    if inner.store.artifact_in_use(id).await? {
        return Err(RuntimeError::new(
            "artifact_in_use",
            format!("artifact '{id}' is still referenced by a session, probe, or change"),
        ));
    }
    if let Some(record) = inner.store.load_artifact(id).await? {
        let _ = fs::remove_file(&record.path);
        inner.store.delete_artifact(id).await?;
    }
    Ok(())
}

pub async fn restore_probe_artifacts(inner: &RuntimeInner) -> Result<(), RuntimeError> {
    let records = inner.store.load_probe_artifacts().await?;
    let mut map = inner.probe_artifacts.lock().expect("probe artifacts lock");
    map.clear();
    for record in records {
        map.insert(record.resource_id.clone(), from_record(record));
    }
    Ok(())
}

pub fn probe_artifact(inner: &RuntimeInner, resource_id: &str) -> Option<ProbeArtifact> {
    inner
        .probe_artifacts
        .lock()
        .expect("probe artifacts lock")
        .get(resource_id)
        .cloned()
}

pub async fn set_probe_artifact(
    inner: &RuntimeInner,
    resource_id: &str,
    artifact_id: &str,
    change_id: Option<&str>,
    created_by: Option<&str>,
) -> Result<ProbeArtifact, RuntimeError> {
    let profile = decode::load_profile(inner, artifact_id).await?;
    if !profile.usable() {
        clear_probe_artifact(inner, resource_id).await?;
        return Err(RuntimeError::new(
            "invalid_artifact",
            "artifact has neither an RTT control block nor defmt data",
        ));
    }
    let record = ProbeArtifactRecord {
        resource_id: resource_id.to_string(),
        artifact_id: artifact_id.to_string(),
        sha256: profile.sha256.clone(),
        size: profile.size as i64,
        defmt: if profile.table.is_some() { 1 } else { 0 },
        rtt_addr: profile.rtt_addr.map(|address| format!("0x{address:x}")),
        change_id: change_id.map(str::to_string),
        created_by: created_by.map(str::to_string),
        flashed_at: now_unix(),
    };
    inner.store.upsert_probe_artifact(&record).await?;
    let artifact = from_record(record);
    inner
        .probe_artifacts
        .lock()
        .expect("probe artifacts lock")
        .insert(resource_id.to_string(), artifact.clone());
    Ok(artifact)
}

pub async fn clear_probe_artifact(
    inner: &RuntimeInner,
    resource_id: &str,
) -> Result<(), RuntimeError> {
    inner.store.delete_probe_artifact(resource_id).await?;
    inner
        .probe_artifacts
        .lock()
        .expect("probe artifacts lock")
        .remove(resource_id);
    Ok(())
}

fn from_record(record: ProbeArtifactRecord) -> ProbeArtifact {
    ProbeArtifact {
        id: record.artifact_id,
        size: record.size as u64,
        sha256: record.sha256,
        flashed_at: crate::runtime::rfc3339_from_unix(record.flashed_at),
        change: record.change_id,
        created_by: record.created_by,
        defmt: record.defmt != 0,
        rtt_addr: record.rtt_addr,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Runtime, RuntimeConfig};
    use crate::store::Store;

    #[tokio::test]
    async fn probe_artifact_round_trip_uses_the_artifact_as_a_reference() {
        let directory = std::env::temp_dir().join(format!(
            "boot-api-probe-artifact-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        let store = std::sync::Arc::new(Store::open(directory.join("boot.sqlite3")).await.unwrap());
        let runtime = Runtime::new(
            store.clone(),
            RuntimeConfig::new(directory.join("sessions"), directory.join("artifacts")),
        );
        let artifact = create(
            &runtime.inner,
            include_bytes!("../../tests/fixtures/rtt.elf"),
        )
        .await
        .unwrap();
        let probe = set_probe_artifact(
            &runtime.inner,
            "probe_test",
            &artifact.id,
            Some("chg_test"),
            Some("principal"),
        )
        .await
        .unwrap();
        assert!(probe.rtt_addr.is_some());
        assert!(probe_artifact(&runtime.inner, "probe_test").is_some());
        assert!(store.artifact_in_use(&artifact.id).await.unwrap());
        clear_probe_artifact(&runtime.inner, "probe_test")
            .await
            .unwrap();
        assert!(!store.artifact_in_use(&artifact.id).await.unwrap());
        let _ = std::fs::remove_dir_all(&directory);
    }
}
