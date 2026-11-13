// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Component, Path, PathBuf};

use koidrive_engine::names::name_key;
use koidrive_net::{ApiError, Auth, ServerError};
use koidrive_proto::v1::{
    CreateShareLinkRequest, ListTree, NodeKind, NodeState, NodeStatus, ShareLink, SpaceInfo,
    rpc_request::Call, rpc_response,
};
use koidrive_store::Store;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::driver::{now_ms, store_error};
use crate::{Driver, DriverError, FolderStatus};

const TREE_PAGE: u32 = 1000;
const SHORT_ID_DIGITS: usize = 10;
const LINK_ID_CHARS: usize = 22;
const PASSWORD_CHARS: std::ops::RangeInclusive<usize> = 8..=256;

/// A value that grants access, such as a link's URL or password. `Debug` shows `Secret(..)`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(pub String);

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// How long a new link works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Expiry {
    /// The server's default: 7 days after the link is made.
    #[default]
    Default,
    /// This many milliseconds after the request.
    After {
        ms: u64,
    },
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    Active,
    Expired,
    Revoked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    File,
    Folder,
}

/// The file or folder a link opens, where the space holds it now.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    /// The names from the space's root down to it, joined with `/`; empty for the root.
    pub path: String,
    pub kind: TargetKind,
}

/// A share link as its owner sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedLink {
    /// Names the link without revealing it: the first 10 hex digits of the SHA-256 of its secret.
    pub id: String,
    pub url: Secret,
    pub space_id: String,
    pub space_name: String,
    /// The folder that syncs the space on this device, if one does.
    pub folder: Option<String>,
    pub node_id: String,
    /// None once the node or a folder above it is in the trash or gone, which leaves the link opening nothing.
    pub target: Option<Target>,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub revoked_at_ms: Option<i64>,
    pub has_password: bool,
    pub state: LinkState,
}

/// A link after [`Driver::revoke_share_link`]; `already` when it was revoked before.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Revoked {
    pub link: SharedLink,
    pub already: bool,
}

impl Driver {
    /// Makes a link to the file or folder at the absolute `path`, which a linked folder holds and the space has.
    pub async fn share(
        &self,
        path: &Path,
        expiry: Expiry,
        password: Option<Secret>,
    ) -> Result<SharedLink, DriverError> {
        if let Some(password) = &password
            && !PASSWORD_CHARS.contains(&password.0.chars().count())
        {
            return Err(DriverError::PasswordLength);
        }
        let folders = self.status().folders;
        let (folder, names) = locate(&folders, path)?;
        let nodes = self.remote_nodes(&folder.space_id)?;
        let tree = Tree::new(&folder.space_id, &nodes);
        let Some(node_id) = tree.find(&names) else {
            return Err(match std::fs::symlink_metadata(path) {
                Ok(_) => DriverError::NotOnServer(path.to_owned()),
                Err(_) => DriverError::PathNotFound(path.to_owned()),
            });
        };
        let auth = self.signed_in()?;
        let now = now_ms();
        let request = CreateShareLinkRequest {
            node_id: node_id.to_owned(),
            expires_at_ms: match expiry {
                Expiry::After { ms } => now.saturating_add_unsigned(ms),
                Expiry::Default | Expiry::Never => 0,
            },
            no_expiry: expiry == Expiry::Never,
            password: password.map(|password| password.0).unwrap_or_default(),
        };
        let link = (auth.api())
            .create_share_link(&auth, &folder.space_id, &request)
            .await?;
        Ok(described(
            link,
            &folder.space_name,
            Some(folder),
            &tree,
            now,
        ))
    }

    /// Every link of the spaces this device's user owns, ended ones included, newest first.
    pub async fn share_links(&self) -> Result<Vec<SharedLink>, DriverError> {
        let auth = self.signed_in()?;
        let folders = self.status().folders;
        let mut found = Vec::new();
        for (space, links) in links_by_space(&auth).await? {
            found.extend(self.describe(&auth, &folders, &space, links).await?);
        }
        found.sort_by_key(|link| std::cmp::Reverse(link.created_at_ms));
        Ok(found)
    }

