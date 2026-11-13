CREATE TABLE removed_here (
    node_id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL
) STRICT, WITHOUT ROWID;

CREATE TABLE commit_in_flight_restore_members (
    target TEXT NOT NULL,
    position INTEGER NOT NULL,
    node_id TEXT NOT NULL,
    PRIMARY KEY (target, position)
) STRICT, WITHOUT ROWID;
