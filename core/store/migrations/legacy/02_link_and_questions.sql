
    CREATE TABLE link (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        space_id TEXT NOT NULL,
        space_name TEXT NOT NULL,
        root BLOB NOT NULL,
        root_dev INTEGER NOT NULL,
        server_url TEXT NOT NULL,
        device_id TEXT NOT NULL,
        linked_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE questions (
        question_id TEXT PRIMARY KEY,
        asked_at_ms INTEGER NOT NULL,
        body BLOB NOT NULL,
        answer TEXT,
        answered_at_ms INTEGER,
        CHECK ((answer IS NULL) = (answered_at_ms IS NULL))
    ) STRICT;
