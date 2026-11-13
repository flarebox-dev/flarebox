
    CREATE TABLE taken_answers (
        question_id TEXT PRIMARY KEY,
        asked_at_ms INTEGER NOT NULL,
        body BLOB NOT NULL,
        answer TEXT NOT NULL,
        answered_at_ms INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;
