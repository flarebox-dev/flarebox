
    CREATE TABLE replaced_versions (
        node_id TEXT NOT NULL,
        position INTEGER NOT NULL CHECK (position >= 0),
        version_id TEXT NOT NULL,
        PRIMARY KEY (node_id, position)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE resends (
        node_id TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;
