CREATE TABLE held_contents (
    node_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    digest BLOB NOT NULL,
    PRIMARY KEY (node_id, position)
) STRICT, WITHOUT ROWID;
