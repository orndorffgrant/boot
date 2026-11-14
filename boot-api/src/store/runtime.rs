//! Runtime persistence over the shared API SQLite database.
//!
//! Metadata/state rows live in `boot.sqlite3`. Session payloads live in
//! append-only segment files handled by [`crate::runtime::session_log`].

use async_trait::async_trait;
use sqlx::FromRow;

use super::Store;
use crate::auth::StoreError;

#[derive(Debug, Clone, FromRow)]
pub struct SessionRecord {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub generation: i64,
    pub options_json: String,
    pub max_subscribers: i64,
    pub detach_timeout_seconds: i64,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    pub state: String,
    pub first_seq: i64,
    pub latest_seq: i64,
    pub data_bytes: i64,
    pub replayable: i64,
    pub resume_count: i64,
    pub artifact_id: Option<String>,
    pub decode_json: String,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct SegmentRecord {
    pub session_id: String,
    pub segment_no: i64,
    pub first_seq: i64,
    pub last_seq: i64,
    pub bytes: i64,
    pub path: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct ChangeRecord {
    pub id: String,
    pub kind: String,
    pub resource_id: String,
    pub status: String,
    pub created_by: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub ready_at: Option<i64>,
    pub snapshot_json: String,
    pub log_json: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct ArtifactRecord {
    pub id: String,
    pub path: String,
    pub size: i64,
    pub sha256: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow)]
pub struct ProbeArtifactRecord {
    pub resource_id: String,
    pub artifact_id: String,
    pub sha256: String,
    pub size: i64,
    pub defmt: i64,
    pub rtt_addr: Option<String>,
    pub change_id: Option<String>,
    pub created_by: Option<String>,
    pub flashed_at: i64,
}

#[async_trait]
pub trait RuntimeStore: Send + Sync {
    async fn insert_session(&self, record: &SessionRecord) -> Result<(), StoreError>;
    async fn update_session(&self, record: &SessionRecord) -> Result<(), StoreError>;
    async fn load_session(&self, id: &str) -> Result<Option<SessionRecord>, StoreError>;
    async fn load_sessions(&self) -> Result<Vec<SessionRecord>, StoreError>;
    async fn add_segment(&self, record: &SegmentRecord) -> Result<(), StoreError>;
    async fn load_segments(&self, session_id: &str) -> Result<Vec<SegmentRecord>, StoreError>;
    async fn purge_session_data(&self, session_id: &str) -> Result<(), StoreError>;
    async fn insert_change(&self, record: &ChangeRecord) -> Result<(), StoreError>;
    async fn update_change(&self, record: &ChangeRecord) -> Result<(), StoreError>;
    async fn load_change(&self, id: &str) -> Result<Option<ChangeRecord>, StoreError>;
    async fn load_changes(&self) -> Result<Vec<ChangeRecord>, StoreError>;
    async fn insert_artifact(&self, record: &ArtifactRecord) -> Result<(), StoreError>;
    async fn load_artifact(&self, id: &str) -> Result<Option<ArtifactRecord>, StoreError>;
    async fn delete_artifact(&self, id: &str) -> Result<(), StoreError>;
    async fn artifact_in_use(&self, id: &str) -> Result<bool, StoreError>;
    async fn add_artifact_ref(
        &self,
        artifact_id: &str,
        owner_kind: &str,
        owner_id: &str,
        created_at: i64,
    ) -> Result<(), StoreError>;
    async fn remove_artifact_ref(
        &self,
        artifact_id: &str,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), StoreError>;
    async fn remove_artifact_refs_for_owner(
        &self,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), StoreError>;
    async fn remove_artifact_refs_for_owner_kind(&self, owner_kind: &str)
    -> Result<(), StoreError>;
    async fn upsert_probe_artifact(&self, record: &ProbeArtifactRecord) -> Result<(), StoreError>;
    async fn load_probe_artifact(
        &self,
        resource_id: &str,
    ) -> Result<Option<ProbeArtifactRecord>, StoreError>;
    async fn load_probe_artifacts(&self) -> Result<Vec<ProbeArtifactRecord>, StoreError>;
    async fn delete_probe_artifact(&self, resource_id: &str) -> Result<(), StoreError>;
}

fn db_error(error: sqlx::Error) -> StoreError {
    StoreError::Database(format!("API database error: {error}"))
}

#[async_trait]
impl RuntimeStore for Store {
    async fn insert_session(&self, record: &SessionRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sessions
             (id, kind, resource_id, generation, options_json, max_subscribers,
              detach_timeout_seconds, created_by, created_at, updated_at, closed_at,
              state, first_seq, latest_seq, data_bytes, replayable, resume_count,
              artifact_id, decode_json, last_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.kind)
        .bind(&record.resource_id)
        .bind(record.generation)
        .bind(&record.options_json)
        .bind(record.max_subscribers)
        .bind(record.detach_timeout_seconds)
        .bind(&record.created_by)
        .bind(record.created_at)
        .bind(record.updated_at)
        .bind(record.closed_at)
        .bind(&record.state)
        .bind(record.first_seq)
        .bind(record.latest_seq)
        .bind(record.data_bytes)
        .bind(record.replayable)
        .bind(record.resume_count)
        .bind(&record.artifact_id)
        .bind(&record.decode_json)
        .bind(&record.last_error)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn update_session(&self, record: &SessionRecord) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE sessions SET
                generation = ?, options_json = ?, max_subscribers = ?,
                detach_timeout_seconds = ?, updated_at = ?, closed_at = ?, state = ?,
                first_seq = ?, latest_seq = ?, data_bytes = ?, replayable = ?,
                resume_count = ?, artifact_id = ?, decode_json = ?, last_error = ?
             WHERE id = ?",
        )
        .bind(record.generation)
        .bind(&record.options_json)
        .bind(record.max_subscribers)
        .bind(record.detach_timeout_seconds)
        .bind(record.updated_at)
        .bind(record.closed_at)
        .bind(&record.state)
        .bind(record.first_seq)
        .bind(record.latest_seq)
        .bind(record.data_bytes)
        .bind(record.replayable)
        .bind(record.resume_count)
        .bind(&record.artifact_id)
        .bind(&record.decode_json)
        .bind(&record.last_error)
        .bind(&record.id)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn load_session(&self, id: &str) -> Result<Option<SessionRecord>, StoreError> {
        sqlx::query_as::<_, SessionRecord>("SELECT * FROM sessions WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(db_error)
    }

    async fn load_sessions(&self) -> Result<Vec<SessionRecord>, StoreError> {
        sqlx::query_as::<_, SessionRecord>("SELECT * FROM sessions ORDER BY created_at, id")
            .fetch_all(self.pool())
            .await
            .map_err(db_error)
    }

    async fn add_segment(&self, record: &SegmentRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO session_segments
             (session_id, segment_no, first_seq, last_seq, bytes, path, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.session_id)
        .bind(record.segment_no)
        .bind(record.first_seq)
        .bind(record.last_seq)
        .bind(record.bytes)
        .bind(&record.path)
        .bind(record.created_at)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn load_segments(&self, session_id: &str) -> Result<Vec<SegmentRecord>, StoreError> {
        sqlx::query_as::<_, SegmentRecord>(
            "SELECT * FROM session_segments WHERE session_id = ? ORDER BY segment_no",
        )
        .bind(session_id)
        .fetch_all(self.pool())
        .await
        .map_err(db_error)
    }

    async fn purge_session_data(&self, session_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM session_segments WHERE session_id = ?")
            .bind(session_id)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        sqlx::query("UPDATE sessions SET replayable = 0, first_seq = latest_seq, data_bytes = 0 WHERE id = ?")
            .bind(session_id)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn insert_change(&self, record: &ChangeRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO changes
             (id, kind, resource_id, status, created_by, created_at, updated_at,
              ready_at, snapshot_json, log_json)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.kind)
        .bind(&record.resource_id)
        .bind(&record.status)
        .bind(&record.created_by)
        .bind(record.created_at)
        .bind(record.updated_at)
        .bind(record.ready_at)
        .bind(&record.snapshot_json)
        .bind(&record.log_json)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn update_change(&self, record: &ChangeRecord) -> Result<(), StoreError> {
        sqlx::query(
            "UPDATE changes SET status = ?, updated_at = ?, ready_at = ?,
                snapshot_json = ?, log_json = ? WHERE id = ?",
        )
        .bind(&record.status)
        .bind(record.updated_at)
        .bind(record.ready_at)
        .bind(&record.snapshot_json)
        .bind(&record.log_json)
        .bind(&record.id)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn load_change(&self, id: &str) -> Result<Option<ChangeRecord>, StoreError> {
        sqlx::query_as::<_, ChangeRecord>("SELECT * FROM changes WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(db_error)
    }

    async fn load_changes(&self) -> Result<Vec<ChangeRecord>, StoreError> {
        sqlx::query_as::<_, ChangeRecord>("SELECT * FROM changes ORDER BY created_at DESC")
            .fetch_all(self.pool())
            .await
            .map_err(db_error)
    }

    async fn insert_artifact(&self, record: &ArtifactRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO artifacts (id, path, size, sha256, created_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.path)
        .bind(record.size)
        .bind(&record.sha256)
        .bind(record.created_at)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn load_artifact(&self, id: &str) -> Result<Option<ArtifactRecord>, StoreError> {
        sqlx::query_as::<_, ArtifactRecord>("SELECT * FROM artifacts WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await
            .map_err(db_error)
    }

    async fn delete_artifact(&self, id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM artifacts WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn artifact_in_use(&self, id: &str) -> Result<bool, StoreError> {
        let referenced: i64 = sqlx::query_scalar(
            "SELECT
                EXISTS(SELECT 1 FROM artifact_refs WHERE artifact_id = ?)
                OR EXISTS(SELECT 1 FROM probe_artifacts WHERE artifact_id = ?)",
        )
        .bind(id)
        .bind(id)
        .fetch_one(self.pool())
        .await
        .map_err(db_error)?;
        Ok(referenced != 0)
    }

    async fn add_artifact_ref(
        &self,
        artifact_id: &str,
        owner_kind: &str,
        owner_id: &str,
        created_at: i64,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT OR IGNORE INTO artifact_refs
             (artifact_id, owner_kind, owner_id, created_at)
             VALUES (?, ?, ?, ?)",
        )
        .bind(artifact_id)
        .bind(owner_kind)
        .bind(owner_id)
        .bind(created_at)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn remove_artifact_ref(
        &self,
        artifact_id: &str,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "DELETE FROM artifact_refs
             WHERE artifact_id = ? AND owner_kind = ? AND owner_id = ?",
        )
        .bind(artifact_id)
        .bind(owner_kind)
        .bind(owner_id)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn remove_artifact_refs_for_owner(
        &self,
        owner_kind: &str,
        owner_id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM artifact_refs WHERE owner_kind = ? AND owner_id = ?")
            .bind(owner_kind)
            .bind(owner_id)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn remove_artifact_refs_for_owner_kind(
        &self,
        owner_kind: &str,
    ) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM artifact_refs WHERE owner_kind = ?")
            .bind(owner_kind)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        Ok(())
    }

    async fn upsert_probe_artifact(&self, record: &ProbeArtifactRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO probe_artifacts
             (resource_id, artifact_id, sha256, size, defmt, rtt_addr,
              change_id, created_by, flashed_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(resource_id) DO UPDATE SET
                artifact_id = excluded.artifact_id,
                sha256 = excluded.sha256,
                size = excluded.size,
                defmt = excluded.defmt,
                rtt_addr = excluded.rtt_addr,
                change_id = excluded.change_id,
                created_by = excluded.created_by,
                flashed_at = excluded.flashed_at",
        )
        .bind(&record.resource_id)
        .bind(&record.artifact_id)
        .bind(&record.sha256)
        .bind(record.size)
        .bind(record.defmt)
        .bind(&record.rtt_addr)
        .bind(&record.change_id)
        .bind(&record.created_by)
        .bind(record.flashed_at)
        .execute(self.pool())
        .await
        .map_err(db_error)?;
        Ok(())
    }

    async fn load_probe_artifact(
        &self,
        resource_id: &str,
    ) -> Result<Option<ProbeArtifactRecord>, StoreError> {
        sqlx::query_as::<_, ProbeArtifactRecord>(
            "SELECT * FROM probe_artifacts WHERE resource_id = ?",
        )
        .bind(resource_id)
        .fetch_optional(self.pool())
        .await
        .map_err(db_error)
    }

    async fn load_probe_artifacts(&self) -> Result<Vec<ProbeArtifactRecord>, StoreError> {
        sqlx::query_as::<_, ProbeArtifactRecord>("SELECT * FROM probe_artifacts")
            .fetch_all(self.pool())
            .await
            .map_err(db_error)
    }

    async fn delete_probe_artifact(&self, resource_id: &str) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM probe_artifacts WHERE resource_id = ?")
            .bind(resource_id)
            .execute(self.pool())
            .await
            .map_err(db_error)?;
        Ok(())
    }
}
