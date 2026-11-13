
    CREATE TABLE pending_names (
        node_id TEXT PRIMARY KEY,
        sent TEXT NOT NULL,
        name TEXT NOT NULL,
        landed INTEGER NOT NULL CHECK (landed IN (0, 1))
    ) STRICT, WITHOUT ROWID;
