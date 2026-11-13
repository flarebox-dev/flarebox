
    ALTER TABLE remote_cursor ADD COLUMN commit_id TEXT NOT NULL DEFAULT '';
    ALTER TABLE remote_cursor ADD COLUMN committed_at_ms INTEGER NOT NULL DEFAULT 0;
