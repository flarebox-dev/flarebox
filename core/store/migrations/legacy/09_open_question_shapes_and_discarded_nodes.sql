
    CREATE TABLE open_question_shapes (
        question_id TEXT NOT NULL REFERENCES open_questions (question_id),
        node_id TEXT NOT NULL,
        parent_id TEXT NOT NULL,
        name TEXT NOT NULL,
        kind INTEGER NOT NULL,
        content BLOB,
        PRIMARY KEY (question_id, node_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE discarded_nodes (
        node_id TEXT PRIMARY KEY
    ) STRICT, WITHOUT ROWID;
