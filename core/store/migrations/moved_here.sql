
    CREATE TABLE moved_here (
        node_id TEXT PRIMARY KEY,
        seq INTEGER NOT NULL,
        upgraded INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE past_positions (
        node_id TEXT NOT NULL,
        position INTEGER NOT NULL,
        rev INTEGER NOT NULL,
        parent_id TEXT,
        name TEXT NOT NULL,
        PRIMARY KEY (node_id, position)
    ) STRICT, WITHOUT ROWID;

    INSERT INTO moved_here (node_id, seq, upgraded)
    SELECT remote_nodes.node_id, remote_nodes.meta_rev, 1
      FROM remote_nodes
      JOIN commit_slots ON commit_slots.seq = remote_nodes.meta_rev
     WHERE remote_nodes.status = 1;
