// SPDX-License-Identifier: Apache-2.0

//! `koi trash` and `koi versions`: what the space of a linked folder keeps after a delete or an edit. Both
//! read the server through its HTTP RPC route with this profile's credential. They change no file here or on the
//! server, and without a daemon they read the profile without writing to it. `koi restore` reads the same
//! listings to find what to restore and records the request in the folder's store, for the folder's engine to send
//! (design §6.3). `koi restores` lists those requests and how each went.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use koidrive_driver::{
    Driver, DriverError, FolderState, FolderStatus, Paths, RestoreRejection, RestoreRequest,
    RestoreState, RestoreStatus, RestoreWait,
};
use koidrive_engine::names::name_key;
use koidrive_net::Auth;
use koidrive_proto::v1::{
    ListTrash, ListTree, ListVersions, NodeKind, NodeState, VersionInfo, rpc_request::Call,
    rpc_response,
};

use crate::ipc::{self, Request, Response};
use crate::{ago, now_ms, print_json, printable, refusal, signed_in, size};

/// The most entries asked for in one page; the server sends at most this many.
const PAGE: u32 = 1000;
const UNEXPECTED: &str = "the server answered with another kind of result";

/// `koi trash [folder]`: the deletions in the trash of the folder's space, or of the part under `folder` when it is a
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

/// `koi versions <file>`: the versions the server keeps of a live file, newest first.
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
            "the server has no file at {} in {}; koi trash lists deleted ones",
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

/// `koi restore <path>`: asks the folder's engine to bring the deletion at `path` back from the trash, or, with
/// `version`, to make that version of the live file at `path` current again. `node` picks one of several deletions
/// that had the path. The request is checked against the server's listings first, so a typo fails here and not after
/// the next commit.
pub async fn restore(
    paths: &Paths,
    path: &Path,
    version: Option<String>,
    node: Option<String>,
) -> Result<(), String> {
    let (linked, below) = locate(paths, path).await?;
    if below.is_empty() {
        return Err(format!(
            "{} is the linked folder itself; name a file or folder in it",
            linked.path
        ));
    }
    let auth = signed_in(paths)?;
    let space = linked.space_id.as_str();
    let shown = printable(&below.join("/"));
    let live = tree(&auth, space).await?;
    let request = match version {
        Some(version_id) => {
            let file = find(&live, space, &below).ok_or_else(|| {
                format!(
                    "the server has no file at {shown} in {}; koi trash lists deleted ones",
                    linked.path
                )
            })?;
            if file.kind() == NodeKind::Dir {
                return Err(format!("{shown} is a folder; only a file has versions"));
            }
            if file
                .current
                .as_ref()
                .map(|current| current.version_id.as_str())
                == Some(version_id.as_str())
            {
                return Err(format!(
                    "{version_id} is already the current version of {shown}"
                ));
            }
            let kept = versions_of(&auth, space, &file.node_id).await?;
            if !kept.iter().any(|kept| kept.version_id == version_id) {
                return Err(format!(
                    "{shown} has no version {version_id}; koi versions lists them"
                ));
            }
            RestoreRequest::Version {
                node_id: file.node_id.clone(),
                version_id,
            }
        }
        None => {
            let trashed = trash_nodes(&auth, space).await?;
            let deleted = trashed_at(&live, &trashed, space, &below, node.as_deref())
                .map_err(|why| why.message(&shown, &linked))?;
            RestoreRequest::FromTrash {
                node_id: deleted.node_id.clone(),
                parent_id: None,
                name: None,
            }
        }
    };
    let request = Request::Restore {
        space_id: linked.space_id.clone(),
        restore: request,
    };
    let id = match ipc::run(paths, request).await? {
        Response::Restore { id } => id,
        Response::Error { message } => return Err(message),
        other => return Err(format!("the daemon answered {other:?}")),
    };
    println!("Asked to restore {shown} (restore {id}).");
    if linked.state == FolderState::Stopped {
        println!("Run koi daemon to send it. koi restores shows how it went.");
    } else {
        println!(
            "The folder sends it with its next commit. koi restores {} shows how it went.",
            linked.path
        );
    }
    Ok(())
}

