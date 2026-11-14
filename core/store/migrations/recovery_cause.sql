ALTER TABLE commit_state ADD COLUMN recovery_trigger TEXT
    CHECK (recovery_trigger IN ('space_rewound', 'store_rolled_back', 'store_older_than_disk'));
ALTER TABLE commit_state ADD COLUMN restore_seq INTEGER;
ALTER TABLE commit_state ADD COLUMN restore_point_ms INTEGER;
UPDATE commit_state SET recovery_trigger = 'store_rolled_back' WHERE halt = 'recovery';
