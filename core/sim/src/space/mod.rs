// SPDX-License-Identifier: Apache-2.0

//! An in-memory Space DO written from spec v0 sections 6 to 8 and checked against the TypeScript
//! server (flarebox#6): commit idempotency, `base_seq` checks, chunk presence and receipts, rules
//! R1 to R11, trash groups, revisions and post-image change records, resume and the paged listings.
//! Garbage collection and retention are not modelled.

mod apply;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use flarebox_engine::ids::{IdGen, is_uuid_v7};
use flarebox_proto::v1::{
    ChangeRecord, ChunkPlan, ChunkRef, ChunkTarget, Commit, CommitRecord, CommitResult, Content,
    DownloadTargets, Error, GetDownloadTargets, GetVersionChunks, ListTrash, ListTree,
    ListVersions, NodeKind, NodeState, NodeStatus, Op, PrepareUpload, PrepareUploadResult, Resume,
    ResyncRequired, RpcRequest, RpcResponse, Target, TrashPage, TreePage, UploadReceipt,
    VersionChunks, VersionInfo, VersionList, Welcome, chunk_plan, error, op, resync_required,
    rpc_request, rpc_response,
};
use prost::Message;
use sha2::{Digest, Sha256};

use apply::{Applier, Context};

pub use apply::MAX_DEPTH;

pub const FORMAT_VERSION: u32 = 1;
pub const MAX_OPS: usize = 1000;
pub const MAX_CHUNKS_PER_CALL: usize = 1000;
pub const MAX_CHUNK_BYTES: u32 = 16 * 1024 * 1024;
pub const INLINE_CHUNKS: u32 = 16_384;
pub const PAGE_CHUNKS: usize = 4096;
const GRANT_MS: i64 = 3_600_000;
const PAGE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Editor,
    Viewer,
}

/// Who is calling, as the Worker and the WebSocket `Hello` establish it.
#[derive(Clone, Debug)]
pub struct Session {
    pub device_id: String,
    pub user_id: String,
    pub device_name: String,
    pub role: Role,
    pub max_format: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChunkState {
    Pending,
    Live,
}

#[derive(Clone, Debug)]
pub(crate) struct ChunkRow {
    size: u32,
    state: ChunkState,
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    node_id: String,
    parent_id: Option<String>,
    name: String,
    name_key: String,
    kind: NodeKind,
    status: NodeStatus,
    content_rev: u64,
    content_dev: String,
    meta_rev: u64,
    meta_dev: String,
    cur_version_id: Option<String>,
    trash_group: Option<String>,
    /// Insertion order, standing in for SQLite's rowid where the server reads rows without ORDER BY.
    row: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct Version {
    version_id: String,
    node_id: String,
    seq: u64,
    /// Every chunk inline, with `chunk_count` set.
    content: Content,
    device_id: String,
    user_id: String,
    created_at_ms: i64,
}

/// Everything one commit can change, cloned per commit so that a failing commit leaves no trace.
#[derive(Clone, Debug)]
pub(crate) struct State {
    nodes: BTreeMap<String, Node>,
    versions: BTreeMap<String, Version>,
    chunks: BTreeMap<Vec<u8>, ChunkRow>,
    rows: u64,
    ids: IdGen,
}

impl State {
    fn next_row(&mut self) -> u64 {
        self.rows += 1;
        self.rows
    }

    fn children(&self, parent_id: &str, status: NodeStatus) -> Vec<Node> {
        let mut children: Vec<Node> = self
            .nodes
            .values()
            .filter(|n| n.parent_id.as_deref() == Some(parent_id) && n.status == status)
            .cloned()
            .collect();
        children.sort_by_key(|n| n.row);
        children
    }

    fn version_info(&self, version: &Version) -> VersionInfo {
        VersionInfo {
            version_id: version.version_id.clone(),
            seq: version.seq,
            content: Some(inline_content(&version.content)),
            device_id: version.device_id.clone(),
            user_id: version.user_id.clone(),
            created_at_ms: version.created_at_ms,
        }
    }