/// `koi restores [folder]`: the restores asked for in the folder's space, those that ended first.
pub async fn restores(paths: &Paths, folder: &Path, json: bool) -> Result<(), String> {
    let (linked, _) = locate(paths, folder).await?;
    let request = Request::Restores {
        space_id: linked.space_id.clone(),
    };
    let restores = match ipc::run(paths, request).await? {
        Response::Restores { restores } => restores,
        Response::Error { message } => return Err(message),
        other => return Err(format!("the daemon answered {other:?}")),
    };
    if json {
        return print_json(&restores);
    }
    print!("{}", restores_report(&restores, &linked));
    Ok(())
}

/// Why no single deletion could be picked.
#[derive(Debug, PartialEq)]
enum NotPicked<'a> {
    /// No deletion had the path. `live` says whether a live node has it.
    None { live: bool },
    /// Several did, and none was picked.
    Several(Vec<&'a NodeState>),
    /// None of the deletions at the path is the node asked for.
    NotThere(String),
}

impl NotPicked<'_> {
    fn message(&self, shown: &str, linked: &FolderStatus) -> String {
        match self {
            Self::None { live: true } => {
                format!("{shown} is not deleted; koi versions lists the versions of a file")
            }
            Self::None { live: false } => format!(
                "nothing deleted at {shown} is in the trash of {}; koi trash lists what is",
                linked.path
            ),
            Self::Several(found) => {
                let mut message = format!(
                    "{} deletions had the path {shown}; pick one with --node:",
                    found.len()
                );
                for node in found {
                    message.push_str(&format!(
                        "\n  {}  {}, {}",
                        node.node_id,
                        kind(node),
                        size(content_size(node.current.as_ref()))
                    ));
                }
                message
            }
            Self::NotThere(node) => {
                format!("no deletion at {shown} is node {node}; koi trash --json lists them")
            }
        }
    }
}

