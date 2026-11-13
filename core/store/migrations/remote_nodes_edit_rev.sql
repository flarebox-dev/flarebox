ALTER TABLE remote_nodes ADD COLUMN edit_rev INTEGER NOT NULL DEFAULT 0;
UPDATE remote_nodes SET edit_rev = content_rev;
