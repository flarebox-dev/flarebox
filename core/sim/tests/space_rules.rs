// SPDX-License-Identifier: Apache-2.0

//! The conflict rules of spec v0 section 6 on the reference Space DO, following the scenarios of the
//! TypeScript server's tests (server/test/space-do.test.ts in flarebox#6).

use flarebox_engine::ids::IdGen;
use flarebox_engine::names::conflict_name;
use flarebox_proto::v1::{
    ChunkRef, Commit, CommitResult, Content, CreateDir, CreateFile, Delete, Error, ListTrash,
    ListTree, ListVersions, Move, NodeState, NodeStatus, Op, PrepareUpload, Restore, Resume,
    UpdateContent, UploadReceipt, chunk_plan, error, op, op_result, resync_required,
};
use flarebox_sim::space::{Role, Session, SpaceDo, node_states};
use sha2::{Digest, Sha256};

const NOW: i64 = 1_794_268_740_000;

struct Space {
    space: SpaceDo,
    ids: IdGen,
}

struct Client {
    session: Session,
    base_seq: u64,
}

impl Space {
    fn new() -> Self {
        Self {
            space: SpaceDo::new(1, NOW),
            ids: IdGen::new(2),
        }
    }

    fn root(&self) -> String {
        self.space.space_id().to_string()
    }

    fn id(&mut self) -> String {
        self.ids.uuid_v7(NOW as u64)
    }

    fn client(&self, name: &str) -> Client {
        self.client_with(name, Role::Owner, 1)
    }

    fn client_with(&self, name: &str, role: Role, max_format: u32) -> Client {
        let session = Session {
            device_id: format!("dev-{name}"),
            user_id: "user".into(),
            device_name: name.into(),
            role,
            max_format,
        };
        Client {
            session,
            base_seq: 0,
        }
    }

    fn content(text: &str) -> Content {
        let chunks = vec![ChunkRef {
            hash: Sha256::digest(text).to_vec(),
            size: text.len() as u32,
            encoding: 0,
        }];
        let list: Vec<u8> = chunks
            .iter()
            .flat_map(|c| c.hash.iter().copied().chain(c.size.to_le_bytes()))
            .collect();
        Content {
            size: text.len() as u64,
            mtime_ns: 1,
            executable: false,
            chunk_count: 1,
            manifest_digest: Sha256::digest(list).to_vec(),
            chunker: "fastcdc2020-1m-4m-16m".into(),
            chunks,
        }
    }

    fn upload(&mut self, c: &Client, content: &Content) -> Vec<UploadReceipt> {
        let plan = self
            .space
            .prepare_upload(
                &c.session,
                NOW,
                &PrepareUpload {
                    chunks: content.chunks.clone(),
                },
            )
            .unwrap();
        plan.chunks
            .iter()
            .zip(&content.chunks)
            .filter(|(p, _)| p.action == chunk_plan::Action::Upload as i32)
            .map(|(_, chunk)| self.space.put_chunk(&chunk.hash, chunk.size))
            .collect()
    }

    fn commit_raw(
        &mut self,
        c: &Client,
        ops: Vec<Op>,
        base_seq: u64,
        receipts: Vec<UploadReceipt>,
        commit_id: Option<String>,
    ) -> Result<CommitResult, Error> {
        let commit_id = commit_id.unwrap_or_else(|| self.id());
        let commit = Commit {
            commit_id,
            base_seq,
            ops,
            receipts,
        };
        self.space
            .commit(&c.session, NOW, &commit)
            .map(|done| done.result)
    }

    /// Commits at the client's base and moves the base to the new head, as a caught-up client would.
    fn commit(
        &mut self,
        c: &mut Client,
        ops: Vec<Op>,
        receipts: Vec<UploadReceipt>,
    ) -> CommitResult {
        let result = self.commit_raw(c, ops, c.base_seq, receipts, None).unwrap();
        c.base_seq = result.seq;
        result
    }

    fn create_file(
        &mut self,
        c: &mut Client,
        parent: &str,
        name: &str,
        text: &str,
    ) -> (String, CommitResult) {
        let content = Self::content(text);
        let receipts = self.upload(c, &content);
        let id = self.id();
        let result = self.commit(c, vec![create_file(&id, parent, name, content)], receipts);
        (id, result)
    }