/// The trashed node that had the path `names`, or the one of them that is `node`. A node trashed with its folder
/// counts at its own path, so a file can be restored on its own.
fn trashed_at<'a>(
    live: &'a [NodeState],
    trash: &'a [NodeState],
    root: &str,
    names: &[String],
    node: Option<&str>,
) -> Result<&'a NodeState, NotPicked<'a>> {
    let by_id: HashMap<&str, &NodeState> = live
        .iter()
        .chain(trash)
        .map(|node| (node.node_id.as_str(), node))
        .collect();
    let mut found: Vec<&NodeState> = trash
        .iter()
        .filter(|trashed| {
            let had = names_of(trashed, &by_id, root);
            had.len() == names.len() && starts_with(&had, names)
        })
        .collect();
    if let Some(node) = node {
        found.retain(|trashed| trashed.node_id == node);
        return match found.as_slice() {
            [one] => Ok(one),
            _ => Err(NotPicked::NotThere(node.to_owned())),
        };
    }
    match found.as_slice() {
        [] => Err(NotPicked::None {
            live: find(live, root, names).is_some(),
        }),
        [one] => Ok(one),
        _ => {
            found.sort_by(|a, b| a.node_id.cmp(&b.node_id));
            Err(NotPicked::Several(found))
        }
    }
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
        "{} is not in a linked folder; koi status lists them",
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

fn restores_report(restores: &[RestoreStatus], linked: &FolderStatus) -> String {
    if restores.is_empty() {
        return format!("No restore was asked for in {}.\n", linked.path);
    }
    let mut report = String::new();
    for restore in restores {
        let what = match &restore.request {
            RestoreRequest::FromTrash { node_id, .. } => format!("deleted node {node_id}"),
            RestoreRequest::Version {
                node_id,
                version_id,
            } => format!("version {version_id} of node {node_id}"),
        };
        report.push_str(&format!(
            "{}  {what}\n  {}\n",
            restore.id,
            restore_state(&restore.state)
        ));
    }
    let pending = restores
        .iter()
        .any(|restore| matches!(restore.state, RestoreState::Pending { .. }));
    if pending {
        report.push_str(
            "A pending restore goes out with a commit of the folder once the earlier changes of its node have synced.\n",
        );
        if linked.state == FolderState::Stopped {
            report.push_str("The folder is not syncing: run koi daemon.\n");
        }
        if let Some(problem) = &linked.problem {
            report.push_str(&format!("The folder has a problem: {problem}\n"));
        }
        if linked.questions > 0 {
            report.push_str(&format!(
                "{} for the folder: {} lists them.\n",
                crate::count(linked.questions, "pending decision"),
                crate::RESOLVE
            ));
        }
    }
    report
}

fn restore_state(state: &RestoreState) -> String {
    match state {
        RestoreState::Pending { waits_for: None } => "pending: waits for the folder to sync".into(),
        RestoreState::Pending {
            waits_for: Some(RestoreWait::NodeProblem),
        } => "pending: the server refused a change this device made to the node, and nothing is sent for it until that problem is cleared"
            .into(),
        RestoreState::Restored => "restored".into(),
        RestoreState::Renamed { name } => {
            format!("restored as {}, since its name was taken", printable(name))
        }
        RestoreState::Copied { name, .. } => format!(
            "restored as a new file {}, since another device changed the file meanwhile",
            printable(name)
        ),
        RestoreState::Rejected { reason } => format!("refused: {}", rejection(reason)),
        RestoreState::DoesNotFit => {
            "not sent: it brings back more nodes than one commit can hold".into()
        }
    }
}

fn rejection(reason: &RestoreRejection) -> String {
    match reason {
        RestoreRejection::NotFound => {
            "the server no longer has it, or the folder to restore into; it was purged or expired"
                .into()
        }
        RestoreRejection::NotFile => "only a file has versions".into(),
        RestoreRejection::InvalidName => "its name is not valid".into(),
        RestoreRejection::ParentNotDir => "the place to restore into is a file".into(),
        RestoreRejection::Cycle => "the folder to restore into is inside it".into(),
        RestoreRejection::Root => "it is the root of the space".into(),
        RestoreRejection::ReadOnly => "the server takes no writes from this device".into(),
        RestoreRejection::Other(reason) => reason.clone(),
    }
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
    use koidrive_driver::Credential;
    use koidrive_proto::v1::{
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
            lexical(Path::new("/home/ada/./Koidrive/docs/../link.txt")),
            Path::new("/home/ada/Koidrive/link.txt")
        );
    }

    #[test]
    fn a_path_below_a_linked_folder_gives_its_names() {
        let root = Path::new("/home/ada/Koidrive");
        assert_eq!(
            below(root, Path::new("/home/ada/Koidrive/docs/a.txt")),
            Some(names("docs/a.txt"))
        );
        assert_eq!(below(root, root), Some(Vec::new()));
        assert_eq!(below(root, Path::new("/home/ada/Koidrive2/a.txt")), None);
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
            path: "/home/ada/Koidrive".into(),
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
            "Nothing from /home/ada/Koidrive is in the trash.\n"
        );
    }

    #[test]
    fn a_restore_picks_the_deletion_that_had_the_path() {
        let live = vec![
            node("docs", SPACE, "docs", NodeKind::Dir, 0, ""),
            node("note", "docs", "note.txt", NodeKind::File, 4, ""),
        ];
        let trash = vec![
            node("photos", SPACE, "photos", NodeKind::Dir, 0, "g1"),
            node("cat", "photos", "cat.jpg", NodeKind::File, 300, "g1"),
            node("old-b", "docs", "old.txt", NodeKind::File, 12, "g3"),
            node("old-a", "docs", "old.txt", NodeKind::File, 7, "g2"),
        ];
        let pick = |path: &str, node: Option<&str>| {
            trashed_at(&live, &trash, SPACE, &names(path), node)
                .map(|picked| picked.node_id.as_str())
                .map_err(|why| match why {
                    NotPicked::Several(found) => {
                        format!(
                            "several: {:?}",
                            found.iter().map(|node| &node.node_id).collect::<Vec<_>>()
                        )
                    }
                    other => format!("{other:?}"),
                })
        };

        assert_eq!(pick("photos", None), Ok("photos"));
        assert_eq!(
            pick("photos/cat.jpg", None),
            Ok("cat"),
            "a file trashed with its folder"
        );
        assert_eq!(
            pick("docs/old.txt", None),
            Err(r#"several: ["old-a", "old-b"]"#.into())
        );
        assert_eq!(pick("docs/old.txt", Some("old-b")), Ok("old-b"));
        assert_eq!(
            pick("docs/old.txt", Some("cat")),
            Err(r#"NotThere("cat")"#.into())
        );
        assert_eq!(
            pick("docs/note.txt", None),
            Err("None { live: true }".into())
        );
        assert_eq!(
            pick("docs/new.txt", None),
            Err("None { live: false }".into())
        );

        let several = trashed_at(&live, &trash, SPACE, &names("docs/old.txt"), None).unwrap_err();
        assert_eq!(
            several.message("docs/old.txt", &linked()),
            "2 deletions had the path docs/old.txt; pick one with --node:\n  old-a  file, 7 B\n  old-b  file, 12 B"
        );
    }

    fn linked() -> FolderStatus {
        FolderStatus {
            path: "/home/ada/Koidrive".into(),
            ..serde_json::from_value(serde_json::json!({
                "space_id": SPACE, "space_name": "Home", "path": "", "state": "up_to_date",
                "history_id": null, "last_seq": null, "head_seq": null, "remote_nodes": 0,
                "local_entries": null, "questions": 0
            }))
            .unwrap()
        }
    }

    #[test]
    fn the_restores_report_says_what_holds_a_pending_restore_back() {
        let restores = [
            RestoreStatus {
                id: "r1".into(),
                request: RestoreRequest::FromTrash {
                    node_id: "cat".into(),
                    parent_id: None,
                    name: None,
                },
                state: RestoreState::Renamed {
                    name: "cat (2).jpg".into(),
                },
            },
            RestoreStatus {
                id: "r2".into(),
                request: RestoreRequest::Version {
                    node_id: "note".into(),
                    version_id: "note-0".into(),
                },
                state: RestoreState::Pending { waits_for: None },
            },
        ];
        let folder = FolderStatus {
            state: FolderState::Stopped,
            questions: 1,
            problem: Some("the root is missing".into()),
            ..linked()
        };

        assert_eq!(
            restores_report(&restores, &folder),
            concat!(
                "r1  deleted node cat\n",
                "  restored as cat (2).jpg, since its name was taken\n",
                "r2  version note-0 of node note\n",
                "  pending: waits for the folder to sync\n",
                "A pending restore goes out with a commit of the folder once the earlier changes of its node have synced.\n",
                "The folder is not syncing: run koi daemon.\n",
                "The folder has a problem: the root is missing\n",
                "1 pending decision for the folder: koi resolve lists them.\n",
            )
        );
        let two = FolderStatus {
            questions: 2,
            ..linked()
        };
        assert!(
            restores_report(&restores, &two)
                .ends_with("2 pending decisions for the folder: koi resolve lists them.\n")
        );
        assert_eq!(
            restores_report(&restores[..1], &folder),
            "r1  deleted node cat\n  restored as cat (2).jpg, since its name was taken\n"
        );
        assert_eq!(
            restores_report(&[], &folder),
            "No restore was asked for in /home/ada/Koidrive.\n"
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
