CREATE TABLE move_halves (
    node_id TEXT PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    content BLOB
) STRICT, WITHOUT ROWID;