    fn live(&self, node_id: &str) -> bool {
        self.space
            .node(node_id)
            .is_some_and(|n| n.status == NodeStatus::Live as i32)
    }

    fn get(&self, node_id: &str) -> NodeState {
        self.space.node(node_id).unwrap()
    }

    fn live_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .space
            .nodes()
            .into_iter()
            .filter(|n| n.status == NodeStatus::Live as i32)
            .map(|n| n.name)
            .collect();
        names.sort();
        names
    }
}

fn wrap(op: op::Op) -> Op {
    Op { op: Some(op) }
}

fn mkdir(id: &str, parent: &str, name: &str) -> Op {
    wrap(op::Op::CreateDir(CreateDir {
        node_id: id.into(),
        parent_id: parent.into(),
        name: name.into(),
    }))
}

fn create_file(id: &str, parent: &str, name: &str, content: Content) -> Op {
    wrap(op::Op::CreateFile(CreateFile {
        node_id: id.into(),
        parent_id: parent.into(),
        name: name.into(),
        content: Some(content),
    }))
}

fn update(id: &str, content: Content) -> Op {
    wrap(op::Op::UpdateContent(UpdateContent {
        node_id: id.into(),
        content: Some(content),
    }))
}

fn mv(id: &str, parent: &str, name: &str) -> Op {
    wrap(op::Op::Move(Move {
        node_id: id.into(),
        new_parent_id: parent.into(),
        new_name: name.into(),
    }))
}

fn delete(id: &str) -> Op {
    wrap(op::Op::Delete(Delete { node_id: id.into() }))
}

fn restore(id: &str, version_id: &str) -> Op {
    wrap(op::Op::Restore(Restore {
        node_id: id.into(),
        version_id: version_id.into(),
        ..Default::default()
    }))
}

fn codes(result: &CommitResult) -> Vec<op_result::Code> {
    result.results.iter().map(|r| r.code()).collect()
}

fn error_code(result: Result<CommitResult, Error>) -> error::Code {
    result.unwrap_err().code()
}

#[test]
fn two_devices_commit_and_see_each_others_changes() {
    let mut s = Space::new();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    let (root, docs) = (s.root(), s.id());
    let notes = Space::content("hello from A");
    let receipts = s.upload(&a, &notes);
    let file = s.id();
    let first = s.commit(
        &mut a,
        vec![
            mkdir(&docs, &root, "Docs"),
            create_file(&file, &docs, "a.txt", notes),
        ],
        receipts,
    );
    assert_eq!(first.seq, 1);
    assert_eq!(
        codes(&first),
        [op_result::Code::Applied, op_result::Code::Applied]
    );
    assert_eq!(first.changes.len(), 2);

    b.base_seq = 1;
    let (_, second) = s.create_file(&mut b, &docs, "b.txt", "hello from B");
    assert_eq!(second.seq, 2);
    assert_eq!(s.live_names(), ["", "Docs", "a.txt", "b.txt"]);
}

#[test]
fn returns_the_stored_result_when_a_commit_id_is_retried() {
    let mut s = Space::new();
    let a = s.client("A");
    let (root, commit_id) = (s.root(), s.id());
    let (once, twice) = (s.id(), s.id());
    let first = s
        .commit_raw(
            &a,
            vec![mkdir(&once, &root, "Once")],
            0,
            vec![],
            Some(commit_id.clone()),
        )
        .unwrap();
    let again = s
        .commit_raw(
            &a,
            vec![mkdir(&twice, &root, "Twice")],
            0,
            vec![],
            Some(commit_id),
        )
        .unwrap();
    assert_eq!(again, first);
    assert!(!s.live_names().contains(&"Twice".to_string()));
    assert_eq!(s.space.head_seq(), 1);
}

#[test]
fn rejects_a_base_ahead_of_the_server_and_commits_from_viewers_or_old_clients() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let x = s.id();
    assert_eq!(
        error_code(s.commit_raw(&a, vec![mkdir(&x, &root, "X")], 5, vec![], None)),
        error::Code::InvalidArgument
    );
    let viewer = s.client_with("Viewer", Role::Viewer, 1);
    assert_eq!(
        error_code(s.commit_raw(&viewer, vec![mkdir(&x, &root, "X")], 0, vec![], None)),
        error::Code::Forbidden
    );
    let old = s.client_with("Old", Role::Editor, 0);
    assert_eq!(
        error_code(s.commit_raw(&old, vec![mkdir(&x, &root, "X")], 0, vec![], None)),
        error::Code::ReadOnly
    );
    assert!(s.space.welcome(&old.session).read_only);
    assert!(!s.space.welcome(&a.session).read_only);
}

