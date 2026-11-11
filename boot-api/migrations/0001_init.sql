-- Shared API database: token hashes plus runtime metadata.
-- Plaintext tokens live only in the CLI database.
CREATE TABLE IF NOT EXISTS tokens (
    id         TEXT    PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    token_hash BLOB    NOT NULL UNIQUE,
    scopes     TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER,
    CHECK (length(id) = 16),
    CHECK (length(token_hash) = 32),
    CHECK (length(scopes) > 0),
    CHECK (name <> '')
);
-- Runtime state in the shared boot.sqlite3.

CREATE TABLE IF NOT EXISTS sessions (
    id                       TEXT PRIMARY KEY,
    kind                     TEXT NOT NULL,
    resource_id              TEXT NOT NULL,
    generation               INTEGER NOT NULL,
    options_json             TEXT NOT NULL,
    max_subscribers          INTEGER NOT NULL,
    detach_timeout_seconds   INTEGER NOT NULL,
    created_by               TEXT NOT NULL,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    closed_at                INTEGER,
    state                    TEXT NOT NULL,
    first_seq                INTEGER NOT NULL DEFAULT 0,
    latest_seq               INTEGER NOT NULL DEFAULT 0,
    data_bytes               INTEGER NOT NULL DEFAULT 0,
    replayable               INTEGER NOT NULL DEFAULT 1,
    resume_count             INTEGER NOT NULL DEFAULT 0,
    last_error               TEXT,
    CHECK (kind IN ('serial','rtt','dap')),
    CHECK (state IN ('creating','starting','active','suspended','resuming','closing','closed','failed'))
);

CREATE INDEX IF NOT EXISTS sessions_by_state ON sessions(state, updated_at);

CREATE TABLE IF NOT EXISTS session_segments (
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    segment_no  INTEGER NOT NULL,
    first_seq   INTEGER NOT NULL,
    last_seq    INTEGER NOT NULL,
    bytes       INTEGER NOT NULL,
    path        TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    PRIMARY KEY (session_id, segment_no)
);

CREATE TABLE IF NOT EXISTS session_marks (
    mark_id     TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    detail_json TEXT
);

CREATE INDEX IF NOT EXISTS session_marks_by_session ON session_marks(session_id, seq);

CREATE TABLE IF NOT EXISTS changes (
    id            TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    resource_id   TEXT NOT NULL,
    status        TEXT NOT NULL,
    created_by    TEXT NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    ready_at      INTEGER,
    snapshot_json TEXT NOT NULL,
    log_json      TEXT NOT NULL,
    CHECK (status IN ('queued','waiting','doing','done','failed','cancelling','cancelled'))
);

CREATE INDEX IF NOT EXISTS changes_by_status ON changes(status, updated_at);

CREATE TABLE IF NOT EXISTS artifacts (
    id          TEXT PRIMARY KEY,
    path        TEXT NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS artifacts_by_expiry ON artifacts(expires_at);
