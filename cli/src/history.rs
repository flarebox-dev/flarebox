// SPDX-License-Identifier: Apache-2.0

//! `flarebox trash` and `flarebox versions`: what the space of a linked folder keeps after a delete or an edit. Both
//! read the server through its HTTP RPC route with this profile's credential. They change no file here or on the
//! server, and without a daemon they read the profile without writing to it.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use flarebox_driver::{Driver, DriverError, FolderStatus, Paths};
use flarebox_engine::names::name_key;
use flarebox_net::Auth;
use flarebox_proto::v1::{
    ListTrash, ListTree, ListVersions, NodeKind, NodeState, VersionInfo, rpc_request::Call,
    rpc_response,
};

use crate::ipc::{self, Request, Response};
use crate::{ago, now_ms, print_json, printable, refusal, signed_in, size};

/// The most entries asked for in one page; the server sends at most this many.
const PAGE: u32 = 1000;
const UNEXPECTED: &str = "the server answered with another kind of result";

/// `flarebox trash [folder]`: the deletions in the trash of the folder's space, or of the part under `folder` when it is a
/// folder inside a linked one.
pub async fn trash(paths: &Paths, folder: &Path, json: bool) -> Result<(), String> {
    let (linked, below) = locate(paths, folder).await?;
    let auth = signed_in(paths)?;
    let space = linked.space_id.as_str();
    let live = tree(&auth, space).await?;
    let trashed = trash_nodes(&auth, space).await?;
    let mut found = deletions(&live, &trashed, space);
    found.retain(|deleted| starts_with(&deleted.names, &below));
    if json {
        return print_json(&found.iter().map(Deleted::json).collect::<Vec<_>>());
    }
    print!("{}", trash_report(&found, &linked));
    Ok(())
}

/// `flarebox versions <file>`: the versions the server keeps of a live file, newest first.
pub async fn versions(paths: &Paths, file: &Path, json: bool) -> Result<(), String> {
    let (linked, below) = locate(paths, file).await?;
    if below.is_empty() {
        return Err(format!(
            "{} is the linked folder itself; name a file in it",
            linked.path
        ));
    }
    let auth = signed_in(paths)?;
    let space = linked.space_id.as_str();
    let live = tree(&auth, space).await?;
    let node = find(&live, space, &below).ok_or_else(|| {
        format!(
            "the server has no file at {} in {}; flarebox trash lists deleted ones",
            printable(&below.join("/")),
            linked.path
        )
    })?;
    if node.kind() == NodeKind::Dir {
        return Err(format!(
            "{} is a folder; name a file in it",
            printable(&below.join("/"))
        ));
    }
    let kept = versions_of(&auth, space, &node.node_id).await?;
    if json {
        return print_json(&kept.iter().map(version_json).collect::<Vec<_>>());
    }
    print!(
        "{}",
        versions_report(&below.join("/"), node, &kept, now_ms())
    );
    Ok(())
}

/// The linked folder that holds `path`, and the names below it as typed. The folders come from the daemon, or from the
/// profile's stores read without writing anything when no daemon runs. `path` need not exist here, since a file
/// deleted on this device may still be on the server. Symbolic links are followed only up to the linked folder: below
/// it a link is a node of the space like any other, so the names stay as typed and never select the node a local link
/// points to.
async fn locate(paths: &Paths, path: &Path) -> Result<(FolderStatus, Vec<String>), String> {
    let folders = match ipc::ask(paths, &Request::Status).await? {
        Some(Response::Status { status }) => status.folders,
        Some(Response::Error { message }) => return Err(message),
        Some(other) => return Err(format!("the daemon answered {other:?}")),
        None => Driver::linked_folders(paths).map_err(|error| match error {
            DriverError::Busy(_) => ipc::busy(paths),
            other => other.to_string(),
        })?,
    };
    let typed = lexical(&crate::absolute(path)?);
    let components: Vec<Component> = typed.components().collect();
    let mut prefix = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component);
        let Ok(real) = std::fs::canonicalize(&prefix) else {
            break;
        };
        let inside = folders
            .iter()
            .filter_map(|folder| Some((folder, below(Path::new(&folder.path), &real)?)))
            .max_by_key(|(folder, _)| folder.path.len());
        if let Some((folder, mut names)) = inside {
            names.extend(
                components[index + 1..]
                    .iter()
                    .map(|name| name.as_os_str().to_string_lossy().into_owned()),
            );
            return Ok((folder.clone(), names));
        }
    }
    Err(format!(
        "{} is not in a linked folder; flarebox status lists them",
        typed.display()
    ))
}