#[test]
fn requires_every_pending_chunk_to_carry_an_upload_receipt() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let content = Space::content("chunk bytes");
    let receipts = s.upload(&a, &content);
    let f = s.id();
    let missing = s
        .commit_raw(
            &a,
            vec![create_file(&f, &root, "f.bin", content.clone())],
            0,
            vec![],
            None,
        )
        .unwrap_err();
    assert_eq!(missing.code(), error::Code::MissingChunks);
    assert_eq!(missing.missing_chunks, [content.chunks[0].hash.clone()]);
    let forged = UploadReceipt {
        token: "forged".into(),
        ..receipts[0].clone()
    };
    assert_eq!(
        error_code(s.commit_raw(
            &a,
            vec![create_file(&f, &root, "f.bin", content.clone())],
            0,
            vec![forged],
            None
        )),
        error::Code::MissingChunks
    );
    s.commit(
        &mut a,
        vec![create_file(&f, &root, "f.bin", content.clone())],
        receipts,
    );

    let plan = s
        .space
        .prepare_upload(
            &b.session,
            NOW,
            &PrepareUpload {
                chunks: content.chunks.clone(),
            },
        )
        .unwrap();
    assert_eq!(plan.chunks[0].action, chunk_plan::Action::Present as i32);
    b.base_seq = 1;
    let copy = s.id();
    let reuse = s.commit(
        &mut b,
        vec![create_file(&copy, &root, "copy.bin", content)],
        vec![],
    );
    assert_eq!(codes(&reuse), [op_result::Code::Applied]);
}

#[test]
fn rejects_malformed_content_and_ids() {
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let mut content = Space::content("abc");
    let receipts = s.upload(&a, &content);
    content.size = 4;
    let f = s.id();
    assert_eq!(
        error_code(s.commit_raw(
            &a,
            vec![create_file(&f, &root, "bad", content)],
            0,
            receipts,
            None
        )),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![mkdir("not-a-uuid", &root, "x")], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![Op { op: None }], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    assert_eq!(
        error_code(s.commit_raw(&a, vec![], 0, vec![], None)),
        error::Code::InvalidArgument
    );
    let many = (0..1001).map(|_| delete(&root)).collect();
    assert_eq!(
        error_code(s.commit_raw(&a, many, 0, vec![], None)),
        error::Code::LimitExceeded
    );
    assert_eq!(s.space.head_seq(), 0);
}

#[test]
fn r1_renames_an_incoming_node_whose_name_clashes_case_insensitively() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    s.create_file(&mut a, &root, "Notes.txt", "a");
    let (_, result) = s.create_file(&mut b, &root, "notes.TXT", "b");
    assert_eq!(result.results[0].code(), op_result::Code::AppliedRenamed);
    assert_eq!(
        result.results[0].name,
        conflict_name("notes.TXT", "Desktop B", NOW, 1)
    );
    assert_eq!(
        result.results[0].name,
        "notes (conflict from Desktop B 2026-11-09).TXT"
    );
    let (_, again) = s.create_file(&mut b, &root, "NOTES.txt", "c");
    assert_eq!(
        again.results[0].name,
        "NOTES (conflict from Desktop B 2026-11-09 2).txt"
    );
}

#[test]
fn r2_treats_canonically_equivalent_names_that_differ_only_by_case_as_one_name() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let first = s.id();
    s.commit(&mut a, vec![mkdir(&first, &root, "\u{0390}")], vec![]);
    let second = s.id();
    let result = s.commit(
        &mut b,
        vec![mkdir(&second, &root, "\u{03AA}\u{0301}")],
        vec![],
    );
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    assert_eq!(result.results[0].node_id, first);
}