    /// Ends the link that `link` names: its [`SharedLink::id`], its URL, or the secret in its URL. Revoking a link
    /// that is already revoked changes nothing.
    pub async fn revoke_share_link(&self, link: &Secret) -> Result<Revoked, DriverError> {
        let wanted = LinkRef::parse(&link.0).ok_or(DriverError::NotAShareLink)?;
        let auth = self.signed_in()?;
        let mut matched: Vec<(SpaceInfo, ShareLink)> = Vec::new();
        for (space, links) in links_by_space(&auth).await? {
            for link in links.into_iter().filter(|link| wanted.matches(link)) {
                matched.push((space.clone(), link));
            }
        }
        let (space, mut link) = match matched.len() {
            0 => return Err(DriverError::ShareLinkNotFound),
            1 => matched.remove(0),
            _ => {
                return Err(DriverError::ShareLinkAmbiguous(short_id(
                    &matched[0].1.link_id,
                )));
            }
        };
        let already = link.revoked_at_ms != 0;
        if !already {
            (auth.api())
                .revoke_share_link(&auth, &space.space_id, &link.link_id)
                .await?;
            link.revoked_at_ms = now_ms();
        }
        let folders = self.status().folders;
        let mut described = self.describe(&auth, &folders, &space, vec![link]).await?;
        Ok(Revoked {
            link: described.remove(0),
            already,
        })
    }

    async fn describe(
        &self,
        auth: &Auth,
        folders: &[FolderStatus],
        space: &SpaceInfo,
        links: Vec<ShareLink>,
    ) -> Result<Vec<SharedLink>, DriverError> {
        let folder = (folders.iter()).find(|folder| folder.space_id == space.space_id);
        let mut nodes = match folder {
            Some(_) => self.remote_nodes(&space.space_id)?,
            None => Vec::new(),
        };
        let missing = {
            let tree = Tree::new(&space.space_id, &nodes);
            (links.iter()).any(|link| tree.target(&link.node_id).is_none())
        };
        if missing {
            nodes = server_tree(auth, &space.space_id).await?;
        }
        let tree = Tree::new(&space.space_id, &nodes);
        let now = now_ms();
        Ok(links
            .into_iter()
            .map(|link| described(link, &space.name, folder, &tree, now))
            .collect())
    }

    fn remote_nodes(&self, space_id: &str) -> Result<Vec<NodeState>, DriverError> {
        let path = self.paths().store(space_id)?;
        Store::open(&path)
            .and_then(|mut store| store.transaction(|tx| tx.remote_nodes()))
            .map_err(store_error(&path))
    }
}

enum LinkRef {
    Short(String),
    Secret(String),
}

impl LinkRef {
    fn parse(text: &str) -> Option<LinkRef> {
        let text = text.trim();
        let secret = match text.rsplit_once("/s/") {
            Some((_, rest)) => rest.split(['/', '?', '#']).next().unwrap_or_default(),
            None => text,
        };
        let base64url = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
        if secret.len() == LINK_ID_CHARS && secret.chars().all(base64url) {
            return Some(LinkRef::Secret(secret.to_owned()));
        }
        let short = text.to_ascii_lowercase();
        (short.len() == SHORT_ID_DIGITS && short.chars().all(|c| c.is_ascii_hexdigit()))
            .then_some(LinkRef::Short(short))
    }

    fn matches(&self, link: &ShareLink) -> bool {
        match self {
            LinkRef::Short(id) => short_id(&link.link_id) == *id,
            LinkRef::Secret(secret) => link.link_id == *secret,
        }
    }
}

