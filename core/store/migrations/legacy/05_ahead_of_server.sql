
    ALTER TABLE remote_cursor ADD COLUMN ahead_of_server INTEGER NOT NULL DEFAULT 0
        CHECK (ahead_of_server IN (0, 1));