#[test]
fn r2_merges_a_new_directory_into_an_existing_one_with_the_same_name() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (photos_a, photos_b, file) = (s.id(), s.id(), s.id());
    s.commit(&mut a, vec![mkdir(&photos_a, &root, "Photos")], vec![]);
    let content = Space::content("img");
    let receipts = s.upload(&b, &content);
    let result = s.commit(
        &mut b,
        vec![
            mkdir(&photos_b, &root, "photos"),
            create_file(&file, &photos_b, "1.jpg", content),
        ],
        receipts,
    );
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    assert_eq!(
        (
            result.results[0].node_id.as_str(),
            result.results[0].name.as_str()
        ),
        (photos_a.as_str(), "Photos")
    );
    assert_eq!(s.get(&file).parent_id, photos_a);
    assert!(s.space.node(&photos_b).is_none());
}

#[test]
fn r2_does_not_merge_a_directory_into_a_file() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    s.create_file(&mut a, &root, "Thing", "x");
    let dir = s.id();
    let result = s.commit(&mut a, vec![mkdir(&dir, &root, "thing")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::AppliedRenamed);
}

#[test]
fn r3_keeps_the_first_edit_and_turns_a_concurrent_edit_into_a_conflict_copy() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("Laptop A"), s.client("Desktop B"));
    let (node, _) = s.create_file(&mut a, &root, "report.docx", "v1");
    b.base_seq = 1;
    let from_b = Space::content("v2 from B");
    let receipts = s.upload(&b, &from_b);
    s.commit(&mut b, vec![update(&node, from_b.clone())], receipts);

    let from_a = Space::content("v2 from A");
    let receipts = s.upload(&a, &from_a);
    let result = s
        .commit_raw(&a, vec![update(&node, from_a.clone())], 1, receipts, None)
        .unwrap();
    let copy = &result.results[0];
    assert_eq!(copy.code(), op_result::Code::ConflictCopy);
    assert_ne!(copy.node_id, node);
    assert_eq!(copy.name, conflict_name("report.docx", "Laptop A", NOW, 1));
    let digest = |id: &str| s.get(id).current.unwrap().content.unwrap().manifest_digest;
    assert_eq!(digest(&node), from_b.manifest_digest);
    assert_eq!(digest(&copy.node_id), from_a.manifest_digest);
    assert_eq!(s.get(&copy.node_id).parent_id, root);
}

#[test]
fn r3_does_not_treat_a_devices_own_earlier_commit_as_unseen() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (node, _) = s.create_file(&mut a, &root, "draft.md", "1");
    for text in ["2", "3"] {
        let content = Space::content(text);
        let receipts = s.upload(&a, &content);
        let result = s
            .commit_raw(&a, vec![update(&node, content)], 1, receipts, None)
            .unwrap();
        assert_eq!(codes(&result), [op_result::Code::Applied]);
    }
}

#[test]
fn r4_applies_an_edit_over_an_unseen_rename() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (node, _) = s.create_file(&mut a, &root, "old.txt", "1");
    b.base_seq = 1;
    s.commit(&mut b, vec![mv(&node, &root, "new.txt")], vec![]);
    let edit = Space::content("2");
    let receipts = s.upload(&a, &edit);
    let result = s
        .commit_raw(&a, vec![update(&node, edit.clone())], 1, receipts, None)
        .unwrap();
    assert_eq!(codes(&result), [op_result::Code::Applied]);
    let state = s.get(&node);
    assert_eq!(state.name, "new.txt");
    assert_eq!(
        state.current.unwrap().content.unwrap().manifest_digest,
        edit.manifest_digest
    );
}

#[test]
fn r5_and_r6_apply_moves_in_commit_order_and_reject_a_cycle() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (x, y) = (s.id(), s.id());
    s.commit(
        &mut a,
        vec![mkdir(&x, &root, "X"), mkdir(&y, &root, "Y")],
        vec![],
    );
    b.base_seq = 1;
    s.commit(&mut a, vec![mv(&x, &y, "X")], vec![]);
    let result = s.commit(&mut b, vec![mv(&y, &x, "Y")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::Rejected);
    assert_eq!(result.results[0].reject_reason, "CYCLE");
    assert_eq!(s.get(&x).parent_id, y);
    assert_eq!(s.get(&y).parent_id, root);
}