/// `path` without `.` names, and with each `..` removing the name before it, worked out without reading the disk.
fn lexical(path: &Path) -> PathBuf {
    let mut normal = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normal.pop();
            }
            other => normal.push(other),
        }
    }
    normal
}

/// The names of `path` below `root`, or `None` when it is not inside it.
fn below(root: &Path, path: &Path) -> Option<Vec<String>> {
    path.strip_prefix(root)
        .ok()?
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// Whether `names` begins with `prefix`, comparing names as the server does.
fn starts_with(names: &[String], prefix: &[String]) -> bool {
    names.len() >= prefix.len()
        && names
            .iter()
            .zip(prefix)
            .all(|(name, wanted)| name_key(name) == name_key(wanted))
}

async fn call(auth: &Auth, space: &str, call: Call) -> Result<rpc_response::Result, String> {
    match auth
        .api()
        .rpc(auth, space, call)
        .await
        .map_err(refusal)?
        .result
    {
        Some(rpc_response::Result::Error(error)) => {
            Err(format!("the server refused: {}", error.message))
        }
        Some(result) => Ok(result),
        None => Err("the server sent an empty answer".into()),
    }
}

/// Every page of a listing, from the empty cursor to the page whose `next_cursor` is empty. A cursor the server sends
/// twice ends the listing with an error instead of a loop.
async fn every_page<T>(
    mut page: impl AsyncFnMut(String) -> Result<(Vec<T>, String), String>,
) -> Result<Vec<T>, String> {
    let mut all = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    loop {
        let (items, next) = page(cursor).await?;
        all.extend(items);
        if next.is_empty() {
            return Ok(all);
        }
        if !seen.insert(next.clone()) {
            return Err("the server sent the same page cursor twice".into());
        }
        cursor = next;
    }
}

async fn tree(auth: &Auth, space: &str) -> Result<Vec<NodeState>, String> {
    every_page(async |cursor| {
        match call(
            auth,
            space,
            Call::ListTree(ListTree {
                cursor,
                limit: PAGE,
            }),
        )
        .await?
        {
            rpc_response::Result::Tree(page) => Ok((page.nodes, page.next_cursor)),
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await
}

async fn trash_nodes(auth: &Auth, space: &str) -> Result<Vec<NodeState>, String> {
    every_page(async |cursor| {
        match call(
            auth,
            space,
            Call::ListTrash(ListTrash {
                cursor,
                limit: PAGE,
            }),
        )
        .await?
        {
            rpc_response::Result::Trash(page) => Ok((page.nodes, page.next_cursor)),
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await
}

async fn versions_of(auth: &Auth, space: &str, node_id: &str) -> Result<Vec<VersionInfo>, String> {
    every_page(async |cursor| {
        let request = ListVersions {
            node_id: node_id.to_owned(),
            cursor,
            limit: PAGE,
        };
        match call(auth, space, Call::ListVersions(request)).await? {
            rpc_response::Result::Versions(list) => Ok((list.versions, list.next_cursor)),
            _ => Err(UNEXPECTED.into()),
        }
    })
    .await
}

/// The live node at `names` below the space's root, whose node id is the space id. Names compare by `name_key`, under which
/// no two live children of a folder collide, so a name differing only in case or Unicode form still finds its node.
fn find<'a>(live: &'a [NodeState], root: &str, names: &[String]) -> Option<&'a NodeState> {
    let children: HashMap<(&str, String), &NodeState> = live
        .iter()
        .map(|node| ((node.parent_id.as_str(), name_key(&node.name)), node))
        .collect();
    let mut parent = root;
    let mut found = None;
    for name in names {
        let node = *children.get(&(parent, name_key(name)))?;
        parent = &node.node_id;
        found = Some(node);
    }
    found
}

/// The names from the root down to `node`, through the nodes in `by_id`. A parent the listings do not hold, such as
/// one purged since, shows as "…".
fn names_of(node: &NodeState, by_id: &HashMap<&str, &NodeState>, root: &str) -> Vec<String> {
    let mut names = vec![node.name.clone()];
    let mut parent = node.parent_id.as_str();
    while parent != root {
        match by_id.get(parent) {
            Some(next) if names.len() <= by_id.len() => {
                names.push(next.name.clone());
                parent = &next.parent_id;
            }
            _ => {
                names.push("…".into());
                break;
            }
        }
    }
    names.reverse();
    names
}

/// One deletion in the trash: a trashed node whose parent was not trashed with it, and what was trashed beneath it.
#[derive(Debug)]
struct Deleted<'a> {
    node: &'a NodeState,
    names: Vec<String>,
    files: u64,
    bytes: u64,
}

impl Deleted<'_> {
    fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "node_id": self.node.node_id,
            "path": self.names.join("/"),
            "kind": kind(self.node),
            "trash_group": self.node.trash_group,
            "files": self.files,
            "bytes": self.bytes,
        })
    }
}

