//! SQLite storage for the API database.
//!
//! The database is opened read-write by both the server and the CLI. It contains
//! SHA-256 token hashes (never plaintext credentials) plus runtime metadata for
//! sessions, changes, and artifacts.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::Connection;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

use crate::auth::{
    CreateTokenRequest, CreatedToken, Scopes, StoreError, TokenRecord, TokenStore, generate_token,
    generate_token_id, validate_token_name,
};

/// `BOOT` as a SQLite application id.
pub const API_DB_APPLICATION_ID: i64 = 0x424F_4F54;
/// Default file name inside the boot data directory.
pub const API_DB_FILE_NAME: &str = "boot.sqlite3";

/// Resolve the API DB path from an explicit flag, `BOOT_API_DB`, or the
/// platform data directory.
pub fn resolve_api_db_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("BOOT_API_DB") {
        return Ok(PathBuf::from(path));
    }
    default_data_dir().map(|directory| directory.join(API_DB_FILE_NAME))
}

pub fn default_data_dir() -> Result<PathBuf> {
    let directories = directories::ProjectDirs::from("", "", "boot")
        .context("Failed to determine the boot data directory")?;
    Ok(directories.data_dir().to_path_buf())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn prepare_db_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
                    .with_context(|| format!("Failed to secure {}", parent.display()))?;
            }
        }
    }

    if !path.exists() {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(path)
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata =
            fs::metadata(path).with_context(|| format!("Failed to inspect {}", path.display()))?;
        let mode = metadata.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is accessible by group or other users (mode {:03o}); run: chmod 600 {}",
                path.display(),
                mode,
                path.display()
            );
        }
    }

    Ok(())
}

fn writer_options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Delete)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5))
        .pragma("secure_delete", "ON")
}

async fn application_id_pool(pool: &SqlitePool) -> Result<i64> {
    Ok(sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(pool)
        .await?)
}

async fn ensure_application_id_pool(pool: &SqlitePool) -> Result<()> {
    let current = application_id_pool(pool).await?;
    if current == API_DB_APPLICATION_ID {
        return Ok(());
    }
    if current == 0 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA application_id = {}",
            API_DB_APPLICATION_ID
        )))
        .execute(pool)
        .await?;
        return Ok(());
    }
    bail!(
        "not a boot API database (application_id {:#x}); refusing to use it",
        current
    )
}

pub mod runtime;

pub use runtime::*;

/// Handle to the shared API database.
pub struct Store {
    pool: SqlitePool,
}