#[test]
fn r7_keeps_nodes_with_unseen_changes_and_their_ancestors_when_deleting() {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let (dir, sub) = (s.id(), s.id());
    s.commit(
        &mut a,
        vec![mkdir(&dir, &root, "Project"), mkdir(&sub, &dir, "src")],
        vec![],
    );
    let (edited, _) = s.create_file(&mut a, &sub, "main.rs", "fn main() {}");
    let (untouched, _) = s.create_file(&mut a, &dir, "README", "readme");
    b.base_seq = a.base_seq;
    let edit = Space::content("fn main() { edited }");
    let receipts = s.upload(&a, &edit);
    s.commit(&mut a, vec![update(&edited, edit)], receipts);

    let result = s.commit(&mut b, vec![delete(&dir)], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::Superseded);
    assert!([&dir, &sub, &edited].iter().all(|id| s.live(id)));
    assert!(!s.live(&untouched));
    let trash = s.space.list_trash(&ListTrash::default());
    assert_eq!(
        trash
            .nodes
            .iter()
            .map(|n| n.node_id.clone())
            .collect::<Vec<_>>(),
        [untouched]
    );
}

#[test]
fn r7_trashes_a_whole_subtree_under_one_trash_group() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Old")], vec![]);
    let (file, _) = s.create_file(&mut a, &dir, "x", "x");
    let result = s.commit(&mut a, vec![delete(&dir)], vec![]);
    assert_eq!(codes(&result), [op_result::Code::Applied]);
    let mut ids: Vec<&str> = node_states(&result.changes)
        .map(|n| n.node_id.as_str())
        .collect();
    ids.sort();
    let mut expected = [dir.as_str(), file.as_str()];
    expected.sort();
    assert_eq!(ids, expected);
    assert!(
        node_states(&result.changes).all(|n| n.trash_group == format!("{}:0", result.seq)
            && n.status == NodeStatus::Trashed as i32)
    );
    let again = s.commit(&mut a, vec![delete(&dir)], vec![]);
    assert_eq!(codes(&again), [op_result::Code::Applied]);
    assert!(again.changes.is_empty());
}

#[test]
fn r8_an_unseen_edit_revives_a_deleted_file_and_creating_inside_a_trashed_folder_revives_the_folder()
 {
    let mut s = Space::new();
    let root = s.root();
    let (mut a, mut b) = (s.client("A"), s.client("B"));
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Shared")], vec![]);
    let (file, _) = s.create_file(&mut a, &dir, "notes.txt", "1");
    b.base_seq = a.base_seq;
    s.commit(&mut a, vec![delete(&dir)], vec![]);

    let edit = Space::content("edited on B");
    let receipts = s.upload(&b, &edit);
    let revived = s.commit(&mut b, vec![update(&file, edit)], receipts);
    assert_eq!(codes(&revived), [op_result::Code::Applied]);
    assert!(s.live(&file) && s.live(&dir));

    let base = b.base_seq;
    s.commit_raw(&a, vec![delete(&dir)], base, vec![], None)
        .unwrap();
    let created = s.id();
    s.commit_raw(
        &b,
        vec![mkdir(&created, &dir, "new")],
        base + 1,
        vec![],
        None,
    )
    .unwrap();
    assert!(s.live(&dir) && s.live(&created));
    assert!(!s.live(&file));
}

#[test]
fn r9_and_r10_restore_from_the_trash_and_restore_an_old_version() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let dir = s.id();
    s.commit(&mut a, vec![mkdir(&dir, &root, "Album")], vec![]);
    let (file, created) = s.create_file(&mut a, &dir, "pic.png", "first");
    let first_version = node_states(&created.changes)
        .find(|n| n.node_id == file)
        .unwrap()
        .current
        .clone()
        .unwrap()
        .version_id;
    let second = Space::content("second");
    let receipts = s.upload(&a, &second);
    s.commit(&mut a, vec![update(&file, second)], receipts);

    s.commit(&mut a, vec![delete(&dir)], vec![]);
    let restored = s.commit(&mut a, vec![restore(&dir, "")], vec![]);
    assert_eq!(codes(&restored), [op_result::Code::Applied]);
    assert!(s.live(&dir) && s.live(&file));

    let back = s.commit(&mut a, vec![restore(&file, &first_version)], vec![]);
    assert_eq!(codes(&back), [op_result::Code::Applied]);
    let current = s.get(&file).current.unwrap();
    assert_ne!(current.version_id, first_version);
    assert_eq!(
        current.content.unwrap().manifest_digest,
        Space::content("first").manifest_digest
    );

    let versions = s.space.list_versions(&ListVersions {
        node_id: file.clone(),
        limit: 2,
        ..Default::default()
    });
    assert_eq!(versions.versions.len(), 2);
    assert_ne!(versions.next_cursor, "");
    let rest = s.space.list_versions(&ListVersions {
        node_id: file,
        limit: 2,
        cursor: versions.next_cursor,
    });
    assert_eq!(rest.versions.len(), 1);
    assert_eq!(rest.next_cursor, "");
}