    fn node_state(&self, node: &Node) -> NodeState {
        NodeState {
            node_id: node.node_id.clone(),
            parent_id: node.parent_id.clone().unwrap_or_default(),
            name: node.name.clone(),
            kind: node.kind as i32,
            status: node.status as i32,
            content_rev: node.content_rev,
            meta_rev: node.meta_rev,
            trash_group: node.trash_group.clone().unwrap_or_default(),
            current: node
                .cur_version_id
                .as_ref()
                .map(|id| self.version_info(&self.versions[id])),
        }
    }
}

/// A version's content as records carry it: chunks inline up to 16,384, `chunk_count` always.
fn inline_content(content: &Content) -> Content {
    let chunks = if content.chunk_count <= INLINE_CHUNKS {
        content.chunks.clone()
    } else {
        Vec::new()
    };
    Content {
        chunks,
        ..content.clone()
    }
}

pub(crate) fn fail(code: error::Code, message: impl Into<String>) -> Error {
    Error {
        code: code as i32,
        message: message.into(),
        ..Default::default()
    }
}

/// `(32-byte hash || u32 LE size)` for every chunk, the bytes `manifest_digest` hashes (spec v0 section 5.1).
fn encode_chunk_list(chunks: &[ChunkRef]) -> Vec<u8> {
    chunks
        .iter()
        .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
        .collect()
}

fn check_chunk_ref(chunk: &ChunkRef) -> Result<(), Error> {
    if chunk.hash.len() != 32 {
        return Err(fail(
            error::Code::InvalidArgument,
            "chunk hash must be 32 bytes",
        ));
    }
    if chunk.size < 1 || chunk.size > MAX_CHUNK_BYTES {
        return Err(fail(
            error::Code::InvalidArgument,
            "chunk size out of range",
        ));
    }
    if chunk.encoding != 0 {
        return Err(fail(
            error::Code::InvalidArgument,
            "unsupported chunk encoding",
        ));
    }
    Ok(())
}

fn check_content(content: Option<&Content>) -> Result<&Content, Error> {
    let content =
        content.ok_or_else(|| fail(error::Code::InvalidArgument, "content is required"))?;
    if content.chunks.len() != content.chunk_count as usize {
        return Err(fail(
            error::Code::InvalidArgument,
            "commits must list every chunk inline",
        ));
    }
    content.chunks.iter().try_for_each(check_chunk_ref)?;
    if content
        .chunks
        .iter()
        .map(|c| u64::from(c.size))
        .sum::<u64>()
        != content.size
    {
        return Err(fail(
            error::Code::InvalidArgument,
            "content size does not match its chunks",
        ));
    }
    if Sha256::digest(encode_chunk_list(&content.chunks)).as_slice()
        != content.manifest_digest.as_slice()
    {
        return Err(fail(
            error::Code::InvalidArgument,
            "manifest_digest does not match the chunk list",
        ));
    }
    Ok(content)
}

fn check_ids(op: &Op, index: usize) -> Result<(), Error> {
    let (required, optional): (Vec<&str>, Vec<&str>) = match &op.op {
        Some(op::Op::CreateDir(o)) => (vec![&o.node_id, &o.parent_id], vec![]),
        Some(op::Op::CreateFile(o)) => (vec![&o.node_id, &o.parent_id], vec![]),
        Some(op::Op::UpdateContent(o)) => (vec![&o.node_id], vec![]),
        Some(op::Op::Delete(o)) => (vec![&o.node_id], vec![]),
        Some(op::Op::Move(o)) => (vec![&o.node_id, &o.new_parent_id], vec![]),
        Some(op::Op::Restore(o)) => (vec![&o.node_id], vec![&o.version_id, &o.target_parent_id]),
        None => {
            return Err(fail(
                error::Code::InvalidArgument,
                format!("op {index} is empty"),
            ));
        }
    };
    if !required.iter().all(|id| is_uuid_v7(id))
        || !optional.iter().all(|id| id.is_empty() || is_uuid_v7(id))
    {
        return Err(fail(
            error::Code::InvalidArgument,
            format!("op {index} has a malformed id"),
        ));
    }
    Ok(())
}

fn op_content(op: &Op) -> Option<Option<&Content>> {
    match &op.op {
        Some(op::Op::CreateFile(o)) => Some(o.content.as_ref()),
        Some(op::Op::UpdateContent(o)) => Some(o.content.as_ref()),
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The token the reference relay returns for an upload. The real one is an HMAC; here it only has to
/// be unforgeable by a test that does not call [`SpaceDo::put_chunk`].
fn receipt_token(hash: &[u8], size: u32) -> String {
    format!("receipt:{}:{size}", hex(hash))
}

/// The outcome of [`SpaceDo::commit`]: the result for the committing device and, for a commit that was
/// applied now rather than found by its `commit_id`, the record to push to the other connections.
#[derive(Clone, Debug, PartialEq)]
pub struct Committed {
    pub result: CommitResult,
    pub record: Option<CommitRecord>,
}

pub struct SpaceDo {
    space_id: String,
    history_id: String,
    format_version: u32,
    state: State,
    log: Vec<CommitRecord>,
    results: HashMap<String, CommitResult>,
    head_seq: u64,
    log_floor_seq: u64,
}

impl SpaceDo {
    /// A new space whose IDs, including the space and history IDs, come from `seed`.
    pub fn new(seed: u64, now_ms: i64) -> Self {
        let mut ids = IdGen::new(seed);
        let space_id = ids.uuid_v7(now_ms as u64);
        let history_id = ids.uuid_v7(now_ms as u64);
        let root = Node {
            node_id: space_id.clone(),
            parent_id: None,
            name: String::new(),
            name_key: String::new(),
            kind: NodeKind::Dir,
            status: NodeStatus::Live,
            content_rev: 0,
            content_dev: String::new(),
            meta_rev: 0,
            meta_dev: String::new(),
            cur_version_id: None,
            trash_group: None,
            row: 1,
        };
        Self {
            history_id,
            format_version: FORMAT_VERSION,
            state: State {
                nodes: BTreeMap::from([(space_id.clone(), root)]),
                versions: BTreeMap::new(),
                chunks: BTreeMap::new(),
                rows: 1,
                ids,
            },
            space_id,
            log: Vec::new(),
            results: HashMap::new(),
            head_seq: 0,
            log_floor_seq: 0,
        }
    }

    /// Also the root node's ID.
    pub fn space_id(&self) -> &str {
        &self.space_id
    }

    pub fn history_id(&self) -> &str {
        &self.history_id
    }

    pub fn head_seq(&self) -> u64 {
        self.head_seq
    }

    pub fn log_floor_seq(&self) -> u64 {
        self.log_floor_seq
    }

    /// Every node, live and trashed, as a post-image.
    pub fn nodes(&self) -> Vec<NodeState> {
        self.state
            .nodes
            .values()
            .map(|n| self.state.node_state(n))
            .collect()
    }

    pub fn node(&self, node_id: &str) -> Option<NodeState> {
        self.state
            .nodes
            .get(node_id)
            .map(|n| self.state.node_state(n))
    }

    pub fn welcome(&self, session: &Session) -> Welcome {
        Welcome {
            proto: 0,
            server_version: "flarebox-sim".into(),
            format_version: self.format_version,
            history_id: self.history_id.clone(),
            head_seq: self.head_seq,
            log_floor_seq: self.log_floor_seq,
            read_only: session.role == Role::Viewer || session.max_format < self.format_version,
            ..Default::default()
        }
    }

    /// The backlog after `since_seq` (spec v0 section 7.3), or why the client must bootstrap instead.
    pub fn resume(&self, resume: &Resume) -> Result<Vec<CommitRecord>, ResyncRequired> {
        let reason = if resume.history_id != self.history_id {
            Some(resync_required::Reason::HistoryChanged)
        } else if resume.since_seq > self.head_seq {
            Some(resync_required::Reason::AheadOfServer)
        } else if resume.since_seq < self.log_floor_seq {
            Some(resync_required::Reason::LogTruncated)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(ResyncRequired {
                reason: reason as i32,
            });
        }
        Ok(self
            .log
            .iter()
            .filter(|r| r.seq > resume.since_seq)
            .cloned()
            .collect())
    }

    /// Forgets the log up to and including `floor_seq`, so that older resumes need a bootstrap.
    pub fn truncate_log(&mut self, floor_seq: u64) {
        self.log_floor_seq = floor_seq.min(self.head_seq).max(self.log_floor_seq);
        let floor = self.log_floor_seq;
        self.log.retain(|r| r.seq > floor);
    }

    /// Answers one RPC the way the server's `call` does. A new commit also returns its record.
    pub fn rpc(
        &mut self,
        session: &Session,
        now_ms: i64,
        request: &RpcRequest,
    ) -> (RpcResponse, Option<CommitRecord>) {
        use rpc_request::Call;
        use rpc_response::Result as R;
        let (result, record) = match &request.call {
            Some(Call::Commit(c)) => match self.commit(session, now_ms, c) {
                Ok(done) => (Ok(R::Commit(done.result)), done.record),
                Err(e) => (Err(e), None),
            },
            Some(Call::PrepareUpload(c)) => (
                self.prepare_upload(session, now_ms, c)
                    .map(R::PrepareUpload),
                None,
            ),
            Some(Call::GetDownloadTargets(c)) => (
                self.download_targets(now_ms, c).map(R::DownloadTargets),
                None,
            ),
            Some(Call::GetVersionChunks(c)) => (self.version_chunks(c).map(R::VersionChunks), None),
            Some(Call::ListVersions(c)) => (Ok(R::Versions(self.list_versions(c))), None),
            Some(Call::ListTree(c)) => (Ok(R::Tree(self.list_tree(c))), None),
            Some(Call::ListTrash(c)) => (Ok(R::Trash(self.list_trash(c))), None),
            None => (
                Err(fail(error::Code::InvalidArgument, "empty request")),
                None,
            ),
        };
        let result = Some(result.unwrap_or_else(R::Error));
        (
            RpcResponse {
                req_id: request.req_id,
                result,
            },
            record,
        )
    }

    fn check_can_write(&self, session: &Session) -> Result<(), Error> {
        if session.role == Role::Viewer {
            return Err(fail(
                error::Code::Forbidden,
                "viewers cannot change this space",
            ));
        }
        if session.max_format < self.format_version {
            return Err(fail(
                error::Code::ReadOnly,
                "this client is older than the space format",
            ));
        }
        Ok(())
    }

    pub fn commit(
        &mut self,
        session: &Session,
        now_ms: i64,
        commit: &Commit,
    ) -> Result<Committed, Error> {
        self.check_can_write(session)?;
        if !is_uuid_v7(&commit.commit_id) {
            return Err(fail(
                error::Code::InvalidArgument,
                "commit_id must be a UUIDv7",
            ));
        }
        if commit.ops.is_empty() {
            return Err(fail(
                error::Code::InvalidArgument,
                "a commit needs at least one op",
            ));
        }
        if commit.ops.len() > MAX_OPS {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_OPS} ops per commit"),
            ));
        }
        if let Some(stored) = self.results.get(&commit.commit_id) {
            return Ok(Committed {
                result: stored.clone(),
                record: None,
            });
        }

        let mut referenced: Vec<&ChunkRef> = Vec::new();
        let mut seen = BTreeSet::new();
        for (index, op) in commit.ops.iter().enumerate() {
            check_ids(op, index)?;
            let Some(content) = op_content(op) else {
                continue;
            };
            for chunk in &check_content(content)?.chunks {
                if seen.insert(chunk.hash.as_slice()) {
                    referenced.push(chunk);
                }
            }
        }
        let receipted: BTreeSet<&[u8]> = commit
            .receipts
            .iter()
            .filter(|r| {
                referenced
                    .iter()
                    .any(|c| c.hash == r.hash && c.size == r.size)
                    && r.token == receipt_token(&r.hash, r.size)
            })
            .map(|r| r.hash.as_slice())
            .collect();

        if commit.base_seq > self.head_seq {
            return Err(fail(
                error::Code::InvalidArgument,
                "base_seq is ahead of the server",
            ));
        }
        let missing: Vec<Vec<u8>> = referenced
            .iter()
            .filter(|chunk| {
                let ready = self.state.chunks.get(&chunk.hash).is_some_and(|row| {
                    row.size == chunk.size
                        && (row.state == ChunkState::Live
                            || (row.state == ChunkState::Pending
                                && receipted.contains(chunk.hash.as_slice())))
                });
                !ready
            })
            .map(|chunk| chunk.hash.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Error {
                missing_chunks: missing,
                ..fail(error::Code::MissingChunks, "some chunks are not uploaded")
            });
        }

