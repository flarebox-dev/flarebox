
    CREATE TABLE restore_requests (
        restore_id TEXT PRIMARY KEY,
        node_id TEXT NOT NULL,
        version_id TEXT,
        target_parent_id TEXT,
        name TEXT,
        CHECK (version_id IS NULL OR (target_parent_id IS NULL AND name IS NULL))
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE restore_intents (
        restore_id TEXT PRIMARY KEY,
        node_id TEXT NOT NULL,
        version_id TEXT,
        target_parent_id TEXT,
        name TEXT,
        position INTEGER UNIQUE,
        outcome TEXT
            CHECK (outcome IN ('restored', 'renamed', 'copied', 'rejected', 'does_not_fit')),
        outcome_node_id TEXT,
        outcome_name TEXT,
        reject_reason TEXT,
        CHECK (version_id IS NULL OR (target_parent_id IS NULL AND name IS NULL)),
        CHECK ((outcome IS NULL) = (position IS NULL)),
        CHECK ((outcome IS 'renamed' OR outcome IS 'copied') = (outcome_name IS NOT NULL)),
        CHECK ((outcome IS 'copied') = (outcome_node_id IS NOT NULL)),
        CHECK ((outcome IS 'rejected') = (reject_reason IS NOT NULL))
    ) STRICT, WITHOUT ROWID;
