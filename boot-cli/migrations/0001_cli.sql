-- SPDX-License-Identifier: AGPL-3.0-or-later
-- CLI database: known servers and their optional plaintext client tokens.
CREATE TABLE IF NOT EXISTS servers (
    name       TEXT PRIMARY KEY,
    url        TEXT NOT NULL,
    ca_cert    TEXT,
    token      TEXT,                -- NULL = no credential stored
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (name <> ''),
    CHECK (url LIKE 'http://%' OR url LIKE 'https://%')
);
