CREATE TABLE discarded_shapes (
    node_id TEXT PRIMARY KEY,
    parent_id TEXT,
    name TEXT NOT NULL,
    kind INTEGER NOT NULL,
    content BLOB
) STRICT, WITHOUT ROWID;
INSERT INTO discarded_shapes (node_id, parent_id, name, kind, content)
SELECT node_id, parent_id, name, kind, content FROM waiting_updates
 WHERE name IS NOT NULL AND node_id IN (SELECT node_id FROM discarded_nodes);
INSERT OR IGNORE INTO discarded_shapes (node_id, parent_id, name, kind, content)
SELECT node_id, parent_id, name, kind, content FROM synced_nodes
 WHERE node_id IN (SELECT node_id FROM discarded_nodes);
CREATE TEMP TABLE undecided AS
SELECT node_id, parent_id, name, kind, content FROM local_nodes
 WHERE node_id IN (SELECT node_id FROM discarded_nodes)
   AND node_id NOT IN (SELECT node_id FROM discarded_shapes);
CREATE TEMP TABLE asked_again AS
SELECT lower(hex(randomblob(4)) || '-' || hex(randomblob(2)) || '-' || hex(randomblob(2))
             || '-' || hex(randomblob(2)) || '-' || hex(randomblob(6))) AS question_id
 WHERE EXISTS (SELECT 1 FROM undecided);
INSERT INTO open_questions (question_id, position, asked_at_ms, kind, answer)
SELECT question_id, (SELECT COALESCE(MAX(position) + 1, 0) FROM open_questions),
       CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER),
       '{"type":"recovery","group":"created_on_this_device"}', NULL
  FROM asked_again;
INSERT INTO open_question_items (question_id, top_id, files)
SELECT question_id, node_id, kind = 2 FROM asked_again, undecided;
INSERT INTO open_question_members (question_id, top_id, node_id, content_rev, meta_rev)
SELECT question_id, node_id, node_id, NULL, NULL FROM asked_again, undecided;
INSERT INTO open_question_shapes (question_id, node_id, parent_id, name, kind, content)
SELECT question_id, node_id, parent_id, name, kind, content FROM asked_again, undecided;
DROP TABLE undecided;
DROP TABLE asked_again;
DROP TABLE discarded_nodes;
ALTER TABLE discarded_shapes RENAME TO discarded_nodes;
