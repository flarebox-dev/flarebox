
    CREATE TABLE recorded_nodes (
        node_id TEXT PRIMARY KEY,
        displaced INTEGER NOT NULL CHECK (displaced IN (0, 1)),
        dev INTEGER,
        ino INTEGER,
        birth_ns INTEGER,
        CHECK ((displaced = 1) = (dev IS NOT NULL)),
        CHECK ((dev IS NULL) = (ino IS NULL)),
        CHECK (dev IS NOT NULL OR birth_ns IS NULL)
    ) STRICT, WITHOUT ROWID;
