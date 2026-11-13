
    ALTER TABLE waiting_updates ADD COLUMN slot INTEGER CHECK (slot IS NULL OR slot > 0);
    UPDATE waiting_updates
       SET slot = (SELECT accepted FROM commit_state WHERE id = 1)
     WHERE node_id NOT IN (SELECT node_id FROM discarded_nodes)
       AND (SELECT accepted FROM commit_state WHERE id = 1) > 0;
