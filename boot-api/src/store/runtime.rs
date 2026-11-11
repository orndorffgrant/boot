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
    pub expires_at: i64,
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
    async fn expired_artifacts(&self, now: i64) -> Result<Vec<ArtifactRecord>, StoreError>;
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
              state, first_seq, latest_seq, data_bytes, replayable, resume_count, last_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
                resume_count = ?, last_error = ?
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
            "INSERT INTO artifacts (id, path, size, sha256, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(&record.path)
        .bind(record.size)
        .bind(&record.sha256)
        .bind(record.created_at)
        .bind(record.expires_at)
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

    async fn expired_artifacts(&self, now: i64) -> Result<Vec<ArtifactRecord>, StoreError> {
        sqlx::query_as::<_, ArtifactRecord>("SELECT * FROM artifacts WHERE expires_at <= ?")
            .bind(now)
            .fetch_all(self.pool())
            .await
            .map_err(db_error)
    }
}