#[test]
fn r11_rejects_invalid_ops_without_failing_the_commit() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (file, _) = s.create_file(&mut a, &root, "file", "x");
    let (under, missing, dot, dir, update_dir) = (s.id(), s.id(), s.id(), s.id(), s.id());
    let result = s.commit(
        &mut a,
        vec![
            mv(&root, &root, "root"),
            delete(&root),
            mkdir(&under, &file, "under a file"),
            delete(&missing),
            mkdir(&file, &root, "duplicate id"),
            mkdir(&dot, &root, ".."),
            mkdir(&dir, &root, "fine"),
            update(&dir, Space::content("x")),
            mkdir(&update_dir, &root, "fine too"),
        ],
        vec![],
    );
    let reasons: Vec<&str> = result
        .results
        .iter()
        .map(|r| r.reject_reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "ROOT",
            "ROOT",
            "PARENT_NOT_DIR",
            "NOT_FOUND",
            "ID_EXISTS",
            "INVALID_NAME",
            "",
            "NOT_FILE",
            ""
        ]
    );
    assert_eq!(result.results[6].code(), op_result::Code::Applied);
}

#[test]
fn treats_names_that_differ_only_by_unicode_case_folding_as_the_same() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let (x, y, z) = (s.id(), s.id(), s.id());
    s.commit(&mut a, vec![mkdir(&x, &root, "Straße")], vec![]);
    let result = s.commit(&mut a, vec![mkdir(&y, &root, "STRASSE")], vec![]);
    assert_eq!(result.results[0].code(), op_result::Code::MergedInto);
    let nfd = s.commit(&mut a, vec![mkdir(&z, &root, "Cafe\u{301}")], vec![]);
    assert_eq!(nfd.results[0].name, "Café");
}

#[test]
fn a_commit_that_fails_while_applying_leaves_no_trace() {
    let mut s = Space::new();
    let mut a = s.client("A");
    let mut parent = s.root();
    let chain: Vec<String> = (0..257).map(|_| s.id()).collect();
    let ops = chain
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let op = mkdir(id, &parent, &format!("d{i}"));
            parent = id.clone();
            op
        })
        .collect();
    let deep = s.commit_raw(&a, ops, 0, vec![], None).unwrap_err();
    assert_eq!(deep.code(), error::Code::LimitExceeded);
    assert_eq!(s.space.head_seq(), 0);
    assert_eq!(s.live_names(), [""]);
    let ok = s.id();
    let root = s.root();
    assert_eq!(
        s.commit(&mut a, vec![mkdir(&ok, &root, "ok")], vec![]).seq,
        1
    );
}

#[test]
fn resumes_from_the_log_or_asks_for_a_resync() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    for name in ["a", "b", "c"] {
        let id = s.id();
        s.commit(&mut a, vec![mkdir(&id, &root, name)], vec![]);
    }
    let history_id = s.space.history_id().to_string();
    let since = |since_seq| Resume {
        history_id: history_id.clone(),
        since_seq,
    };
    let backlog = s.space.resume(&since(1)).unwrap();
    assert_eq!(backlog.iter().map(|r| r.seq).collect::<Vec<_>>(), [2, 3]);
    let reason = |r: Resume| s.space.resume(&r).unwrap_err().reason();
    assert_eq!(reason(since(4)), resync_required::Reason::AheadOfServer);
    assert_eq!(
        reason(Resume {
            history_id: "other".into(),
            since_seq: 0
        }),
        resync_required::Reason::HistoryChanged
    );

    s.space.truncate_log(2);
    assert_eq!(s.space.log_floor_seq(), 2);
    assert_eq!(
        s.space.resume(&since(1)).unwrap_err().reason(),
        resync_required::Reason::LogTruncated
    );
    assert_eq!(
        s.space
            .resume(&since(2))
            .unwrap()
            .iter()
            .map(|r| r.seq)
            .collect::<Vec<_>>(),
        [3]
    );
}

