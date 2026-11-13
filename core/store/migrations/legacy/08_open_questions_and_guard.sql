
    CREATE TABLE open_questions (
        question_id TEXT PRIMARY KEY,
        position INTEGER NOT NULL UNIQUE,
        asked_at_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        answer TEXT
    ) STRICT;

    CREATE TABLE open_question_items (
        question_id TEXT NOT NULL REFERENCES open_questions (question_id),
        top_id TEXT NOT NULL,
        files INTEGER NOT NULL,
        PRIMARY KEY (question_id, top_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE open_question_members (
        question_id TEXT NOT NULL,
        top_id TEXT NOT NULL,
        node_id TEXT NOT NULL,
        content_rev INTEGER,
        meta_rev INTEGER,
        PRIMARY KEY (question_id, top_id, node_id),
        FOREIGN KEY (question_id, top_id) REFERENCES open_question_items (question_id, top_id),
        CHECK ((content_rev IS NULL) = (meta_rev IS NULL))
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_ledger (
        node_id TEXT PRIMARY KEY,
        at_ms INTEGER NOT NULL,
        files INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed (
        top_id TEXT PRIMARY KEY,
        files INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE guard_confirmed_members (
        top_id TEXT NOT NULL REFERENCES guard_confirmed (top_id),
        node_id TEXT NOT NULL,
        content_rev INTEGER,
        meta_rev INTEGER,
        PRIMARY KEY (top_id, node_id),
        CHECK ((content_rev IS NULL) = (meta_rev IS NULL))
    ) STRICT, WITHOUT ROWID;