impl Store {
    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Open (creating if necessary) and migrate the API database.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        prepare_db_file(&path)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(writer_options(&path))
            .await
            .with_context(|| format!("Failed to open {}", path.display()))?;
        ensure_application_id_pool(&pool).await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .with_context(|| format!("Failed to migrate {}", path.display()))?;
        Ok(Self { pool })
    }

    pub async fn has_tokens(&self) -> Result<bool, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
            .fetch_one(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(count > 0)
    }

    pub async fn has_admin_token(&self) -> Result<bool, StoreError> {
        let rows = sqlx::query!("SELECT scopes, expires_at FROM tokens")
            .fetch_all(&self.pool)
            .await
            .map_err(db_error)?;
        let now = now_unix();
        for row in rows {
            let (scopes, _) = Scopes::from_db(&row.scopes);
            if !row.expires_at.is_some_and(|expires_at| expires_at <= now)
                && scopes.has(crate::auth::Capability::Admin)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub async fn token_count(&self) -> Result<usize, StoreError> {
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tokens")
            .fetch_one(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(count as usize)
    }

    pub async fn create_token(
        &self,
        request: CreateTokenRequest,
    ) -> Result<CreatedToken, StoreError> {
        validate_token_name(&request.name).map_err(StoreError::Invalid)?;
        let name = request.name;
        let scopes = request.scopes;
        let mut connection = self.pool.acquire().await.map_err(db_error)?;
        let mut transaction = connection
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(db_error)?;

        if request.replace {
            sqlx::query!("DELETE FROM tokens WHERE name = ?", name)
                .execute(&mut *transaction)
                .await
                .map_err(db_error)?;
        } else {
            let count: i64 =
                sqlx::query_scalar!("SELECT COUNT(*) FROM tokens WHERE name = ?", name)
                    .fetch_one(&mut *transaction)
                    .await
                    .map_err(db_error)?;
            if count > 0 {
                return Err(StoreError::Conflict);
            }
        }

        let token = generate_token();
        let token_hash = crate::auth::hash_token(&token);
        let id = generate_token_id();
        let created_at = now_unix();
        let scopes_text = scopes.canonical();

        if let Err(error) = sqlx::query!(
            "INSERT INTO tokens (id, name, token_hash, scopes, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            id,
            name,
            token_hash.as_slice(),
            scopes_text,
            created_at,
            request.expires_at,
        )
        .execute(&mut *transaction)
        .await
        {
            if error
                .as_database_error()
                .is_some_and(|error| error.is_unique_violation())
            {
                return Err(StoreError::Conflict);
            }
            return Err(db_error(error));
        }
        transaction.commit().await.map_err(db_error)?;

        Ok(CreatedToken {
            record: TokenRecord {
                id,
                name,
                token_hash,
                scopes,
                created_at,
                expires_at: request.expires_at,
            },
            token,
        })
    }
}

fn db_error(error: sqlx::Error) -> StoreError {
    StoreError::Database(format!("API database error: {error}"))
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: String,
    name: String,
    token_hash: Vec<u8>,
    scopes: String,
    created_at: i64,
    expires_at: Option<i64>,
}

impl TokenRow {
    fn into_record(self) -> Result<TokenRecord, StoreError> {
        if self.token_hash.len() != 32 {
            return Err(StoreError::Database(format!(
                "token {} has an invalid hash length",
                self.id
            )));
        }
        let mut token_hash = [0u8; 32];
        token_hash.copy_from_slice(&self.token_hash);
        let (scopes, _unknown) = Scopes::from_db(&self.scopes);
        Ok(TokenRecord {
            id: self.id,
            name: self.name,
            token_hash,
            scopes,
            created_at: self.created_at,
            expires_at: self.expires_at,
        })
    }
}

#[async_trait::async_trait]
impl TokenStore for Store {
    async fn find_token_by_hash(&self, hash: &[u8; 32]) -> Result<Option<TokenRecord>, StoreError> {
        let row = sqlx::query_as!(
            TokenRow,
            "SELECT id AS \"id!\", name AS \"name!\", token_hash AS \"token_hash!\",
                    scopes AS \"scopes!\", created_at AS \"created_at!\", expires_at
             FROM tokens WHERE token_hash = ?",
            hash.as_slice()
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(TokenRow::into_record).transpose()
    }

    async fn find_token_by_id(&self, id: &str) -> Result<Option<TokenRecord>, StoreError> {
        let row = sqlx::query_as!(
            TokenRow,
            "SELECT id AS \"id!\", name AS \"name!\", token_hash AS \"token_hash!\",
                    scopes AS \"scopes!\", created_at AS \"created_at!\", expires_at
             FROM tokens WHERE id = ?",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(db_error)?;
        row.map(TokenRow::into_record).transpose()
    }

    async fn create_token(&self, request: CreateTokenRequest) -> Result<CreatedToken, StoreError> {
        Store::create_token(self, request).await
    }

    async fn list_tokens(&self) -> Result<Vec<TokenRecord>, StoreError> {
        let rows = sqlx::query_as!(
            TokenRow,
            "SELECT id AS \"id!\", name AS \"name!\", token_hash AS \"token_hash!\",
                    scopes AS \"scopes!\", created_at AS \"created_at!\", expires_at
             FROM tokens ORDER BY created_at, id"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_error)?;
        rows.into_iter().map(TokenRow::into_record).collect()
    }

    async fn revoke_token(&self, selector: &str) -> Result<usize, StoreError> {
        let result = sqlx::query!(
            "DELETE FROM tokens WHERE id = ? OR name = ?",
            selector,
            selector
        )
        .execute(&self.pool)
        .await
        .map_err(db_error)?;
        if result.rows_affected() == 0 {
            Err(StoreError::NotFound)
        } else {
            Ok(result.rows_affected() as usize)
        }
    }

    async fn revoke_all(&self) -> Result<usize, StoreError> {
        let result = sqlx::query!("DELETE FROM tokens")
            .execute(&self.pool)
            .await
            .map_err(db_error)?;
        Ok(result.rows_affected() as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Capability, generate_token, hash_token};

    fn temp_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("boot-api-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory.join("boot.sqlite3")
    }

    #[tokio::test]
    async fn token_round_trip_stores_only_a_hash() {
        let path = temp_path("round-trip");
        let db = Store::open(&path).await.unwrap();
        let created = db
            .create_token(CreateTokenRequest {
                name: "laptop".to_string(),
                scopes: Scopes::control(),
                expires_at: None,
                replace: false,
            })
            .await
            .unwrap();
        assert!(created.token.starts_with(crate::auth::TOKEN_PREFIX));

        let raw: Vec<u8> =
            sqlx::query_scalar("SELECT token_hash FROM tokens WHERE name = 'laptop'")
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(raw.len(), 32);
        assert_eq!(raw, hash_token(&created.token).to_vec());
        assert_ne!(raw, created.token.as_bytes());

        let listed = db.list_tokens().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "laptop");
        assert!(listed[0].scopes.has(Capability::Control));

        assert!(!db.has_admin_token().await.unwrap());
        assert_eq!(db.revoke_token("laptop").await.unwrap(), 1);
        assert!(matches!(
            db.revoke_token("laptop").await,
            Err(StoreError::NotFound)
        ));
    }

    #[tokio::test]
    async fn duplicate_names_and_replace_are_atomic() {
        let path = temp_path("replace");
        let db = Store::open(&path).await.unwrap();
        let request = |replace| CreateTokenRequest {
            name: "ci".to_string(),
            scopes: Scopes::read(),
            expires_at: None,
            replace,
        };
        db.create_token(request(false)).await.unwrap();
        assert!(matches!(
            db.create_token(request(false)).await,
            Err(StoreError::Conflict)
        ));
        let replacement = db.create_token(request(true)).await.unwrap();
        assert_eq!(db.token_count().await.unwrap(), 1);
        assert!(
            db.find_token_by_hash(&hash_token(&replacement.token))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn schema_is_idempotent_and_newer_versions_fail() {
        let path = temp_path("schema");
        {
            let db = Store::open(&path).await.unwrap();
            let _ = db
                .create_token(CreateTokenRequest {
                    name: "local".to_string(),
                    scopes: Scopes::admin(),
                    expires_at: None,
                    replace: false,
                })
                .await;
        }
        let db = Store::open(&path).await.unwrap();
        assert_eq!(db.token_count().await.unwrap(), 1);
        sqlx::query(
            "INSERT INTO _sqlx_migrations
                (version, description, success, checksum, execution_time)
             VALUES (99, 'newer', 1, X'00', 0)",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        assert!(Store::open(&path).await.is_err());
    }

    #[tokio::test]
    async fn names_are_bound_parameters() {
        let path = temp_path("sql");
        let db = Store::open(&path).await.unwrap();
        let name = "x'); DROP TABLE tokens; --";
        db.create_token(CreateTokenRequest {
            name: name.to_string(),
            scopes: Scopes::read(),
            expires_at: None,
            replace: false,
        })
        .await
        .unwrap();
        assert!(
            db.list_tokens()
                .await
                .unwrap()
                .iter()
                .any(|record| record.name == name)
        );
        assert_eq!(db.revoke_token(name).await.unwrap(), 1);
    }

    #[test]
    fn generated_tokens_are_unique() {
        assert_ne!(generate_token(), generate_token());
    }

    #[tokio::test]
    async fn artifacts_track_references_and_probe_records() {
        let path = temp_path("artifact-refs");
        let db = Store::open(&path).await.unwrap();
        db.insert_artifact(&ArtifactRecord {
            id: "art_test".into(),
            path: "/tmp/art_test.bin".into(),
            size: 3,
            sha256: "abc".into(),
            created_at: 1,
        })
        .await
        .unwrap();
        assert!(!db.artifact_in_use("art_test").await.unwrap());

        db.add_artifact_ref("art_test", "session", "ses_test", 2)
            .await
            .unwrap();
        assert!(db.artifact_in_use("art_test").await.unwrap());
        db.remove_artifact_ref("art_test", "session", "ses_test")
            .await
            .unwrap();
        assert!(!db.artifact_in_use("art_test").await.unwrap());

        db.upsert_probe_artifact(&ProbeArtifactRecord {
            resource_id: "probe_test".into(),
            artifact_id: "art_test".into(),
            sha256: "abc".into(),
            size: 3,
            defmt: 1,
            rtt_addr: Some("0x2000".into()),
            change_id: Some("chg_test".into()),
            created_by: Some("principal".into()),
            flashed_at: 3,
        })
        .await
        .unwrap();
        assert!(db.artifact_in_use("art_test").await.unwrap());
        assert_eq!(
            db.load_probe_artifact("probe_test")
                .await
                .unwrap()
                .unwrap()
                .artifact_id,
            "art_test"
        );
        assert_eq!(db.load_probe_artifacts().await.unwrap().len(), 1);

        db.delete_probe_artifact("probe_test").await.unwrap();
        assert!(!db.artifact_in_use("art_test").await.unwrap());
        assert!(
            db.load_probe_artifact("probe_test")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_group_or_world_readable_database() {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("permissions");
        Store::open(&path).await.unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = match Store::open(&path).await {
            Ok(_) => panic!("group-readable database was accepted"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("chmod 600"), "unexpected error: {error}");
    }

    #[tokio::test]
    async fn wrong_application_id_is_rejected() {
        use sqlx::{Connection, SqliteConnection};

        let path = temp_path("app-id");
        drop(Store::open(&path).await.unwrap());
        let mut connection = SqliteConnection::connect_with(&writer_options(&path))
            .await
            .unwrap();
        sqlx::query("PRAGMA application_id = 305419896")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);
        assert!(Store::open(&path).await.is_err());
    }
}