#[test]
fn lists_the_live_tree_in_pages() {
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let ops = (0..25)
        .map(|i| {
            let id = s.id();
            mkdir(&id, &root, &format!("d{i}"))
        })
        .collect();
    s.commit(&mut a, ops, vec![]);
    let mut seen = Vec::new();
    let mut cursor = String::new();
    loop {
        let page = s.space.list_tree(&ListTree {
            cursor: cursor.clone(),
            limit: 10,
        });
        assert_eq!(page.head_seq, 1);
        seen.extend(page.nodes.into_iter().map(|n| n.node_id));
        if page.next_cursor.is_empty() {
            break;
        }
        cursor = page.next_cursor;
    }
    assert_eq!(seen.len(), 26);
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn answers_rpcs_and_returns_the_record_only_for_a_new_commit() {
    use flarebox_proto::v1::{RpcRequest, rpc_request, rpc_response};
    let mut s = Space::new();
    let root = s.root();
    let a = s.client("A");
    let (commit_id, dir) = (s.id(), s.id());
    let commit = Commit {
        commit_id,
        base_seq: 0,
        ops: vec![mkdir(&dir, &root, "D")],
        receipts: vec![],
    };
    let request = RpcRequest {
        req_id: 7,
        call: Some(rpc_request::Call::Commit(commit)),
    };
    let (response, record) = s.space.rpc(&a.session, NOW, &request);
    assert_eq!(response.req_id, 7);
    assert!(matches!(response.result, Some(rpc_response::Result::Commit(ref r)) if r.seq == 1));
    assert_eq!(record.map(|r| r.seq), Some(1));
    let (again, record) = s.space.rpc(&a.session, NOW, &request);
    assert_eq!(again, response);
    assert!(record.is_none());

    let viewer = s.client_with("V", Role::Viewer, 1);
    let (denied, _) = s.space.rpc(
        &viewer.session,
        NOW,
        &RpcRequest {
            req_id: 8,
            call: Some(rpc_request::Call::PrepareUpload(PrepareUpload::default())),
        },
    );
    assert!(
        matches!(denied.result, Some(rpc_response::Result::Error(ref e)) if e.code() == error::Code::Forbidden)
    );
    let (empty, _) = s.space.rpc(
        &a.session,
        NOW,
        &RpcRequest {
            req_id: 9,
            call: None,
        },
    );
    assert!(
        matches!(empty.result, Some(rpc_response::Result::Error(ref e)) if e.code() == error::Code::InvalidArgument)
    );
}

#[test]
fn serves_downloads_only_for_committed_chunks() {
    use flarebox_proto::v1::{GetDownloadTargets, GetVersionChunks};
    let mut s = Space::new();
    let root = s.root();
    let mut a = s.client("A");
    let content = Space::content("bytes");
    let receipts = s.upload(&a, &content);
    let hashes = vec![content.chunks[0].hash.clone()];
    let pending = s
        .space
        .download_targets(
            NOW,
            &GetDownloadTargets {
                hashes: hashes.clone(),
            },
        )
        .unwrap_err();
    assert_eq!(pending.code(), error::Code::MissingChunks);
    let f = s.id();
    let created = s.commit(
        &mut a,
        vec![create_file(&f, &root, "f", content.clone())],
        receipts,
    );
    let targets = s
        .space
        .download_targets(NOW, &GetDownloadTargets { hashes })
        .unwrap();
    assert_eq!(targets.targets.len(), 1);
    assert_eq!(targets.targets[0].target.as_ref().unwrap().method, "GET");

    let version_id = node_states(&created.changes)
        .next()
        .unwrap()
        .current
        .clone()
        .unwrap()
        .version_id;
    let page = s
        .space
        .version_chunks(&GetVersionChunks {
            version_id: version_id.clone(),
            page: 0,
        })
        .unwrap();
    assert_eq!((page.chunks, page.page_count), (content.chunks, 1));
    assert_eq!(
        s.space
            .version_chunks(&GetVersionChunks {
                version_id,
                page: 1
            })
            .unwrap_err()
            .code(),
        error::Code::InvalidArgument
    );
}
