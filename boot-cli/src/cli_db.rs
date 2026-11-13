//! SQLite storage for CLI state: known servers and their plaintext tokens.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sqlx::Connection;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions, SqliteSynchronous,
};

/// `BCLI` as a SQLite application id.
pub const CLI_DB_APPLICATION_ID: i64 = 0x4243_4C49;
pub const CLI_DB_FILE_NAME: &str = "boot-cli.sqlite3";

#[derive(Debug, Clone)]
pub struct ServerEntry {
    pub name: String,
    pub url: String,
    pub ca_cert: Option<String>,
    pub token: Option<String>,
}

impl ServerEntry {
    pub fn tls(&self) -> bool {
        self.url.starts_with("https://")
    }
}

pub fn resolve_cli_db_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }
    if let Some(path) = std::env::var_os("BOOT_CLI_DB") {
        return Ok(PathBuf::from(path));
    }
    let directories = directories::ProjectDirs::from("", "", "boot")
        .context("Failed to determine the boot data directory")?;
    Ok(directories.data_dir().join(CLI_DB_FILE_NAME))
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
                fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
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
        let mode = fs::metadata(path)?.permissions().mode() & 0o777;
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

fn options(path: &Path) -> SqliteConnectOptions {
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Delete)
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(Duration::from_secs(5))
        .pragma("secure_delete", "ON")
}

async fn ensure_application_id(pool: &SqlitePool) -> Result<()> {
    let current: i64 = sqlx::query_scalar("PRAGMA application_id")
        .fetch_one(pool)
        .await?;
    if current == CLI_DB_APPLICATION_ID {
        return Ok(());
    }
    if current == 0 {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "PRAGMA application_id = {}",
            CLI_DB_APPLICATION_ID
        )))
        .execute(pool)
        .await?;
        return Ok(());
    }
    bail!(
        "not a boot CLI database (application_id {:#x}); refusing to use it",
        current
    )
}

pub struct CliDb {
    pool: SqlitePool,
}

#[derive(sqlx::FromRow)]
struct ServerRow {
    name: String,
    url: String,
    ca_cert: Option<String>,
    token: Option<String>,
}

impl From<ServerRow> for ServerEntry {
    fn from(row: ServerRow) -> Self {
        Self {
            name: row.name,
            url: row.url,
            ca_cert: row.ca_cert,
            token: row.token,
        }
    }
}

