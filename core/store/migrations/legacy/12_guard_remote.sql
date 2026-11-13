
    CREATE TABLE guard_remote (
        top_id TEXT NOT NULL,
        known INTEGER NOT NULL CHECK (known IN (0, 1)),
        trash_group TEXT NOT NULL,
        trashed_seq INTEGER NOT NULL,
        at_ms INTEGER NOT NULL,
        files INTEGER NOT NULL,
        PRIMARY KEY (top_id, known, trash_group, trashed_seq)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_remote_dismissed (
        top_id TEXT NOT NULL,
        known INTEGER NOT NULL CHECK (known IN (0, 1)),
        trash_group TEXT NOT NULL,
        trashed_seq INTEGER NOT NULL,
        PRIMARY KEY (top_id, known, trash_group, trashed_seq)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed_limits (
        limit_crossed TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;
