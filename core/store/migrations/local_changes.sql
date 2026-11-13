CREATE TABLE local_changes (
    node_id TEXT PRIMARY KEY
) STRICT, WITHOUT ROWID;

INSERT INTO local_changes (node_id)
SELECT local_nodes.node_id
  FROM local_nodes
  LEFT JOIN synced_nodes ON synced_nodes.node_id = local_nodes.node_id
 WHERE synced_nodes.node_id IS NULL
    OR synced_nodes.parent_id IS NOT local_nodes.parent_id
    OR synced_nodes.kind IS NOT local_nodes.kind
    OR NOT same_name(synced_nodes.name, local_nodes.name)
    OR NOT same_content(synced_nodes.content, local_nodes.content);
