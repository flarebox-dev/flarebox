
    CREATE TABLE remote_cursor (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        history_id TEXT NOT NULL,
        last_seq INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE remote_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        status INTEGER NOT NULL,
        content_rev INTEGER NOT NULL,
        meta_rev INTEGER NOT NULL,
        trash_group TEXT,
        current BLOB
    ) STRICT;

    CREATE TABLE synced_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB
    ) STRICT;

    CREATE TABLE local_nodes (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB,
        dev INTEGER NOT NULL,
        ino INTEGER NOT NULL,
        birth_ns INTEGER,
        parked INTEGER NOT NULL CHECK (parked IN (0, 1))
    ) STRICT;

    CREATE TABLE commit_in_flight (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        commit_message BLOB NOT NULL
    ) STRICT;

    CREATE TABLE commit_in_flight_shapes (
        op_index INTEGER PRIMARY KEY,
        parent_id TEXT,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB
    ) STRICT;

    CREATE TABLE problems (
        node_id TEXT PRIMARY KEY,
        reason TEXT NOT NULL
    ) STRICT;

    CREATE TABLE engine_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        id_generator BLOB NOT NULL
    ) STRICT;

    CREATE TABLE chunk_cache (
        hash BLOB PRIMARY KEY CHECK (length(hash) = 32),
        size INTEGER NOT NULL,
        last_used_ms INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX chunk_cache_by_use ON chunk_cache (last_used_ms);
