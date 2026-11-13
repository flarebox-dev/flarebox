
    DROP TABLE commit_in_flight_shapes;
    DROP TABLE problems;
    ALTER TABLE commit_in_flight ADD COLUMN result BLOB;

    CREATE TABLE commit_in_flight_deletes (
        target TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_in_flight_delete_members (
        target TEXT NOT NULL,
        position INTEGER NOT NULL,
        node_id TEXT NOT NULL,
        PRIMARY KEY (target, position)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE commit_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        accepted INTEGER NOT NULL,
        halt TEXT CHECK (halt IN ('read_only', 'recovery', 'policy')),
        policy_code INTEGER,
        cap INTEGER CHECK (cap > 0),
        CHECK ((halt IS 'policy') = (policy_code IS NOT NULL))
    ) STRICT;

    CREATE TABLE effect_state (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        cleared INTEGER NOT NULL,
        in_flight BLOB
    ) STRICT;

    CREATE TABLE failed_effects (
        node_id TEXT PRIMARY KEY,
        effect BLOB NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE node_sets (
        node_set TEXT NOT NULL
            CHECK (node_set IN ('dirty', 'unsearched', 'reverted', 'problems', 'split')),
        node_id TEXT NOT NULL,
        PRIMARY KEY (node_set, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE node_maps (
        node_map TEXT NOT NULL CHECK (node_map IN ('parked_as', 'aliases')),
        node_id TEXT NOT NULL,
        target TEXT NOT NULL,
        PRIMARY KEY (node_map, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE waiting_updates (
        node_id TEXT PRIMARY KEY,
        parent_id TEXT,
        name TEXT,
        kind INTEGER,
        content BLOB,
        CHECK ((name IS NULL) = (kind IS NULL))
    ) STRICT, WITHOUT ROWID;
