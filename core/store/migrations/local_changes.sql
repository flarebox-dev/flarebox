CREATE TABLE local_changes (
    node_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

INSERT INTO local_changes (node_id)
SELECT local_nodes.node_id
  FROM local_nodes
  LEFT JOIN synced_nodes ON synced_nodes.node_id = local_nodes.node_id
 WHERE synced_nodes.node_id IS NULL
    OR synced_nodes.parent_id IS NOT local_nodes.parent_id
    OR synced_nodes.name IS NOT local_nodes.name
    OR synced_nodes.kind IS NOT local_nodes.kind
    OR synced_nodes.content IS NOT local_nodes.content;