/// The trash as deletions, sorted by path. A file counts with its own size; a folder counts the files trashed with it.
fn deletions<'a>(live: &'a [NodeState], trash: &'a [NodeState], root: &str) -> Vec<Deleted<'a>> {
    let by_id: HashMap<&str, &NodeState> = live
        .iter()
        .chain(trash)
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let trashed: HashMap<&str, &NodeState> = trash
        .iter()
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let top = |node: &'a NodeState| -> &'a NodeState {
        let mut top = node;
        for _ in 0..trash.len() {
            match trashed.get(top.parent_id.as_str()) {
                Some(parent) if parent.trash_group == top.trash_group => top = parent,
                _ => break,
            }
        }
        top
    };
    let mut found: HashMap<&str, Deleted<'a>> = HashMap::new();
    for node in trash {
        let head = top(node);
        let deleted = found
            .entry(head.node_id.as_str())
            .or_insert_with(|| Deleted {
                node: head,
                names: names_of(head, &by_id, root),
                files: 0,
                bytes: 0,
            });
        if node.kind() != NodeKind::Dir {
            deleted.files += 1;
            deleted.bytes += content_size(node.current.as_ref());
        }
    }
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| {
        a.names
            .cmp(&b.names)
            .then(a.node.node_id.cmp(&b.node.node_id))
    });
    found
}

fn content_size(version: Option<&VersionInfo>) -> u64 {
    version
        .and_then(|version| version.content.as_ref())
        .map_or(0, |content| content.size)
}

fn kind(node: &NodeState) -> &'static str {
    match node.kind() {
        NodeKind::Dir => "folder",
        NodeKind::Symlink => "symlink",
        _ => "file",
    }
}

fn trash_report(found: &[Deleted], linked: &FolderStatus) -> String {
    if found.is_empty() {
        return format!("Nothing from {} is in the trash.\n", linked.path);
    }
    let mut report = String::new();
    for deleted in found {
        let path = printable(&deleted.names.join("/"));
        let line = match deleted.node.kind() {
            NodeKind::Dir => format!(
                "{:>9}  {path}/  (folder, {})",
                size(deleted.bytes),
                crate::count(deleted.files, "file")
            ),
            _ => format!("{:>9}  {path}", size(deleted.bytes)),
        };
        report.push_str(&line);
        report.push('\n');
    }
    report
}

fn version_json(version: &VersionInfo) -> serde_json::Value {
    serde_json::json!({
        "version_id": version.version_id,
        "seq": version.seq,
        "size": content_size(Some(version)),
        "device_id": version.device_id,
        "created_at_ms": version.created_at_ms,
    })
}

fn versions_report(path: &str, node: &NodeState, kept: &[VersionInfo], now_ms: i64) -> String {
    let current = node
        .current
        .as_ref()
        .map(|version| version.version_id.as_str());
    let mut report = format!(
        "{}: {}, newest first\n",
        printable(path),
        crate::count(kept.len() as u64, "version")
    );
    for version in kept {
        let marker = if Some(version.version_id.as_str()) == current {
            "current"
        } else {
            ""
        };
        report.push_str(&format!(
            "{marker:>7}  {:>9}  {:<14}  {}\n",
            size(content_size(Some(version))),
            ago(version.created_at_ms, now_ms),
            version.version_id
        ));
    }
    report
}

#[cfg(test)]
mod tests {
    use flarebox_driver::Credential;
    use flarebox_proto::v1::{
        AccessToken, Content, Error, NodeStatus, RpcRequest, RpcResponse, TrashPage, TreePage,
        VersionList,
    };
    use prost::Message;

    use super::*;
    use crate::setup::fake::{Fake, proto};

    const SPACE: &str = "0194f8a2-0000-7000-8000-00000000000a";

