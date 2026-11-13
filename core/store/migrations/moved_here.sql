
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
      JOIN synced_nodes ON synced_nodes.node_id = remote_nodes.node_id
     WHERE remote_nodes.status = 1
       AND synced_nodes.parent_id IS remote_nodes.parent_id
       AND synced_nodes.name = remote_nodes.name;
