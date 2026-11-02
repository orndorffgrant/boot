-- API database: token hashes only. Plaintext tokens live in the CLI database.
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