    fn node(
        id: &str,
        parent: &str,
        name: &str,
        kind: NodeKind,
        size: u64,
        group: &str,
    ) -> NodeState {
        let status = if group.is_empty() {
            NodeStatus::Live
        } else {
            NodeStatus::Trashed
        };
        NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            kind: kind as i32,
            status: status as i32,
            trash_group: group.into(),
            current: Some(version(&format!("{id}-1"), size, 0)),
            ..NodeState::default()
        }
    }

    fn version(id: &str, size: u64, created_at_ms: i64) -> VersionInfo {
        VersionInfo {
            version_id: id.into(),
            content: Some(Content {
                size,
                ..Content::default()
            }),
            created_at_ms,
            ..VersionInfo::default()
        }
    }

    fn names(path: &str) -> Vec<String> {
        path.split('/').map(String::from).collect()
    }

    fn answer(result: rpc_response::Result) -> RpcResponse {
        RpcResponse {
            req_id: 1,
            result: Some(result),
        }
    }

    /// A signed-in device of the server `fake`, which answers its token request first.
    fn signed_in_to(fake: &Fake) -> Auth {
        let credential = Credential {
            server: fake.base.to_string(),
            user_id: "user".into(),
            user_name: None,
            device_id: "0194f8a2-0000-7000-8000-000000000001".into(),
            device_name: "laptop".into(),
            refresh_credential: "refresh".into(),
        };
        Auth::new(credential.api().unwrap(), credential.net())
    }

    fn token() -> crate::setup::fake::Reply {
        let token = AccessToken {
            token: "access".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        proto("POST", "/v1/token", token)
    }

    #[tokio::test]
    async fn every_page_follows_the_cursor_to_the_last_page() {
        let mut asked = Vec::new();
        let all = every_page(async |cursor: String| {
            asked.push(cursor.clone());
            Ok(match cursor.as_str() {
                "" => (vec![1, 2], "b".to_owned()),
                "b" => (vec![3], "c".to_owned()),
                _ => (vec![4], String::new()),
            })
        })
        .await
        .unwrap();
        assert_eq!(all, [1, 2, 3, 4]);
        assert_eq!(asked, ["", "b", "c"]);

        let error = every_page(async |_cursor: String| Ok((vec![0], "same".to_owned())))
            .await
            .unwrap_err();
        assert!(error.contains("same page cursor twice"), "{error}");
    }

    #[tokio::test]
    async fn versions_are_read_page_by_page_over_http() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let page = |ids: &[&str], next: &str| {
            let list = VersionList {
                versions: ids.iter().map(|id| version(id, 1, 0)).collect(),
                next_cursor: next.into(),
            };
            proto("POST", &rpc, answer(rpc_response::Result::Versions(list)))
        };
        let fake = Fake::start(vec![
            token(),
            page(&["v3", "v2"], "after-v2"),
            page(&["v1"], ""),
        ])
        .await;

        let kept = versions_of(&signed_in_to(&fake), SPACE, "node")
            .await
            .unwrap();

        let ids: Vec<_> = kept
            .iter()
            .map(|version| version.version_id.as_str())
            .collect();
        assert_eq!(ids, ["v3", "v2", "v1"]);
        let asked: Vec<_> = fake
            .seen()
            .iter()
            .filter(|seen| seen.path == rpc)
            .map(
                |seen| match RpcRequest::decode(seen.body.as_slice()).unwrap().call {
                    Some(Call::ListVersions(list)) => (list.node_id, list.cursor),
                    other => panic!("expected ListVersions, got {other:?}"),
                },
            )
            .collect();
        assert_eq!(
            asked,
            [
                ("node".to_owned(), String::new()),
                ("node".to_owned(), "after-v2".to_owned())
            ]
        );
    }

    #[tokio::test]
    async fn the_tree_and_the_trash_are_read_over_http_and_a_refusal_stops_them() {
        let rpc = format!("/v1/spaces/{SPACE}/rpc");
        let tree_page = TreePage {
            nodes: vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")],
            ..TreePage::default()
        };
        let trash_page = TrashPage {
            nodes: vec![node("old", "docs", "old.txt", NodeKind::File, 12, "g")],
            next_cursor: String::new(),
        };
        let refusal = Error {
            message: "not a member of this space".into(),
            ..Error::default()
        };
        let fake = Fake::start(vec![
            token(),
            proto("POST", &rpc, answer(rpc_response::Result::Tree(tree_page))),
            proto(
                "POST",
                &rpc,
                answer(rpc_response::Result::Trash(trash_page)),
            ),
            proto("POST", &rpc, answer(rpc_response::Result::Error(refusal))),
        ])
        .await;
        let auth = signed_in_to(&fake);

        let live = tree(&auth, SPACE).await.unwrap();
        let trashed = trash_nodes(&auth, SPACE).await.unwrap();
        let error = tree(&auth, SPACE).await.unwrap_err();

        assert_eq!(live[0].node_id, "docs");
        assert_eq!(trashed[0].node_id, "old");
        assert_eq!(error, "the server refused: not a member of this space");
    }

    #[test]
    fn a_path_finds_its_live_node_by_name_key() {
        let live = vec![
            node("docs", SPACE, "Docs", NodeKind::Dir, 0, ""),
            node("cafe", "docs", "Café.txt", NodeKind::File, 5, ""),
            node("other", SPACE, "café.txt", NodeKind::File, 1, ""),
        ];
        let found = |path: &str| find(&live, SPACE, &names(path)).map(|node| node.node_id.as_str());

        assert_eq!(found("docs/CAFE\u{301}.TXT"), Some("cafe"));
        assert_eq!(found("Docs/Café.txt"), Some("cafe"));
        assert_eq!(found("café.txt"), Some("other"));
        assert_eq!(found("docs/missing.txt"), None);
        assert_eq!(found("café.txt/below"), None);
    }

    #[tokio::test]
    async fn without_a_daemon_the_commands_leave_a_fresh_profile_alone() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(&dir.path().join("profile"));
        let folder = dir.path().join("Sync");
        std::fs::create_dir(&folder).unwrap();

        let trashed = trash(&paths, &folder, false).await.unwrap_err();
        let kept = versions(&paths, &folder.join("a.txt"), false)
            .await
            .unwrap_err();

        assert!(trashed.contains("not in a linked folder"), "{trashed}");
        assert!(kept.contains("not in a linked folder"), "{kept}");
        assert!(!dir.path().join("profile").exists());
    }

    #[test]
    fn a_path_is_normalized_without_reading_the_disk() {
        assert_eq!(
            lexical(Path::new("/home/ada/./Flarebox/docs/../link.txt")),
            Path::new("/home/ada/Flarebox/link.txt")
        );
    }

    #[test]
    fn a_path_below_a_linked_folder_gives_its_names() {
        let root = Path::new("/home/ada/Flarebox");
        assert_eq!(
            below(root, Path::new("/home/ada/Flarebox/docs/a.txt")),
            Some(names("docs/a.txt"))
        );
        assert_eq!(below(root, root), Some(Vec::new()));
        assert_eq!(below(root, Path::new("/home/ada/Flarebox2/a.txt")), None);
        assert!(starts_with(&names("Docs/a.txt"), &names("docs")));
        assert!(!starts_with(&names("docs"), &names("docs/a.txt")));
    }

    #[test]
    fn the_trash_lists_each_deletion_once_with_what_it_held() {
        let live = vec![node("docs", SPACE, "docs", NodeKind::Dir, 0, "")];
        let trash = vec![
            node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
            node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
            node("trip", "photos", "trip", NodeKind::Dir, 0, "g1"),
            node("dog", "trip", "dog.jpg", NodeKind::File, 700, "g1"),
            node("late", "photos", "late.jpg", NodeKind::File, 9, "g2"),
            node("old", "docs", "old.txt", NodeKind::File, 12, "g3"),
            node("lost", "purged", "lost.txt", NodeKind::File, 5, "g4"),
        ];

        let found = deletions(&live, &trash, SPACE);

        let summary: Vec<_> = found
            .iter()
            .map(|deleted| (deleted.names.join("/"), deleted.files, deleted.bytes))
            .collect();
        assert_eq!(
            summary,
            [
                ("docs/old.txt".to_owned(), 1, 12),
                ("photos".to_owned(), 2, 1000),
                ("photos/late.jpg".to_owned(), 1, 9),
                ("…/lost.txt".to_owned(), 1, 5),
            ]
        );
        let linked = FolderStatus {
            path: "/home/ada/Flarebox".into(),
            ..serde_json::from_value(serde_json::json!({
                "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
                "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
                "local_entries": null, "questions": 0
            }))
            .unwrap()
        };
        assert_eq!(
            trash_report(&found[..2], &linked),
            "     12 B  docs/old.txt\n   1.0 kB  photos/  (folder, 2 files)\n"
        );
        assert_eq!(
            trash_report(&[], &linked),
            "Nothing from /home/ada/Flarebox is in the trash.\n"
        );
    }

    #[test]
    fn the_versions_report_marks_the_current_one() {
        let file = node("note", SPACE, "note.txt", NodeKind::File, 20, "");
        let kept = [version("note-1", 20, 3_000_000), version("note-0", 10, 0)];

        let report = versions_report("note.txt", &file, &kept, 3_000_000 + 7_200_000);

        assert_eq!(
            report,
            "note.txt: 2 versions, newest first\ncurrent       20 B  2 h ago         note-1\n              10 B  never           note-0\n"
        );
    }
}
