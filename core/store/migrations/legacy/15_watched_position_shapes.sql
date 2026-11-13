
    ALTER TABLE watched_positions ADD COLUMN parent_id TEXT;
    ALTER TABLE watched_positions ADD COLUMN name TEXT;
    UPDATE watched_positions
    SET parent_id = synced_nodes.parent_id, name = synced_nodes.name
    FROM synced_nodes
    WHERE synced_nodes.node_id = watched_positions.node_id AND watched_positions.rev IS NOT NULL;
    UPDATE watched_positions SET rev = NULL WHERE name IS NULL;
