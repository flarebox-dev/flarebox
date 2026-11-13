
    CREATE TABLE trashed_here (
        node_id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_slots (
        device_seq INTEGER PRIMARY KEY,
        seq INTEGER NOT NULL
    ) STRICT;