fn short_id(link_id: &str) -> String {
    let digest = Sha256::digest(link_id.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    hex[..SHORT_ID_DIGITS].to_owned()
}

fn described(
    link: ShareLink,
    space_name: &str,
    folder: Option<&FolderStatus>,
    tree: &Tree,
    now: i64,
) -> SharedLink {
    let expires_at_ms = (link.expires_at_ms != 0).then_some(link.expires_at_ms);
    let revoked_at_ms = (link.revoked_at_ms != 0).then_some(link.revoked_at_ms);
    let state = match (revoked_at_ms, expires_at_ms) {
        (Some(_), _) => LinkState::Revoked,
        (None, Some(expires)) if expires <= now => LinkState::Expired,
        (None, _) => LinkState::Active,
    };
    SharedLink {
        id: short_id(&link.link_id),
        target: tree.target(&link.node_id),
        url: Secret(link.url),
        space_id: link.space_id,
        space_name: space_name.to_owned(),
        folder: folder.map(|folder| folder.path.clone()),
        node_id: link.node_id,
        created_at_ms: link.created_at_ms,
        expires_at_ms,
        revoked_at_ms,
        has_password: link.has_password,
        state,
    }
}

struct Tree<'a> {
    root: &'a str,
    by_id: HashMap<&'a str, &'a NodeState>,
    by_name: HashMap<(&'a str, String), &'a str>,
}

impl<'a> Tree<'a> {
    fn new(root: &'a str, nodes: &'a [NodeState]) -> Self {
        let live: Vec<&NodeState> = (nodes.iter())
            .filter(|node| node.status() == NodeStatus::Live && node.node_id != root)
            .collect();
        Tree {
            root,
            by_id: (live.iter())
                .map(|node| (node.node_id.as_str(), *node))
                .collect(),
            by_name: (live.iter())
                .map(|node| {
                    let key = (node.parent_id.as_str(), name_key(&node.name));
                    (key, node.node_id.as_str())
                })
                .collect(),
        }
    }

    fn find(&self, names: &[String]) -> Option<&'a str> {
        (names.iter()).try_fold(self.root, |parent, name| {
            self.by_name.get(&(parent, name_key(name))).copied()
        })
    }

    fn target(&self, node_id: &str) -> Option<Target> {
        if node_id == self.root {
            return Some(Target {
                path: String::new(),
                kind: TargetKind::Folder,
            });
        }
        let node = self.by_id.get(node_id)?;
        let mut names = vec![node.name.as_str()];
        let mut parent = node.parent_id.as_str();
        while parent != self.root {
            let above = self.by_id.get(parent)?;
            names.push(above.name.as_str());
            parent = above.parent_id.as_str();
            if names.len() > self.by_id.len() {
                return None;
            }
        }
        names.reverse();
        Some(Target {
            path: names.join("/"),
            kind: match node.kind() {
                NodeKind::Dir => TargetKind::Folder,
                _ => TargetKind::File,
            },
        })
    }
}

fn locate<'a>(
    folders: &'a [FolderStatus],
    path: &Path,
) -> Result<(&'a FolderStatus, Vec<String>), DriverError> {
    if !path.is_absolute() {
        return Err(DriverError::RelativePath(path.to_owned()));
    }
    let typed = lexical(path);
    let components: Vec<Component> = typed.components().collect();
    let mut prefix = PathBuf::new();
    for (index, component) in components.iter().enumerate() {
        prefix.push(component);
        let Ok(real) = std::fs::canonicalize(&prefix) else {
            break;
        };
        let inside = (folders.iter())
            .filter_map(|folder| Some((folder, below(Path::new(&folder.path), &real)?)))
            .max_by_key(|(folder, _)| folder.path.len());
        if let Some((folder, mut names)) = inside {
            names.extend(
                components[index + 1..]
                    .iter()
                    .map(|name| name.as_os_str().to_string_lossy().into_owned()),
            );
            return Ok((folder, names));
        }
    }
    Err(DriverError::OutsideLinkedFolders(typed))
}

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

async fn links_by_space(auth: &Auth) -> Result<Vec<(SpaceInfo, Vec<ShareLink>)>, DriverError> {
    let mut found = Vec::new();
    for space in auth.api().list_spaces(auth).await?.spaces {
        let mut links = Vec::new();
        let mut cursor = String::new();
        let mut seen = HashSet::new();
        loop {
            let api = auth.api();
            let page = match api.list_share_links(auth, &space.space_id, &cursor).await {
                Ok(page) => page,
                Err(ApiError::Server { status: 403, .. }) => break,
                Err(error) => return Err(error.into()),
            };
            links.extend(page.links);
            if page.next_cursor.is_empty() {
                break;
            }
            if !seen.insert(page.next_cursor.clone()) {
                return Err(ApiError::Decode("ShareLinkList").into());
            }
            cursor = page.next_cursor;
        }
        if !links.is_empty() {
            found.push((space, links));
        }
    }
    Ok(found)
}

