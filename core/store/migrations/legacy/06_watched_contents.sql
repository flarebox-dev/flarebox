
    CREATE TABLE watched_contents (
        node_id TEXT PRIMARY KEY,
        changed INTEGER NOT NULL CHECK (changed IN (0, 1))
    ) STRICT, WITHOUT ROWID;
