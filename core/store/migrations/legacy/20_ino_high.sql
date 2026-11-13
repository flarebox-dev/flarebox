
    ALTER TABLE local_nodes ADD COLUMN ino_high INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE recorded_nodes
        ADD COLUMN ino_high INTEGER NOT NULL DEFAULT 0 CHECK (ino IS NOT NULL OR ino_high = 0);