async fn server_tree(auth: &Auth, space_id: &str) -> Result<Vec<NodeState>, DriverError> {
    let mut nodes = Vec::new();
    let mut cursor = String::new();
    let mut seen = HashSet::new();
    loop {
        let call = Call::ListTree(ListTree {
            cursor,
            limit: TREE_PAGE,
        });
        let page = match auth.api().rpc(auth, space_id, call).await?.result {
            Some(rpc_response::Result::Tree(page)) => page,
            Some(rpc_response::Result::Error(error)) => {
                return Err(DriverError::Refused(ServerError(error)));
            }
            _ => return Err(ApiError::Decode("TreePage").into()),
        };
        nodes.extend(page.nodes);
        if page.next_cursor.is_empty() {
            return Ok(nodes);
        }
        if !seen.insert(page.next_cursor.clone()) {
            return Err(ApiError::Decode("TreePage").into());
        }
        cursor = page.next_cursor;
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::{FolderState, Paths};

    const SPACE: &str = "space-1";

    fn node(id: &str, parent: &str, name: &str, kind: NodeKind, status: NodeStatus) -> NodeState {
        NodeState {
            node_id: id.into(),
            parent_id: parent.into(),
            name: name.into(),
            kind: kind as i32,
            status: status as i32,
            ..NodeState::default()
        }
    }

    fn link(link_id: &str, node_id: &str) -> ShareLink {
        ShareLink {
            link_id: link_id.into(),
            url: format!("https://files.example.com/s/{link_id}"),
            space_id: SPACE.into(),
            node_id: node_id.into(),
            created_at_ms: 1_000,
            ..ShareLink::default()
        }
    }

    fn folder(path: &Path) -> FolderStatus {
        FolderStatus {
            space_id: SPACE.into(),
            space_name: "Team".into(),
            path: path.to_str().unwrap().into(),
            state: FolderState::UpToDate,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: None,
            wait: None,
        }
    }

    fn names(path: &str) -> Vec<String> {
        path.split('/').map(str::to_owned).collect()
    }

    #[test]
    fn a_link_is_named_by_its_short_id_its_secret_or_its_url() {
        let secret = "AAAAAAAAAAAAAAAAAAAAAA";
        let other = link("BBBBBBBBBBBBBBBBBBBBBQ", "n");
        let shared = link(secret, "n");
        let id = short_id(secret);
        let named = [
            id.clone(),
            id.to_ascii_uppercase(),
            secret.to_owned(),
            format!("https://files.example.com/s/{secret}"),
            format!(" https://files.example.com/prefix/s/{secret}/?x#y\n"),
        ];
        for text in named {
            let wanted = LinkRef::parse(&text).unwrap();
            assert!(wanted.matches(&shared), "{text:?}");
            assert!(!wanted.matches(&other), "{text:?}");
        }
        assert_eq!(id.len(), 10);
    }

    #[test]
    fn text_that_names_no_link_is_refused() {
        for text in [
            "",
            "abc",
            "0123456789a",
            "012345678g",
            "AAAAAAAAAAAAAAAAAAAAAAA",
            "AAAAAAAAAAAAAAAAAAAAA=",
            "https://files.example.com/AAAAAAAAAAAAAAAAAAAAAA",
        ] {
            assert!(LinkRef::parse(text).is_none(), "{text:?}");
        }
    }

    #[test]
    fn a_path_finds_its_live_node_by_name_key_and_nothing_below_a_trashed_folder() {
        let nodes = vec![
            node("docs", SPACE, "Docs", NodeKind::Dir, NodeStatus::Live),
            node("cafe", "docs", "Café.txt", NodeKind::File, NodeStatus::Live),
            node("old", SPACE, "Old", NodeKind::Dir, NodeStatus::Trashed),
            node("kept", "old", "kept.txt", NodeKind::File, NodeStatus::Live),
            node(
                "gone",
                "docs",
                "gone.txt",
                NodeKind::File,
                NodeStatus::Trashed,
            ),
        ];
        let tree = Tree::new(SPACE, &nodes);

        assert_eq!(tree.find(&names("docs/CAFE\u{301}.TXT")), Some("cafe"));
        assert_eq!(tree.find(&[]), Some(SPACE));
        assert_eq!(tree.find(&names("Old/kept.txt")), None);
        assert_eq!(tree.find(&names("Docs/gone.txt")), None);
        assert_eq!(
            tree.target("cafe"),
            Some(Target {
                path: "Docs/Café.txt".into(),
                kind: TargetKind::File
            })
        );
        assert_eq!(
            tree.target(SPACE),
            Some(Target {
                path: String::new(),
                kind: TargetKind::Folder
            })
        );
        assert_eq!(tree.target("kept"), None);
        assert_eq!(tree.target("gone"), None);
        assert_eq!(tree.target("unknown"), None);
    }

    #[test]
    fn a_cycle_of_parents_reaches_no_target() {
        let nodes = vec![
            node("a", "b", "a", NodeKind::Dir, NodeStatus::Live),
            node("b", "a", "b", NodeKind::Dir, NodeStatus::Live),
        ];
        assert_eq!(Tree::new(SPACE, &nodes).target("a"), None);
    }

    #[test]
    fn a_link_is_revoked_expired_or_active_by_its_record_and_the_clock() {
        let nodes = vec![node("f", SPACE, "f.txt", NodeKind::File, NodeStatus::Live)];
        let tree = Tree::new(SPACE, &nodes);
        let state = |expires_at_ms: i64, revoked_at_ms: i64| {
            let link = ShareLink {
                expires_at_ms,
                revoked_at_ms,
                ..link("AAAAAAAAAAAAAAAAAAAAAA", "f")
            };
            described(link, "Team", None, &tree, 5_000).state
        };
        assert_eq!(state(0, 0), LinkState::Active);
        assert_eq!(state(5_001, 0), LinkState::Active);
        assert_eq!(state(5_000, 0), LinkState::Expired);
        assert_eq!(state(0, 4_000), LinkState::Revoked);
        assert_eq!(state(1_000, 4_000), LinkState::Revoked);
    }

    #[test]
    fn a_described_link_keeps_its_secret_out_of_debug_output() {
        let secret = "AAAAAAAAAAAAAAAAAAAAAA";
        let nodes = vec![node("f", SPACE, "f.txt", NodeKind::File, NodeStatus::Live)];
        let shared = described(
            link(secret, "f"),
            "Team",
            None,
            &Tree::new(SPACE, &nodes),
            0,
        );
        assert_eq!(
            shared.url.0,
            format!("https://files.example.com/s/{secret}")
        );
        assert!(!format!("{shared:?}").contains(secret));
        assert!(!format!("{:?}", Secret("hunter22".into())).contains("hunter22"));
    }

    #[test]
    fn a_path_is_located_below_its_linked_folder_without_following_links_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let linked = root.join("Sync");
        let outside = root.join("Elsewhere");
        std::fs::create_dir_all(linked.join("Docs")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, linked.join("Docs/away")).unwrap();
        symlink(linked.join("Docs"), root.join("shortcut")).unwrap();
        let folders = [folder(&linked)];

        let located = |path: &Path| locate(&folders, path).map(|(_, names)| names);
        assert_eq!(located(&linked).unwrap(), Vec::<String>::new());
        assert_eq!(
            located(&linked.join("Docs/away/x.txt")).unwrap(),
            names("Docs/away/x.txt")
        );
        assert_eq!(
            located(&root.join("shortcut/new.txt")).unwrap(),
            names("Docs/new.txt")
        );
        assert_eq!(
            located(&linked.join("Docs/../Docs/./a.txt")).unwrap(),
            names("Docs/a.txt")
        );
        assert!(matches!(
            located(&outside.join("x.txt")),
            Err(DriverError::OutsideLinkedFolders(_))
        ));
        assert!(matches!(
            located(Path::new("Sync/x.txt")),
            Err(DriverError::RelativePath(_))
        ));
    }

    #[tokio::test]
    async fn a_password_outside_8_to_256_characters_is_refused_before_anything_is_sent() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        for password in ["", "seven77", &"x".repeat(257)] {
            let outcome = driver
                .share(dir.path(), Expiry::Default, Some(Secret(password.into())))
                .await;
            assert!(
                matches!(outcome, Err(DriverError::PasswordLength)),
                "{password:?}"
            );
        }
        let outcome = driver
            .share(dir.path(), Expiry::Default, Some(Secret("é".repeat(8))))
            .await;
        assert!(
            matches!(outcome, Err(DriverError::OutsideLinkedFolders(_))),
            "{outcome:?}"
        );
    }
}