impl CliDb {
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        prepare_db_file(path)?;
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options(path))
            .await
            .with_context(|| format!("Failed to open {}", path.display()))?;
        ensure_application_id(&pool).await?;
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .with_context(|| format!("Failed to migrate {}", path.display()))?;
        Ok(Self { pool })
    }

    pub async fn open_default(explicit: Option<&Path>) -> Result<Self> {
        Self::open(resolve_cli_db_path(explicit)?).await
    }

    pub async fn list_servers(&self) -> Result<Vec<ServerEntry>> {
        let rows = sqlx::query_as!(
            ServerRow,
            "SELECT name AS \"name!\", url AS \"url!\", ca_cert, token
             FROM servers
             ORDER BY name"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(ServerEntry::from).collect())
    }

    pub async fn get_server(&self, name: &str) -> Result<Option<ServerEntry>> {
        let row = sqlx::query_as!(
            ServerRow,
            "SELECT name AS \"name!\", url AS \"url!\", ca_cert, token
             FROM servers
             WHERE name = ?",
            name
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(ServerEntry::from))
    }

    pub async fn add_server(
        &self,
        name: &str,
        url: &str,
        ca_cert: Option<&str>,
        token: Option<&str>,
    ) -> Result<()> {
        validate_url(url)?;
        let now = now_unix();
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        let exists: i64 = sqlx::query_scalar!("SELECT COUNT(*) FROM servers WHERE name = ?", name)
            .fetch_one(&mut *transaction)
            .await?;
        if exists > 0 {
            bail!("Server with name '{}' already exists", name);
        }
        sqlx::query!(
            "INSERT INTO servers (name, url, ca_cert, token, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            name,
            url,
            ca_cert,
            token,
            now,
            now,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Insert or update a server, preserving an existing credential unless a
    /// replacement token is supplied.
    pub async fn upsert_server(
        &self,
        name: &str,
        url: &str,
        ca_cert: Option<&str>,
        token: Option<&str>,
    ) -> Result<()> {
        validate_url(url)?;
        let now = now_unix();
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query!(
            "INSERT INTO servers (name, url, ca_cert, token, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
                 url = excluded.url,
                 ca_cert = COALESCE(excluded.ca_cert, servers.ca_cert),
                 token = COALESCE(excluded.token, servers.token),
                 updated_at = excluded.updated_at",
            name,
            url,
            ca_cert,
            token,
            now,
            now,
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Update an existing server entry. `None` keeps a field unchanged,
    /// `Some(None)` clears it, and `Some(Some(value))` sets it.
    pub async fn update_server(
        &self,
        name: &str,
        url: Option<&str>,
        ca_cert: Option<Option<&str>>,
        token: Option<Option<&str>>,
    ) -> Result<()> {
        if let Some(url) = url {
            validate_url(url)?;
        }
        let now = now_unix();
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        let existing = sqlx::query_as!(
            ServerRow,
            "SELECT name AS \"name!\", url AS \"url!\", ca_cert, token
             FROM servers
             WHERE name = ?",
            name
        )
        .fetch_optional(&mut *transaction)
        .await?;
        let Some(ServerRow {
            url: old_url,
            ca_cert: old_ca_cert,
            token: old_token,
            ..
        }) = existing
        else {
            bail!("Server '{}' not found", name);
        };
        let new_url = url.unwrap_or(&old_url);
        let new_ca_cert = ca_cert
            .map(|value| value.map(str::to_string))
            .unwrap_or(old_ca_cert);
        let new_token = token
            .map(|value| value.map(str::to_string))
            .unwrap_or(old_token);
        sqlx::query(
            "UPDATE servers SET url = ?, ca_cert = ?, token = ?, updated_at = ? WHERE name = ?",
        )
        .bind(new_url)
        .bind(new_ca_cert)
        .bind(new_token)
        .bind(now)
        .bind(name)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn set_token(&self, name: &str, token: &str) -> Result<()> {
        let now = now_unix();
        let result = sqlx::query!(
            "UPDATE servers SET token = ?, updated_at = ? WHERE name = ?",
            token,
            now,
            name,
        )
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            bail!("Server '{}' not found", name);
        }
        Ok(())
    }

    pub async fn remove_server(&self, name: &str) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut transaction = connection.begin_with("BEGIN IMMEDIATE").await?;
        let result = sqlx::query!("DELETE FROM servers WHERE name = ?", name)
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        if result.rows_affected() == 0 {
            bail!("Server '{}' not found", name);
        }
        Ok(())
    }
}

pub(crate) fn validate_url(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("Server URL must start with http:// or https://");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("boot-cli-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory.join("boot-cli.sqlite3")
    }

    #[tokio::test]
    async fn server_and_credential_round_trip() {
        let db = CliDb::open(temp_path("round-trip")).await.unwrap();
        db.add_server("pi", "http://127.0.0.1:46673", None, Some("boot_v1_secret"))
            .await
            .unwrap();
        let server = db.get_server("pi").await.unwrap().unwrap();
        assert_eq!(server.url, "http://127.0.0.1:46673");
        assert_eq!(server.token.as_deref(), Some("boot_v1_secret"));
        db.set_token("pi", "boot_v1_new").await.unwrap();
        assert_eq!(
            db.get_server("pi").await.unwrap().unwrap().token.as_deref(),
            Some("boot_v1_new")
        );
        db.remove_server("pi").await.unwrap();
        assert!(db.get_server("pi").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_preserves_existing_token() {
        let db = CliDb::open(temp_path("upsert")).await.unwrap();
        db.upsert_server("local", "http://127.0.0.1:46673", None, Some("token"))
            .await
            .unwrap();
        db.upsert_server("local", "http://127.0.0.1:46674", None, None)
            .await
            .unwrap();
        let server = db.get_server("local").await.unwrap().unwrap();
        assert_eq!(server.url, "http://127.0.0.1:46674");
        assert_eq!(server.token.as_deref(), Some("token"));
    }

    #[tokio::test]
    async fn update_server_keeps_clears_and_sets_fields() {
        let db = CliDb::open(temp_path("update")).await.unwrap();
        db.add_server(
            "pi",
            "http://127.0.0.1:46673",
            Some("/tmp/ca.pem"),
            Some("boot_v1_old"),
        )
        .await
        .unwrap();

        db.update_server("pi", Some("https://pi.local:46673"), None, None)
            .await
            .unwrap();
        let server = db.get_server("pi").await.unwrap().unwrap();
        assert_eq!(server.url, "https://pi.local:46673");
        assert_eq!(server.ca_cert.as_deref(), Some("/tmp/ca.pem"));
        assert_eq!(server.token.as_deref(), Some("boot_v1_old"));

        db.update_server("pi", None, Some(None), Some(Some("boot_v1_new")))
            .await
            .unwrap();
        let server = db.get_server("pi").await.unwrap().unwrap();
        assert_eq!(server.ca_cert, None);
        assert_eq!(server.token.as_deref(), Some("boot_v1_new"));

        db.update_server("pi", None, None, Some(None))
            .await
            .unwrap();
        let server = db.get_server("pi").await.unwrap().unwrap();
        assert_eq!(server.token, None);
        assert!(db.update_server("missing", None, None, None).await.is_err());
    }

    #[tokio::test]
    async fn rejects_duplicate_servers_and_bad_urls() {
        let db = CliDb::open(temp_path("duplicate")).await.unwrap();
        db.add_server("pi", "http://127.0.0.1", None, None)
            .await
            .unwrap();
        assert!(
            db.add_server("pi", "http://127.0.0.1", None, None)
                .await
                .is_err()
        );
        assert!(
            db.add_server("x", "ftp://127.0.0.1", None, None)
                .await
                .is_err()
        );
    }
}