        let seq = self.head_seq + 1;
        let mut state = self.state.clone();
        let ctx = Context {
            seq,
            base_seq: commit.base_seq,
            now_ms,
            committer: session,
        };
        let (results, changes) = Applier::new(&mut state, ctx).apply(&commit.ops)?;
        self.state = state;
        let record = CommitRecord {
            seq,
            commit_id: commit.commit_id.clone(),
            device_id: session.device_id.clone(),
            user_id: session.user_id.clone(),
            committed_at_ms: now_ms,
            changes: changes.clone(),
        };
        let result = CommitResult {
            seq,
            results,
            changes,
        };
        self.log.push(record.clone());
        self.results
            .insert(commit.commit_id.clone(), result.clone());
        self.head_seq = seq;
        Ok(Committed {
            result,
            record: Some(record),
        })
    }

    pub fn prepare_upload(
        &mut self,
        session: &Session,
        now_ms: i64,
        request: &PrepareUpload,
    ) -> Result<PrepareUploadResult, Error> {
        self.check_can_write(session)?;
        if request.chunks.len() > MAX_CHUNKS_PER_CALL {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_CHUNKS_PER_CALL} chunks per call"),
            ));
        }
        request.chunks.iter().try_for_each(check_chunk_ref)?;
        let mut seen = BTreeSet::new();
        let chunks: Vec<&ChunkRef> = request
            .chunks
            .iter()
            .filter(|c| seen.insert(c.hash.clone()))
            .collect();
        let mut chunk_rows = self.state.chunks.clone();
        let mut plans = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            let action = match chunk_rows.get_mut(&chunk.hash) {
                None => {
                    chunk_rows.insert(
                        chunk.hash.clone(),
                        ChunkRow {
                            size: chunk.size,
                            state: ChunkState::Pending,
                        },
                    );
                    chunk_plan::Action::Upload
                }
                Some(row) if row.size != chunk.size => {
                    return Err(fail(
                        error::Code::InvalidArgument,
                        "chunk size differs from the stored chunk",
                    ));
                }
                Some(row) => {
                    if row.state == ChunkState::Live {
                        chunk_plan::Action::Present
                    } else {
                        chunk_plan::Action::Upload
                    }
                }
            };
            let target = (action == chunk_plan::Action::Upload).then(|| Target {
                url: format!("sim://chunks/{}", hex(&chunk.hash)),
                method: "PUT".into(),
                expires_at_ms: now_ms + GRANT_MS,
                ..Default::default()
            });
            plans.push(ChunkPlan {
                hash: chunk.hash.clone(),
                action: action as i32,
                target,
                retry_after_ms: 0,
            });
        }
        self.state.chunks = chunk_rows;
        Ok(PrepareUploadResult { chunks: plans })
    }

    /// The relay `PUT` of one chunk: stores it in the bucket and returns the receipt a commit presents.
    pub fn put_chunk(&mut self, hash: &[u8], size: u32) -> UploadReceipt {
        UploadReceipt {
            hash: hash.to_vec(),
            size,
            token: receipt_token(hash, size),
        }
    }

    pub fn download_targets(
        &self,
        now_ms: i64,
        request: &GetDownloadTargets,
    ) -> Result<DownloadTargets, Error> {
        if request.hashes.len() > MAX_CHUNKS_PER_CALL {
            return Err(fail(
                error::Code::LimitExceeded,
                format!("at most {MAX_CHUNKS_PER_CALL} chunks per call"),
            ));
        }
        let missing: Vec<Vec<u8>> = request
            .hashes
            .iter()
            .filter(|h| {
                self.state
                    .chunks
                    .get(*h)
                    .is_none_or(|row| row.state != ChunkState::Live)
            })
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(Error {
                missing_chunks: missing,
                ..fail(error::Code::MissingChunks, "some chunks are not stored")
            });
        }
        let targets = request
            .hashes
            .iter()
            .map(|hash| ChunkTarget {
                hash: hash.clone(),
                target: Some(Target {
                    url: format!("sim://chunks/{}", hex(hash)),
                    method: "GET".into(),
                    expires_at_ms: now_ms + GRANT_MS,
                    ..Default::default()
                }),
            })
            .collect();
        Ok(DownloadTargets { targets })
    }

    pub fn version_chunks(&self, request: &GetVersionChunks) -> Result<VersionChunks, Error> {
        let version = self
            .state
            .versions
            .get(&request.version_id)
            .ok_or_else(|| fail(error::Code::InvalidArgument, "unknown version"))?;
        let page_count = version.content.chunks.len().div_ceil(PAGE_CHUNKS).max(1) as u32;
        if request.page >= page_count {
            return Err(fail(error::Code::InvalidArgument, "page out of range"));
        }
        let chunks = version
            .content
            .chunks
            .chunks(PAGE_CHUNKS)
            .nth(request.page as usize)
            .unwrap_or_default()
            .to_vec();
        Ok(VersionChunks {
            chunks,
            page: request.page,
            page_count,
        })
    }

    /// The retained versions of a node, newest first, paged by seq.
    pub fn list_versions(&self, request: &ListVersions) -> VersionList {
        let limit = if request.limit == 0 {
            100
        } else {
            request.limit.min(1000)
        } as usize;
        let before: u64 = request.cursor.parse().unwrap_or(u64::MAX);
        let mut found: Vec<&Version> = self
            .state
            .versions
            .values()
            .filter(|v| v.node_id == request.node_id && v.seq < before)
            .collect();
        found.sort_by_key(|v| std::cmp::Reverse(v.seq));
        let page: Vec<&Version> = found.iter().take(limit).copied().collect();
        let next_cursor = if found.len() > limit {
            page.last().map(|v| v.seq.to_string()).unwrap_or_default()
        } else {
            String::new()
        };
        VersionList {
            versions: page
                .into_iter()
                .map(|v| self.state.version_info(v))
                .collect(),
            next_cursor,
        }
    }

    fn node_page(
        &self,
        status: NodeStatus,
        cursor: &str,
        requested: u32,
    ) -> (Vec<NodeState>, String) {
        let limit = if requested == 0 {
            1000
        } else {
            requested.min(5000)
        } as usize;
        let found: Vec<&Node> = self
            .state
            .nodes
            .values()
            .filter(|n| n.status == status && n.node_id.as_str() > cursor)
            .take(limit + 1)
            .collect();
        let mut nodes = Vec::new();
        let mut bytes = 0;
        for node in found.iter().take(limit) {
            let state = self.state.node_state(node);
            bytes += state.encoded_len();
            if !nodes.is_empty() && bytes > PAGE_BYTES {
                break;
            }
            nodes.push(state);
        }
        let next_cursor = if nodes.len() < found.len() {
            nodes.last().map(|n| n.node_id.clone()).unwrap_or_default()
        } else {
            String::new()
        };
        (nodes, next_cursor)
    }

    /// One page of the live tree in `node_id` order (spec v0 section 7.5).
    pub fn list_tree(&self, request: &ListTree) -> TreePage {
        let (nodes, next_cursor) = self.node_page(NodeStatus::Live, &request.cursor, request.limit);
        TreePage {
            nodes,
            next_cursor,
            head_seq: self.head_seq,
        }
    }

    pub fn list_trash(&self, request: &ListTrash) -> TrashPage {
        let (nodes, next_cursor) =
            self.node_page(NodeStatus::Trashed, &request.cursor, request.limit);
        TrashPage { nodes, next_cursor }
    }
}

/// Every record a commit can carry is a `NodeState` in this model.
pub fn node_states(changes: &[ChangeRecord]) -> impl Iterator<Item = &NodeState> {
    changes.iter().filter_map(|c| match &c.change {
        Some(flarebox_proto::v1::change_record::Change::Node(n)) => Some(n),
        _ => None,
    })
}
