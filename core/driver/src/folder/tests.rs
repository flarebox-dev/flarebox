// SPDX-License-Identifier: Apache-2.0

//! [`Folder`] with a test in place of the shell: a real folder, store and observer, `flarebox-sim`'s space as the
//! server, and chunks held in memory. A test runs the work a step returns and feeds the result back, so it chooses
//! the order results arrive in. After every step the store is loaded again and compared with the engine in memory.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

use flarebox_engine::commit::Lift;
use flarebox_engine::restore::{Outcome, RestoreIntent};
use flarebox_engine::tree::{Kind, LocalNode, Tree};
use flarebox_fs::{EntryKind, Marker, RootProblem, write_marker};
use flarebox_net::Committed;
use flarebox_proto::v1::{
    ListTrash, ListTree, ListVersions, Op, PrepareUpload, Resume, ResyncRequired, chunk_plan, op,
};
use flarebox_sim::space::{Role, Session as Caller, SpaceDo};

use super::*;
use crate::config::SyncRules;
use crate::disk::tests::{Server, ready};
use crate::driver::{DriverError, lift_halt};
use crate::ignore::{local_rules, matcher};
use crate::observe::Observed;
use crate::question::Answer;

const NOW: i64 = 1_700_000_000_000;

/// The server: the space, the bytes of the chunks devices uploaded, and the result of each commit by its ID.
struct Remote {
    space: SpaceDo,
    chunks: Server,
    results: HashMap<String, CommitResult>,
}

impl Remote {
    fn new() -> Remote {
        Remote {
            space: SpaceDo::new(1, NOW),
            chunks: Server::default(),
            results: HashMap::new(),
        }
    }

    /// Every page of the live tree, with `limit` nodes a page.
    fn pages(&self, limit: u32) -> Vec<TreePage> {
        let mut pages: Vec<TreePage> = Vec::new();
        loop {
            let cursor = pages
                .last()
                .map_or(String::new(), |page| page.next_cursor.clone());
            pages.push(self.space.list_tree(&ListTree { cursor, limit }));
            if pages.last().unwrap().next_cursor.is_empty() {
                return pages;
            }
        }
    }

    /// Every page of the trash, with `limit` nodes a page.
    fn trash_pages(&self, limit: u32) -> Vec<TrashPage> {
        let mut pages: Vec<TrashPage> = Vec::new();
        loop {
            let cursor = pages
                .last()
                .map_or(String::new(), |page| page.next_cursor.clone());
            pages.push(self.space.list_trash(&ListTrash { cursor, limit }));
            if pages.last().unwrap().next_cursor.is_empty() {
                return pages;
            }
        }
    }

    /// What `Transfer::upload` does: asks which chunks the space lacks, reads those from the folder and stores them.
    fn store(&mut self, caller: &Caller, upload: &Upload) -> Vec<UploadReceipt> {
        let mut chunks = upload.chunks.clone();
        chunks.sort_by(|a, b| a.hash.cmp(&b.hash));
        chunks.dedup_by(|a, b| a.hash == b.hash);
        let request = PrepareUpload {
            chunks: chunks.clone(),
        };
        let plan = self.space.prepare_upload(caller, NOW, &request).unwrap();
        let wanted = |chunk: &ChunkRef| {
            plan.chunks.iter().any(|plan| {
                plan.hash == chunk.hash && plan.action == chunk_plan::Action::Upload as i32
            })
        };
        let mut receipts = Vec::new();
        for chunk in chunks.iter().filter(|chunk| wanted(chunk)) {
            let bytes = upload.read(chunk).unwrap();
            assert!(flarebox_net::matches(chunk, &bytes));
            self.chunks
                .chunks
                .insert(chunk.hash.clone(), bytes.to_vec());
            receipts.push(self.space.put_chunk(&chunk.hash, chunk.size, NOW));
        }
        receipts
    }

    /// What `Session::commit` does.
    fn commit(&mut self, caller: &Caller, commit: &Commit) -> Result<CommitResult, CallError> {
        match self.space.commit(caller, NOW, commit) {
            Ok(committed) => {
                let id = commit.commit_id.clone();
                self.results.insert(id, committed.result.clone());
                Ok(committed.result)
            }
            Err(error) => Err(CallError::Server(ServerError(error))),
        }
    }

    /// The names of the live nodes besides the root, in order.
    fn names(&self) -> Vec<String> {
        let root = self.space.space_id();
        let live = flarebox_proto::v1::NodeStatus::Live as i32;
        let mut names: Vec<String> = (self.space.nodes().into_iter())
            .filter(|node| node.node_id != root && node.status == live)
            .map(|node| node.name)
            .collect();
        names.sort();
        names
    }
}

/// A device's folder and store on disk. They outlive the [`Device`], as they outlive a process.
struct Site {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
    link: Link,
    caller: Caller,
}

impl Site {
    /// An empty folder linked to the remote's space, never synced.
    fn new(remote: &Remote, name: &str) -> Site {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("space");
        fs::create_dir(&root).unwrap();
        let space_id = remote.space.space_id().to_owned();
        let marker = Marker {
            space_id: space_id.clone(),
            effects: 0,
        };
        write_marker(&root, &marker).unwrap();
        let db = dir.path().join("store.sqlite");
        let link = Link {
            space_id,
            space_name: "test".into(),
            root: root.clone(),
            root_dev: fs::metadata(&root).unwrap().dev(),
            server_url: "https://files.example.com/".into(),
            device_id: name.into(),
            linked_at_ms: 0,
        };
        let mut store = Store::open(&db).unwrap();
        store.transaction(|tx| tx.set_link(&link)).unwrap();
        let caller = Caller {
            device_id: name.into(),
            user_id: "user".into(),
            device_name: name.into(),
            role: Role::Owner,
            max_format: MAX_FORMAT,
        };
        Site {
            _dir: dir,
            root,
            db,
            link,
            caller,
        }
    }

    /// The engine a process that started now would load from the store. Its trees must be trees.
    fn stored(&self) -> Engine {
        let mut store = Store::open(&self.db).unwrap();
        let engine = store
            .transaction(|tx| Engine::load(tx, &self.link.space_id))
            .unwrap();
        assert_eq!(engine.trees.check(), Ok(()));
        engine
    }

    /// Starts the folder as the task does once it holds the lock: the store, the engine, the disk, and the observer
    /// from L's snapshot.
    fn start(&self) -> Device {
        self.start_with(&SyncRules::default())
    }

    /// [`Site::start`] on a device with a `[sync]` list.
    fn start_with(&self, sync: &SyncRules) -> Device {
        let lock = claim::lock(&self.root)
            .unwrap()
            .expect("the folder is not locked");
        let local = local_rules(sync).unwrap();
        let folder = Folder::open(self.link.clone(), &lock, &self.db, local).unwrap();
        let observer = FsObserver::start(
            &self.root,
            &self.link.space_id,
            self.link.root_dev,
            folder.snapshot(),
            matcher(sync).unwrap(),
            || false,
        )
        .unwrap();
        Device {
            folder,
            observer,
            caller: self.caller.clone(),
            work: VecDeque::new(),
            sent: Vec::new(),
            _lock: lock,
        }
    }
}

/// A running folder with what the shell holds for it.
struct Device {
    folder: Folder,
    observer: FsObserver,
    caller: Caller,
    /// Work the folder asked for that the test has not run.
    work: VecDeque<Work>,
    /// Every commit handed to `Session::commit`, in order.
    sent: Vec<Commit>,
    _lock: claim::Flock,
}

impl Device {
    /// Takes what one step returned. The store must then give the engine the folder holds.
    fn take(&mut self, step: Result<Vec<Work>, Stop>) -> Result<(), Stop> {
        self.work.extend(step?);
        let folder = &mut self.folder;
        let stored = folder
            .store
            .transaction(|tx| Engine::load(tx, &folder.link.space_id))
            .unwrap();
        let held = &folder.engine;
        assert_eq!(held.trees.check(), Ok(()));
        assert_eq!(stored.trees.local(), held.trees.local(), "L");
        assert_eq!(stored.trees.synced(), held.trees.synced(), "S");
        assert_eq!(stored.trees.remote(), held.trees.remote(), "R");
        assert_eq!(stored.trees.state(), held.trees.state());
        assert_eq!(stored.commits.state(), held.commits.state());
        assert_eq!(stored.effects.state(), held.effects.state());
        assert_eq!(stored.cursor, held.cursor);
        assert_eq!(stored.ids.state, held.ids.state);
        Ok(())
    }

    fn event(&mut self, event: Event) -> Result<(), Stop> {
        let step = self.folder.event(event);
        self.take(step)
    }

    fn connect(&mut self, remote: &Remote) {
        let welcome = remote.space.welcome(&self.caller);
        self.event(Event::Connected(welcome)).unwrap();
    }

    /// One pass of the observer's thread.
    fn scan(&mut self, wait: Duration) {
        self.try_scan(wait).unwrap();
    }

    /// One pass of the observer's thread. Returns the folders its batch could not read.
    fn try_scan(&mut self, wait: Duration) -> Result<Vec<PathBuf>, Stop> {
        let sections = self.folder.sections();
        let mut unreadable = Vec::new();
        if let Some(batch) = sections.observe(&mut self.observer, wait).unwrap() {
            unreadable.clone_from(&batch.observation.unreadable);
            let entries = self.observer.snapshot().entries.len() as u64;
            let step = self.folder.observed(batch, entries);
            self.take(step)?;
        }
        if !self.folder.scanned && self.observer.scanned().is_some() {
            let step = self.folder.scanned();
            self.take(step)?;
        }
        Ok(unreadable)
    }

    /// Observes until `done`, for at most ten seconds.
    fn scan_until(&mut self, done: impl Fn(&Device) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < deadline, "not observed in time");
            self.scan(Duration::from_millis(100));
        }
    }

    /// Observes for about a second, to take in changes that must lead to no work.
    fn settle(&mut self) {
        for _ in 0..3 {
            self.scan(Duration::from_millis(300));
        }
    }

    /// What the session delivers after a `Resume`: the log after `since_seq`, with this device's own commits as
    /// their results, then `CaughtUp`.
    fn resume(&mut self, remote: &Remote, history_id: String, since_seq: u64) -> Result<(), Stop> {
        let resume = Resume {
            history_id,
            since_seq,
        };
        let records = match remote.space.resume(&resume) {
            Ok(records) => records,
            Err(resync) => return self.event(Event::ResyncRequired(resync)),
        };
        for record in records {
            let event = if record.device_id == self.caller.device_id {
                Event::Committed(Committed {
                    result: remote.results[&record.commit_id].clone(),
                    commit_id: record.commit_id,
                })
            } else {
                Event::Changes(vec![record])
            };
            self.event(event)?;
        }
        let head_seq = remote.space.head_seq();
        self.event(Event::CaughtUp { head_seq })
    }

    /// What a live session pushes: everything after the cursor.
    fn pull(&mut self, remote: &Remote) {
        let cursor = self
            .folder
            .engine
            .cursor
            .clone()
            .expect("R follows the log");
        self.resume(remote, cursor.history_id, cursor.last_seq)
            .unwrap();
    }

    /// Runs one piece of work and feeds its result back.
    fn perform(&mut self, work: Work, remote: &mut Remote) -> Result<(), Stop> {
        let step = match work {
            Work::Subscribe {
                history_id,
                since_seq,
            } => return self.resume(remote, history_id, since_seq),
            Work::List { trash } | Work::ListAfter(_, Listing { trash, .. }) => {
                let trash = match trash {
                    true => remote.trash_pages(TREE_PAGE),
                    false => Vec::new(),
                };
                let pages = remote.pages(TREE_PAGE);
                self.folder.listed(Ok(Listed::Done { pages, trash }))
            }
            Work::Stage(pending) | Work::StageAfter(_, pending) => {
                let staged = ready(pending.stage(&remote.chunks));
                self.folder.staged(pending, staged)
            }
            Work::Upload(upload) => {
                let receipts = remote.store(&self.caller, &upload);
                self.folder.uploaded(Ok(receipts))
            }
            Work::Send(commit) => {
                self.sent.push(commit.clone());
                let result = remote.commit(&self.caller, &commit);
                self.folder.sent(result)
            }
            Work::ResendAfter(_) => self.folder.resend(),
            Work::RetryAfter(_) => self.folder.retry(),
        };
        self.take(step)
    }

    /// Runs the work the folder asked for and the work that follows from it, until none is left. Returns what ran.
    fn try_run(&mut self, remote: &mut Remote) -> Result<Vec<&'static str>, Stop> {
        let mut ran = Vec::new();
        while let Some(work) = self.work.pop_front() {
            ran.push(match &work {
                Work::Subscribe { .. } => "subscribe",
                Work::List { .. } => "list",
                Work::ListAfter(..) => "list again",
                Work::Stage(_) => "stage",
                Work::StageAfter(..) => "stage again",
                Work::Upload(_) => "upload",
                Work::Send(_) => "send",
                Work::ResendAfter(_) => "wait",
                Work::RetryAfter(_) => "retry",
            });
            self.perform(work, remote)?;
        }
        Ok(ran)
    }

    fn run(&mut self, remote: &mut Remote) -> Vec<&'static str> {
        self.try_run(remote).unwrap()
    }

    /// Writes a file and syncs it to the space.
    fn create(&mut self, remote: &mut Remote, path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).unwrap();
        self.scan_until(|device| !device.work.is_empty());
        assert_eq!(self.run(remote), ["upload", "send"]);
    }

    fn state(&self) -> FolderState {
        self.folder.view().state
    }
}

/// A device that linked an empty folder to the remote's space and read the space's tree.
fn linked(remote: &mut Remote, name: &str) -> (Site, Device) {
    let site = Site::new(remote, name);
    let mut device = site.start();
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.run(remote);
    (site, device)
}

/// A file replaced by a hard link to another file takes that file's content and keeps its own node, so the other
/// path stays on the server.
#[test]
fn a_file_replaced_by_a_hard_link_to_another_leaves_the_other_on_the_server() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::remove_file(site.root.join("two.txt")).unwrap();
    fs::hard_link(site.root.join("one.txt"), site.root.join("two.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(remote.names(), ["one.txt", "two.txt"]);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
}

/// Device b makes a folder and moves a file it downloaded into it before its observer looks. The scan lists the move
/// before the new folder, and the move still reaches the server and device a.
#[test]
fn a_file_moved_into_a_folder_made_in_the_same_batch_reaches_the_other_device() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("report.txt"), b"the report");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::rename(
        site.root.join("report.txt"),
        site.root.join("docs/renamed.txt"),
    )
    .unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(remote.names(), ["docs", "renamed.txt"]);
    assert_eq!(
        files(&site_a.root),
        [("docs/renamed.txt".to_owned(), "the report".to_owned())]
    );
}

/// Device b moves the file `a` into a new folder and renames the folder `b` to `a` before its observer looks. The folder
/// takes the name only after the file left it, so the file is not lost.
#[test]
fn a_folder_renamed_to_the_name_a_file_left_for_a_new_folder_keeps_the_file() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"the file");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("b/f"), b"in b");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site.root.join("docs")).unwrap();
    fs::rename(site.root.join("a"), site.root.join("docs/a")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(remote.names(), ["a", "a", "docs", "f"]);
    let both = [("a/f", "in b"), ("docs/a", "the file")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), both);
}

/// Device b swaps the file `a` and the folder `b` through a third name before its observer looks. The scan reports
/// two renames that each take the other's name. The folder keeps its child, and the file keeps its bytes, under the
/// name the server gives one side of a name swap (design §2.6).
#[test]
fn a_file_and_a_folder_swapped_in_one_batch_keep_the_folders_child() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"file A");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(
        &mut remote,
        &site_a.root.join("b/child"),
        b"directory child",
    );
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site.root.join("a"), site.root.join("tmp")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    fs::rename(site.root.join("tmp"), site.root.join("b")).unwrap();
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let both = files(&site_a.root);
    assert_eq!(files(&site.root), both);
    assert!(both.contains(&("a/child".to_owned(), "directory child".to_owned())));
    assert_eq!(both.iter().filter(|(_, text)| text == "file A").count(), 1);
}

/// Device b rotates the file `a` and the folders `b` and `c` through a third name before its observer looks: `c`
/// takes `a`, `b` takes `c` and the file takes `b`. Each folder keeps its own child on both devices.
#[test]
fn a_file_and_two_folders_rotated_in_one_batch_keep_each_child_in_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("a"), b"A");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.create(&mut remote, &site_a.root.join("b/one"), b"B child");
    fs::create_dir(site_a.root.join("c")).unwrap();
    a.create(&mut remote, &site_a.root.join("c/two"), b"C child");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (from, to) in [("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let both = files(&site_a.root);
    assert_eq!(both, files(&site.root));
    assert!(both.contains(&("a/two".to_owned(), "C child".to_owned())));
    assert!(both.contains(&("c/one".to_owned(), "B child".to_owned())));
    assert_eq!(both.iter().filter(|(_, bytes)| bytes == "A").count(), 1);
}

/// Device b swaps two files, and two folders with a file each, through a third name before its observer looks.
/// Every file keeps its bytes at its new path on both devices.
#[test]
fn files_and_folders_swapped_in_one_batch_keep_every_byte() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one"), b"first file");
    a.create(&mut remote, &site_a.root.join("two"), b"second file");
    fs::create_dir(site_a.root.join("x")).unwrap();
    a.create(&mut remote, &site_a.root.join("x/in-x"), b"in x");
    fs::create_dir(site_a.root.join("y")).unwrap();
    a.create(&mut remote, &site_a.root.join("y/in-y"), b"in y");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    for (first, second) in [("one", "two"), ("x", "y")] {
        fs::rename(site.root.join(first), site.root.join("tmp")).unwrap();
        fs::rename(site.root.join(second), site.root.join(first)).unwrap();
        fs::rename(site.root.join("tmp"), site.root.join(second)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let swapped = [
        ("one", "second file"),
        ("two", "first file"),
        ("x/in-y", "in y"),
        ("y/in-x", "in x"),
    ]
    .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
}

/// Device a swaps the file `a` with the file `b`, which its rules exclude, through a third name. The file that took
/// `b` left what this device syncs, so its copy stays on the server.
#[test]
fn a_file_swapped_into_a_name_the_rules_exclude_keeps_its_remote_copy() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("a"), b"A");
    a.create(&mut remote, &site.root.join("b"), b"BB");
    let old_a = a
        .folder
        .engine
        .trees
        .remote()
        .iter()
        .find(|(_, n)| n.shape.name == "a")
        .unwrap()
        .0
        .clone();
    a.create(&mut remote, &site.root.join(".fbignore"), b"b\n");
    fs::rename(site.root.join("a"), site.root.join("tmp")).unwrap();
    fs::rename(site.root.join("b"), site.root.join("a")).unwrap();
    fs::rename(site.root.join("tmp"), site.root.join("b")).unwrap();
    let included_ino = fs::metadata(site.root.join("a")).unwrap().ino();
    a.scan_until(|a| {
        a.observer
            .snapshot()
            .entries
            .get(Path::new("a"))
            .is_some_and(|e| e.id.ino == included_ino)
    });
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site.root.join("b")).unwrap(), b"A");
    assert!(
        a.folder
            .engine
            .trees
            .remote()
            .get(&old_a)
            .is_some_and(|n| !n.trashed),
        "the copy moved into an ignored name was deleted from the server"
    );
}

/// Device b holds an untracked symlink at e. The server moves d to e and creates another file at d.
/// The move fails because e is taken, but d is parked out of the way, so the new file arrives at d
/// and the symlink stays.
#[test]
fn a_move_onto_a_name_a_symlink_holds_still_lets_the_new_file_take_the_old_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("d"), b"original");
    let (site, mut b) = linked(&mut remote, "b");
    std::os::unix::fs::symlink("untouched", site.root.join("e")).unwrap();
    fs::rename(site_a.root.join("d"), site_a.root.join("e")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    a.create(&mut remote, &site_a.root.join("d"), b"new remote file");
    b.pull(&remote);
    for _ in 0..24 {
        if let Some(work) = b.work.pop_front() {
            b.perform(work, &mut remote).unwrap();
        }
        b.scan(Duration::ZERO);
    }
    assert_eq!(
        fs::read_link(site.root.join("e")).unwrap(),
        PathBuf::from("untouched")
    );
    assert_eq!(fs::read(site.root.join("d")).unwrap(), b"new remote file");
}

/// The names in a folder besides `.flarebox`, in order.
fn names(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != flarebox_fs::PRIVATE_DIR)
        .collect();
    names.sort();
    names
}

/// Every file under a folder besides `.flarebox`, by its path from the folder, with its text, in order.
fn files(root: &Path) -> Vec<(String, String)> {
    let mut files = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join(flarebox_fs::PRIVATE_DIR) {
                continue;
            }
            if path.is_dir() {
                dirs.push(path);
            } else {
                let name = path.strip_prefix(root).unwrap().to_str().unwrap();
                files.push((name.to_owned(), fs::read_to_string(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

/// Every entry under a folder besides `.flarebox`, by its path from the folder, with the text of a file, in order.
fn entries(root: &Path) -> Vec<(String, Option<String>)> {
    let mut entries = Vec::new();
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path == root.join(flarebox_fs::PRIVATE_DIR) {
                continue;
            }
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            if path.is_dir() {
                entries.push((name, None));
                dirs.push(path);
            } else {
                entries.push((name, Some(fs::read_to_string(&path).unwrap())));
            }
        }
    }
    entries.sort();
    entries
}

/// Runs two devices as their tasks would, each one piece of work at a time with a pass of its observer after it,
/// until neither has had work for a while. Both must then hold the server's head and the same entries.
fn sync(remote: &mut Remote, a: (&Site, &mut Device), b: (&Site, &mut Device)) {
    let (mut quiet, mut rounds) = (0, 0);
    while quiet < 4 {
        rounds += 1;
        assert!(rounds < 200, "the devices do not come to rest");
        quiet += 1;
        for (site, device) in [(a.0, &mut *a.1), (b.0, &mut *b.1)] {
            match device.work.pop_front() {
                Some(work) => {
                    device.perform(work, remote).unwrap();
                    quiet = 0;
                }
                None => device.pull(remote),
            }
            device.scan(Duration::from_millis(150));
            if !device.work.is_empty() {
                quiet = 0;
            }
            site.stored();
        }
    }
    assert_eq!(a.1.state(), FolderState::UpToDate);
    assert_eq!(b.1.state(), FolderState::UpToDate);
    assert_eq!(entries(&a.0.root), entries(&b.0.root));
}

/// Two devices that hold the folder `f` with `f/kept.txt`, written on `b`. Device `a` then wrote the file `a`, and
/// `b` planned the effect that writes it, which is returned unstaged.
fn planned_beside_a_folder(remote: &mut Remote) -> ((Site, Device), (Site, Device), Pending) {
    let (site_a, mut a) = linked(remote, "a");
    let (site, mut b) = linked(remote, "b");
    fs::create_dir(site.root.join("f")).unwrap();
    fs::write(site.root.join("f/kept.txt"), b"kept on b").unwrap();
    sync(remote, (&site_a, &mut a), (&site, &mut b));
    assert_eq!(entries(&site_a.root).len(), 2);
    a.create(remote, &site_a.root.join("a"), b"written on a");

    b.pull(remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes a is staged");
    };
    assert!(b.work.is_empty());
    site.stored();
    ((site_a, a), (site, b), pending)
}

/// The user renames the folder `f` to `a`, which a batch reports, and back, which no batch has reported yet.
fn rename_to_a_and_back(site: &Site, b: &mut Device) {
    fs::rename(site.root.join("f"), site.root.join("a")).unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter())
            .any(|(_, node)| node.shape.name == "a" && node.shape.kind == Kind::Dir)
    });
    site.stored();
    fs::rename(site.root.join("a"), site.root.join("f")).unwrap();
}

/// GUN-330. The effect was planned while the name `a` was free in L. Recording its outcome beside the folder L
/// then held at `a` left two nodes with one name, and the store did not load again. The server renames the folder's
/// move to `a`, and the user's rename back to `f` is kept on both devices (GUN-344).
#[test]
fn an_effect_whose_name_l_gave_to_another_node_writes_nothing_and_is_planned_again() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site, mut b), pending) = planned_beside_a_folder(&mut remote);
    rename_to_a_and_back(&site, &mut b);

    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    site.stored();
    assert_eq!(names(&site.root), ["f"], "nothing is written at the name");
    let effects = &b.folder.engine.effects;
    assert!(effects.in_flight().is_none() && effects.blocked().is_empty());
    let marker = flarebox_fs::read_marker(&site.root).unwrap().unwrap();
    assert_eq!(marker.effects, effects.cleared());
    let staging = site.root.join(flarebox_fs::PRIVATE_DIR).join("staging");
    assert_eq!(fs::read_dir(staging).unwrap().count(), 0);

    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    assert_eq!(texts.len(), 2, "{held:?}");
    assert!(texts.contains(&"written on a") && texts.contains(&"kept on b"));
    assert_eq!(
        names(&site.root),
        ["a", "f"],
        "the rename back to f is kept"
    );
    assert_eq!(names(&site_a.root), ["a", "f"]);
}

/// The same L, with the file already at its name when the process stops. `Disk::apply` no longer gets there, so
/// the test leaves what a process without the check left: the marker written and the staged file renamed to `a`,
/// with no acknowledgement. The next start finds the effect's file at its name and does not record it. The
/// observer then reports the file as the user's own, so it is uploaded, and both devices end with the bytes twice:
/// at `a` and at the name the server gave the copy.
#[test]
fn a_file_placed_at_a_name_l_gave_away_is_not_recorded_by_the_next_start() {
    let mut remote = Remote::new();
    let ((site_a, mut a), (site, mut b), pending) = planned_beside_a_folder(&mut remote);
    rename_to_a_and_back(&site, &mut b);

    let Ok(staged) = ready(pending.stage(&remote.chunks)) else {
        panic!("the download is staged");
    };
    let record = b.folder.engine.effects.in_flight().unwrap().clone();
    let marker = Marker {
        space_id: site.link.space_id.clone(),
        effects: record.counter,
    };
    write_marker(&site.root, &marker).unwrap();
    fs::rename(site.root.join(record.staging()), site.root.join("a")).unwrap();
    drop((staged, pending, b));

    let mut b = site.start();
    let stored = site.stored();
    assert!(stored.effects.in_flight().is_none() && stored.effects.blocked().is_empty());
    assert_eq!(stored.effects.cleared(), record.counter);
    assert!(stored.trees.local().get(&record.node).is_none());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let mut texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    texts.sort();
    assert_eq!(
        texts,
        ["kept on b", "written on a", "written on a"],
        "{held:?}"
    );
}

/// The second way to the same store: the folder `a` is planned, the user makes a folder `a` of their own, which a
/// batch reports, and the process stops. The next start finds a folder at the effect's name and takes the effect
/// for done.
#[test]
fn a_restart_that_finds_the_users_folder_at_a_planned_folders_name_keeps_l_a_tree() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("a")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);

    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that creates a is staged");
    };
    site.stored();
    fs::create_dir(site.root.join("a")).unwrap();
    fs::write(site.root.join("a/kept.txt"), b"kept on b").unwrap();
    b.scan_until(|b| b.folder.engine.trees.local().len() == 2);
    site.stored();
    drop((pending, b));

    let mut b = site.start();
    let stored = site.stored();
    assert!(stored.effects.in_flight().is_none() && stored.effects.blocked().is_empty());
    assert_eq!(stored.trees.local().len(), 2);
    drop(b);
    b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    let held = entries(&site.root);
    let texts: Vec<&str> = held
        .iter()
        .filter_map(|(_, text)| text.as_deref())
        .collect();
    assert_eq!(texts, ["kept on b"], "{held:?}");
}

/// Device `a` synced `notes.txt` and stopped with its second commit built and uploaded but never sent. Returns its
/// site, which keeps the store, and that commit.
fn stopped_with_a_commit_in_flight(remote: &mut Remote) -> (Site, Commit) {
    let (site, mut a) = linked(remote, "a");
    a.create(remote, &site.root.join("notes.txt"), b"written on a");
    fs::write(site.root.join("draft.txt"), b"never sent").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    assert_eq!(commit.device_seq, 2);
    (site, commit)
}

fn kinds(commit: &Commit) -> Vec<&'static str> {
    (commit.ops.iter())
        .map(|op: &Op| match &op.op {
            Some(op::Op::CreateFile(_)) => "create file",
            Some(op::Op::UpdateContent(_)) => "update content",
            Some(op::Op::Restore(_)) => "restore",
            Some(op::Op::Delete(_)) => "delete",
            _ => "another op",
        })
        .collect()
}

#[test]
fn a_new_file_becomes_one_commit_one_upload_and_one_send() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    assert_eq!(a.state(), FolderState::UpToDate);

    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.state(), FolderState::CatchingUp);
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    a.settle();
    assert!(a.work.is_empty());

    let [commit] = a.sent.as_slice() else {
        panic!("one commit, not {}", a.sent.len());
    };
    assert_eq!(kinds(commit), ["create file"]);
    assert_eq!(commit.receipts.len(), 1);
    assert_eq!(remote.names(), ["notes.txt"]);
    let trees = &a.folder.engine.trees;
    let (id, local) = trees.local().iter().next().expect("L holds the file");
    assert_eq!(trees.synced().get(id), Some(&local.shape));
    assert_eq!(
        trees.remote().get(id).map(|node| &node.shape),
        Some(&local.shape)
    );
    assert!(trees.dirty().is_empty());
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().last_seq, Some(1));
}

#[test]
fn a_commit_result_is_acknowledged_once_whether_the_call_or_the_stream_brings_it_first() {
    for call_first in [true, false] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        let result = remote.commit(&a.caller, &commit).unwrap();
        let by_stream = Event::Committed(Committed {
            commit_id: commit.commit_id.clone(),
            result: result.clone(),
        });
        if call_first {
            let step = a.folder.sent(Ok(result));
            a.take(step).unwrap();
            a.event(by_stream).unwrap();
        } else {
            a.event(by_stream).unwrap();
            let step = a.folder.sent(Ok(result));
            a.take(step).unwrap();
        }
        assert!(a.work.is_empty(), "call first: {call_first}");
        assert!(a.folder.engine.commits.in_flight().is_none());
        assert_eq!(a.state(), FolderState::UpToDate);

        a.create(&mut remote, &site.root.join("more.txt"), b"the next commit");
        assert_eq!(a.sent.last().unwrap().device_seq, 2);
        assert_eq!(remote.space.head_seq(), 2, "call first: {call_first}");
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_remote_file_becomes_one_effect_and_its_observation_changes_nothing() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    // The effect's acknowledgement settled the trees in its own transaction, so S holds the file as L does before
    // anything is observed or arrives from the log. Otherwise the next change of the file would count as concurrent.
    let folder = &mut b.folder;
    let (id, local) = (folder.engine.trees.local().iter().next()).expect("L holds the file");
    assert_eq!(folder.engine.trees.synced().get(id), Some(&local.shape));
    let stored = (folder.store)
        .transaction(|tx| Engine::load(tx, &folder.link.space_id))
        .unwrap();
    assert_eq!(stored.trees.synced().get(id), Some(&local.shape));
    b.settle();
    assert!(b.work.is_empty());
    assert!(b.sent.is_empty());
    assert!(b.folder.engine.trees.dirty().is_empty());
    assert_eq!(b.folder.engine.effects.cleared(), 1);
    assert_eq!(b.state(), FolderState::UpToDate);
    assert_eq!(remote.space.head_seq(), 1);
}

#[test]
fn a_download_the_server_puts_off_for_an_hour_shows_in_the_status_until_it_goes_through() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.refusal.lock().unwrap() = Some(ServerError(flarebox_proto::v1::Error {
        code: flarebox_proto::v1::error::Code::QuotaExceeded as i32,
        message: "the daily limit is used up".into(),
        retry_after_ms: 3_600_000,
        ..Default::default()
    }));
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    let problem = b
        .folder
        .view()
        .problem
        .expect("the wait shows in the status");
    assert!(problem.contains("QUOTA_EXCEEDED"), "{problem}");

    *remote.chunks.refusal.lock().unwrap() = None;
    assert_eq!(b.run(&mut remote), ["stage again"]);
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
}

#[test]
fn a_download_that_fails_for_now_stages_the_same_effect_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    *remote.chunks.unavailable.lock().unwrap() = 1;
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    let planned = b
        .folder
        .engine
        .effects
        .in_flight()
        .unwrap()
        .effect_id
        .clone();
    let staged = ready(pending.stage(&remote.chunks));
    assert!(matches!(staged, Err(Stall::Later { .. })));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    b.settle();
    assert!(matches!(b.work.make_contiguous(), [Work::StageAfter(..)]));
    assert_eq!(
        b.folder.engine.effects.in_flight().unwrap().effect_id,
        planned
    );

    assert_eq!(b.run(&mut remote), ["stage again"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    assert_eq!(b.folder.engine.effects.cleared(), 1);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_local_change_is_committed_only_after_the_effect_in_flight() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    let pending = loop {
        match b.work.pop_front().expect("an effect is staged") {
            Work::Stage(pending) => break pending,
            other => b.perform(other, &mut remote).unwrap(),
        }
    };
    fs::write(site.root.join("mine.txt"), b"written on b").unwrap();
    b.scan_until(|b| b.folder.engine.trees.local().len() == 1);
    assert!(
        b.work.is_empty(),
        "no commit is built while the effect is staged"
    );
    assert!(b.folder.engine.commits.in_flight().is_none());

    b.work.push_back(Work::Stage(pending));
    assert_eq!(b.run(&mut remote), ["stage", "upload", "send"]);
    b.settle();
    assert_eq!(names(&site.root), ["mine.txt", "notes.txt"]);
    assert_eq!(remote.names(), ["mine.txt", "notes.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_remote_change_is_applied_only_after_the_commit_in_flight() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let (site_b, mut b) = linked(&mut remote, "b");
    fs::write(site.root.join("mine.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(send) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };

    b.create(&mut remote, &site_b.root.join("notes.txt"), b"written on b");
    a.pull(&remote);
    assert_eq!(a.folder.engine.trees.remote().len(), 1, "R holds b's file");
    assert!(
        a.work.is_empty(),
        "no effect is planned while the commit is in flight"
    );
    assert!(a.folder.engine.effects.in_flight().is_none());

    a.work.push_back(send);
    let ran = a.run(&mut remote);
    assert_eq!(ran.last(), Some(&"stage"), "{ran:?}");
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(names(&site.root), ["mine.txt", "notes.txt"]);
    assert_eq!(remote.names(), ["mine.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_tree_listing_put_off_for_an_hour_shows_in_the_status_and_one_put_off_for_a_second_does_not() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert!(matches!(b.work.pop_front(), Some(Work::List { .. })));

    let later = |wait, why: &str| Listed::Later {
        listing: Listing::default(),
        wait,
        why: format!("reading the space's tree failed: {why}"),
    };
    let step = b
        .folder
        .listed(Ok(later(Duration::from_secs(1), "RETRY_LATER: busy")));
    b.take(step).unwrap();
    assert_eq!(b.folder.view().problem, None);
    assert!(matches!(b.work.pop_front(), Some(Work::ListAfter(..))));

    let quota = "QUOTA_EXCEEDED: the daily limit is used up";
    let step = b
        .folder
        .listed(Ok(later(Duration::from_secs(3_600), quota)));
    b.take(step).unwrap();
    let problem = b
        .folder
        .view()
        .problem
        .expect("an hour's wait shows in the status");
    assert!(problem.contains(quota), "{problem}");
    assert!(problem.ends_with(" UTC"), "{problem}");

    b.run(&mut remote);
    assert_eq!(b.folder.view().problem, None);
    assert_eq!(names(&site.root), ["one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// A refusal of the server that puts a call off for an hour.
fn put_off(code: flarebox_proto::v1::error::Code) -> ServerError {
    ServerError(flarebox_proto::v1::Error {
        code: code as i32,
        message: "not now".into(),
        retry_after_ms: 3_600_000,
        ..Default::default()
    })
}

#[test]
fn an_upload_put_off_for_an_hour_leaves_the_status_once_its_file_is_deleted() {
    use flarebox_proto::v1::error::Code;
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    fs::write(&path, b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the file is uploaded");
    };
    let hash = upload.chunks[0].hash.clone();
    let refused = ChunkError::Call(CallError::Server(put_off(Code::QuotaExceeded)));
    let step = a.folder.uploaded(Err(refused));
    a.take(step).unwrap();
    assert!(
        a.folder.view().problem.is_some(),
        "the wait shows in the status"
    );

    fs::remove_file(&path).unwrap();
    a.scan_until(|a| a.folder.engine.trees.local().is_empty());
    let wait = a.work.pop_front().unwrap();
    a.perform(wait, &mut remote).unwrap();
    assert!(matches!(a.work.pop_front(), Some(Work::Upload(_))));
    let step = a.folder.uploaded(Err(ChunkError::SourceChanged { hash }));
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().problem, None);
}

#[test]
fn a_commit_put_off_for_an_hour_leaves_the_status_once_the_server_refuses_it() {
    use flarebox_proto::v1::error::Code;
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::RetryLater))));
    a.take(step).unwrap();
    assert!(
        a.folder.view().problem.is_some(),
        "the wait shows in the status"
    );

    let wait = a.work.pop_front().unwrap();
    a.perform(wait, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent again");
    };
    let step = a
        .folder
        .sent(Err(CallError::Server(put_off(Code::HeadMoved))));
    a.take(step).unwrap();
    assert_eq!(a.folder.view().problem, None);
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_bootstrap_whose_pages_were_read_at_two_heads_replaces_r_after_the_replay() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let first = remote.space.list_tree(&ListTree {
        cursor: String::new(),
        limit: 1,
    });
    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    let mut pages = vec![first];
    while !pages.last().unwrap().next_cursor.is_empty() {
        let cursor = pages.last().unwrap().next_cursor.clone();
        pages.push(remote.space.list_tree(&ListTree { cursor, limit: 1 }));
    }
    let (first_head, last_head) = (pages[0].head_seq, pages.last().unwrap().head_seq);
    assert!(first_head < last_head);

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert!(matches!(b.work.pop_front(), Some(Work::List { .. })));
    let step = b.folder.listed(Ok(Listed::Done {
        pages,
        trash: vec![],
    }));
    b.take(step).unwrap();
    assert!(matches!(
        b.work.front(),
        Some(Work::Subscribe { since_seq, .. }) if *since_seq == first_head
    ));
    b.event(Event::CaughtUp {
        head_seq: first_head,
    })
    .unwrap();
    assert_eq!(b.state(), FolderState::Bootstrapping);
    assert_eq!(b.folder.engine.cursor, None);
    assert!(b.folder.engine.trees.remote().is_empty());

    assert_eq!(b.run(&mut remote), ["subscribe", "stage", "stage"]);
    assert_eq!(b.folder.view().last_seq, Some(last_head));
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_truncated_log_reads_the_tree_again_and_keeps_what_is_synced() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["one.txt"]);

    a.create(&mut remote, &site_a.root.join("two.txt"), b"two");
    a.create(&mut remote, &site_a.root.join("three.txt"), b"three");
    remote.space.truncate_log(remote.space.head_seq());
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage", "stage"]);
    assert_eq!(names(&site.root), ["one.txt", "three.txt", "two.txt"]);
    assert_eq!(b.folder.engine.effects.cleared(), 3);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn an_edit_after_a_restart_is_an_edit_of_the_synced_file() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    a.settle();
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["subscribe"]);
    a.settle();
    assert!(a.work.is_empty(), "a restart finds nothing to sync");
    assert_eq!(a.state(), FolderState::UpToDate);

    a.create(
        &mut remote,
        &site.root.join("notes.txt"),
        b"edited after the restart",
    );
    assert_eq!(kinds(&a.sent[0]), ["update content"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// Design §2.7 leaves a node the server refused an op for out of commits until the next rescan. The first whole scan
/// of a start is that rescan (GUN-345). Nothing cleared the node before, so it never synced again.
#[test]
fn a_node_the_server_refused_is_committed_again_after_the_scan_of_the_next_start() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    assert_eq!(kinds(&commit), ["create file"]);
    let refusal = flarebox_proto::v1::Error {
        code: Code::LimitExceeded as i32,
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);

    // Neither an edit of the file nor the scans of the running folder bring the node back.
    fs::write(site.root.join("notes.txt"), b"edited on a").unwrap();
    a.settle();
    assert!(a.work.is_empty(), "the refused node stays out of commits");
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert!(remote.names().is_empty());
    drop(a);

    let mut a = site.start();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert!(a.folder.engine.commits.problems().is_empty());
    assert_eq!(a.run(&mut remote), ["subscribe", "upload", "send"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// Device `a` wrote `notes.txt`, and device `b` staged the effect that writes it. A file of the user's then holds the name
/// on `b`, which no batch has reported, so the effect fails with the name taken and the node is blocked. The user's
/// file is left as it was.
fn failed_at_a_taken_name(remote: &mut Remote) -> (Site, Device, Duration) {
    let (site_a, mut a) = linked(remote, "a");
    let (site, mut b) = linked(remote, "b");
    a.create(remote, &site_a.root.join("notes.txt"), b"written on a");
    b.pull(remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes notes.txt is staged");
    };
    let wait = take_the_name_and_apply(remote, &site, &mut b, pending);
    (site, b, wait)
}

/// Puts a file of the user's at `notes.txt`, applies `pending` and returns the wait before its node is unblocked.
fn take_the_name_and_apply(
    remote: &Remote,
    site: &Site,
    b: &mut Device,
    pending: Pending,
) -> Duration {
    fs::write(site.root.join("notes.txt"), b"the user's").unwrap();
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"the user's"
    );
    let Some(Work::RetryAfter(wait)) = b.work.pop_front() else {
        panic!("the node is unblocked after a wait: {:?}", b.work.len());
    };
    assert!(b.work.is_empty());
    wait
}

/// GUN-318. An effect that failed was never planned again while the engine planned the same effect, so a name that
/// was taken for a moment kept the node from syncing for good.
#[test]
fn an_effect_that_failed_at_a_name_taken_for_a_moment_is_planned_again_after_a_wait() {
    let mut remote = Remote::new();
    let (site, mut b, first) = failed_at_a_taken_name(&mut remote);
    assert!(first <= Duration::from_secs(1), "{first:?}");

    // The user's file is gone before the observer saw it, so L is as it was and the engine plans the same effect.
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    b.settle();
    assert!(b.work.is_empty(), "the node waits");
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);

    // The name is taken again when the effect is retried, and the next wait is longer.
    let step = b.folder.retry();
    b.take(step).unwrap();
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the same effect is planned again");
    };
    let second = take_the_name_and_apply(&remote, &site, &mut b, pending);
    assert!(second >= Duration::from_secs(1), "{second:?}");

    fs::remove_file(site.root.join("notes.txt")).unwrap();
    let step = b.folder.retry();
    b.take(step).unwrap();
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    assert!(b.folder.engine.effects.blocked().is_empty());
    b.settle();
    assert!(b.work.is_empty());
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The block was stored, so it outlived the process. A start plans the effect of every blocked node again.
#[test]
fn a_start_plans_an_effect_that_failed_before_it_again() {
    let mut remote = Remote::new();
    let (site, b, _) = failed_at_a_taken_name(&mut remote);
    drop(b);
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    assert_eq!(site.stored().effects.blocked().len(), 1);

    let mut b = site.start();
    assert!(b.folder.engine.effects.blocked().is_empty());
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("notes.txt")).unwrap(),
        b"written on a"
    );
    b.settle();
    assert!(b.work.is_empty());
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_restart_with_a_commit_in_flight_sends_the_stored_commit_again() {
    for reached_the_server in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        if reached_the_server {
            remote.commit(&a.caller, &commit).unwrap();
        }
        drop(a);

        let mut a = site.start();
        a.connect(&remote);
        let stored = Commit {
            receipts: Vec::new(),
            ..commit
        };
        assert!(
            matches!(a.work.front(), Some(Work::Send(again)) if *again == stored),
            "reached the server: {reached_the_server}"
        );
        a.scan_until(|a| a.folder.scanned);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty());
        assert!(a.sent.iter().all(|sent| sent.commit_id == stored.commit_id));
        assert_eq!(remote.space.head_seq(), 1);
        assert_eq!(remote.names(), ["notes.txt"]);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_file_that_changes_while_it_is_uploaded_is_committed_with_its_new_bytes() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    fs::write(&path, b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(Work::Upload(upload)) = a.work.pop_front() else {
        panic!("the content is uploaded first");
    };
    fs::write(
        &path,
        b"and written again, longer, before the upload read it",
    )
    .unwrap();
    let chunk = upload.chunks[0].clone();
    assert!(!flarebox_net::matches(
        &chunk,
        &upload.read(&chunk).unwrap()
    ));
    let step = a
        .folder
        .uploaded(Err(ChunkError::SourceChanged { hash: chunk.hash }));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert!(matches!(a.work.make_contiguous(), [Work::ResendAfter(_)]));

    let size = |tree: &Tree<LocalNode>| {
        (tree.iter()).find_map(|(_, node)| node.shape.content.as_ref().map(|content| content.size))
    };
    let on_disk = Some(fs::metadata(&path).unwrap().len());
    a.scan_until(|a| size(a.folder.engine.trees.local()) == on_disk);
    assert_eq!(a.run(&mut remote), ["wait", "upload", "send"]);
    a.settle();
    assert!(a.work.is_empty());
    let in_r = (a.folder.engine.trees.remote().iter())
        .find_map(|(_, node)| node.shape.content.as_ref().map(|content| content.size));
    assert_eq!(in_r, on_disk);
    assert_eq!(remote.space.head_seq(), 1);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_first_link_stops_when_the_folder_and_the_space_hold_the_same_name() {
    // The space's name and the folder's: in another case, in another Unicode form, and where case folding changes
    // the name's length.
    for (theirs, mine) in [
        ("notes.txt", "Notes.txt"),
        ("r\u{e9}sum\u{e9}.txt", "re\u{301}sume\u{301}.txt"),
        ("stra\u{df}e.txt", "STRASSE.txt"),
    ] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join(theirs), b"written on a");

        let site = Site::new(&remote, "b");
        fs::write(site.root.join(mine), b"already here").unwrap();
        fs::write(site.root.join("mine.txt"), b"mine").unwrap();
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        let stop = b.try_run(&mut remote).unwrap_err();
        assert!(matches!(stop, Stop::Unsupported(_)), "{mine}: {stop}");
        assert!(b.sent.is_empty(), "{mine}");
        let mut kept = [
            (mine.to_owned(), "already here".to_owned()),
            ("mine.txt".to_owned(), "mine".to_owned()),
        ];
        kept.sort();
        assert_eq!(files(&site.root), kept);
        assert_eq!(remote.names(), [theirs]);
    }
}

#[test]
fn a_first_link_of_a_folder_and_a_space_that_share_no_name_syncs_both_and_overwrites_nothing() {
    let mut remote = Remote::new();
    let site_a = Site::new(&remote, "a");
    fs::write(site_a.root.join("notes.txt"), b"written on a").unwrap();
    fs::create_dir(site_a.root.join("shared")).unwrap();
    fs::write(site_a.root.join("shared/plan.txt"), b"a plan from a").unwrap();
    let mut a = site_a.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["list", "subscribe", "upload", "send"]);

    let site = Site::new(&remote, "b");
    fs::write(site.root.join("mine.txt"), b"already here").unwrap();
    fs::create_dir(site.root.join("drafts")).unwrap();
    fs::write(site.root.join("drafts/plan.txt"), b"a plan from b").unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    let ran = b.run(&mut remote);
    assert_eq!(ran[..2], ["list", "subscribe"]);
    assert_eq!(ran[ran.len() - 2..], ["upload", "send"]);
    b.settle();
    assert!(b.work.is_empty());
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());

    let both = [
        ("drafts/plan.txt", "a plan from b"),
        ("mine.txt", "already here"),
        ("notes.txt", "written on a"),
        ("shared/plan.txt", "a plan from a"),
    ]
    .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), both);
    assert_eq!(files(&site_a.root), both);
    assert_eq!(remote.space.head_seq(), 2);
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_new_store_on_a_device_that_synced_the_space_before_goes_on_from_the_servers_count() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    a.scan_until(|a| a.folder.scanned);
    assert_eq!(a.run(&mut remote), ["list", "subscribe", "stage"]);
    a.create(
        &mut remote,
        &again.root.join("more.txt"),
        b"from the folder linked again",
    );
    assert_eq!(a.sent[0].device_seq, 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_never_synced_store_takes_the_servers_count_only_at_its_first_welcome() {
    let remote = Remote::new();
    let site = Site::new(&remote, "a");
    let mut a = site.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 0);

    let welcome = Welcome {
        device_seq: 1,
        ..remote.space.welcome(&a.caller)
    };
    let stop = a.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(a.folder.engine.commits.accepted(), 0);
}

#[test]
fn a_store_that_took_the_servers_count_does_not_take_another() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    drop(a);

    let again = Site::new(&remote, "a");
    let mut a = again.start();
    a.connect(&remote);
    assert_eq!(a.folder.engine.commits.accepted(), 1);
    assert_eq!(a.folder.engine.cursor, None);
    drop(a);

    let mut a = again.start();
    let welcome = Welcome {
        device_seq: 2,
        ..remote.space.welcome(&a.caller)
    };
    let stop = a.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(a.folder.engine.commits.accepted(), 1);
}

#[test]
fn a_synced_store_stops_for_recovery_when_the_server_counts_a_commit_it_does_not_hold() {
    let mut remote = Remote::new();
    // The old store read the space and committed nothing: it holds a cursor, and its count is 0 like a new store's.
    let (old, a) = linked(&mut remote, "a");
    assert!(a.folder.engine.cursor.is_some());
    drop(a);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 1);
    let stop = stale.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(stale.folder.engine.commits.accepted(), 0);
    assert!(stale.work.is_empty());
}

#[test]
fn a_store_with_a_commit_in_flight_stops_for_recovery_when_the_server_counts_past_its_slot() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    a.create(&mut remote, &new.root.join("again.txt"), b"and once more");
    drop(a);

    let mut stale = old.start();
    let welcome = remote.space.welcome(&stale.caller);
    assert_eq!(welcome.device_seq, 3);
    let stop = stale.event(Event::Connected(welcome)).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert!(stale.work.is_empty(), "the commit is not sent again");
    let commits = &stale.folder.engine.commits;
    assert_eq!(commits.accepted(), 1);
    assert_eq!(
        commits.in_flight().unwrap().commit.commit_id,
        commit.commit_id
    );
}

#[test]
fn a_commit_from_the_store_a_new_store_replaced_is_refused_by_the_server() {
    let mut remote = Remote::new();
    let (old, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let (new, mut a) = linked(&mut remote, "a");
    a.create(
        &mut remote,
        &new.root.join("more.txt"),
        b"from the new store",
    );
    assert_eq!(a.sent[0].device_seq, commit.device_seq);
    drop(a);

    // The server counts one commit more than the old store acknowledged, which is what a lost response looks like,
    // so the old store sends its commit again. The slot is the new store's by now.
    let mut stale = old.start();
    stale.connect(&remote);
    let Some(Work::Send(again)) = stale.work.pop_front() else {
        panic!("the stored commit is sent again");
    };
    assert_eq!(again.commit_id, commit.commit_id);
    let refusal = remote.commit(&stale.caller, &again).unwrap_err();
    assert!(
        matches!(&refusal, CallError::Server(error) if error.code() == Code::StaleDeviceState),
        "{refusal}"
    );
    let step = stale.folder.sent(Err(refusal));
    let stop = stale.take(step).unwrap_err();
    assert!(matches!(stop, Stop::Recovery(_)), "{stop}");
    assert_eq!(
        stale.folder.engine.commits.halted(),
        Some(CommitHalt::Recovery)
    );
    assert_eq!(remote.space.head_seq(), 2);
    assert_eq!(remote.names(), ["more.txt", "notes.txt"]);
}

/// Records a restore request in the site's store, as `Driver::restore` does from its own connection.
fn ask(site: &Site, intent: &RestoreIntent) -> String {
    let id = crate::restore::new_id(NOW as u64);
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| tx.request_restore(&id, intent))
        .unwrap();
    id
}

/// The node R holds under `name`.
fn node_named(device: &Device, name: &str) -> String {
    let remote = device.folder.engine.trees.remote();
    let (id, _) = (remote.iter())
        .find(|(_, node)| node.shape.name == name)
        .expect("R holds the node");
    id.clone()
}

/// The restores that ended, as the site's store holds them. Nothing may wait in the inbox or in the engine.
fn ended(site: &Site) -> Vec<(String, Outcome)> {
    let mut store = Store::open(&site.db).unwrap();
    let (inbox, (queued, resolved)) = store
        .transaction(|tx| Ok::<_, flarebox_store::Error>((tx.restore_requests()?, tx.restores()?)))
        .unwrap();
    assert!(inbox.is_empty() && queued.is_empty());
    (resolved.into_iter())
        .map(|ended| (ended.id, ended.outcome))
        .collect()
}

/// Device `a` synced `notes.txt` and then deleted it, so the space holds the file in its trash. Returns the file's
/// node.
fn with_a_trashed_file(remote: &mut Remote) -> (Site, Device, String) {
    let (site, mut a) = linked(remote, "a");
    let path = site.root.join("notes.txt");
    a.create(remote, &path, b"written on a");
    let node = node_named(&a, "notes.txt");
    fs::remove_file(&path).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(remote), ["send"]);
    assert!(remote.names().is_empty());
    (site, a, node)
}

fn notes() -> Vec<(String, String)> {
    vec![("notes.txt".to_owned(), "written on a".to_owned())]
}

#[test]
fn a_restore_asked_for_brings_a_trashed_file_back_into_the_folder() {
    let mut remote = Remote::new();
    let (site, mut a, node) = with_a_trashed_file(&mut remote);

    let intent = RestoreIntent::from_trash(node.clone(), None, None).unwrap();
    let id = ask(&site, &intent);
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send", "stage"]);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(files(&site.root), notes());
    assert_eq!(node_named(&a, "notes.txt"), node);
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);

    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), 3);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_restore_asked_for_while_the_folder_was_stopped_is_sent_after_the_next_start() {
    let mut remote = Remote::new();
    let (site, a, node) = with_a_trashed_file(&mut remote);
    drop(a);
    let id = ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());

    let mut a = site.start();
    let queued: Vec<&String> = a.folder.engine.commits.restores().keys().collect();
    assert_eq!(queued, [&id]);
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(files(&site.root), notes());
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);
}

/// The folder stops at each point between the request and its result (design §2.8). After the next start the
/// space has applied the restore once and the folder holds the file.
#[test]
fn a_restore_is_applied_once_wherever_the_folder_stops() {
    for cut in ["asked", "taken", "applied", "answered"] {
        let mut remote = Remote::new();
        let (site, mut a, node) = with_a_trashed_file(&mut remote);
        let id = ask(&site, &RestoreIntent::from_trash(node, None, None).unwrap());
        if cut != "asked" {
            let step = a.folder.requested();
            a.take(step).unwrap();
            let Some(Work::Send(commit)) = a.work.pop_front() else {
                panic!("a restore alone is sent without an upload");
            };
            if cut != "taken" {
                let result = remote.commit(&a.caller, &commit);
                if cut == "answered" {
                    let step = a.folder.sent(result);
                    a.take(step).unwrap();
                }
            }
        }
        drop(a);

        let mut a = site.start();
        a.connect(&remote);
        a.scan_until(|a| a.folder.scanned);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty(), "{cut}");
        assert_eq!(files(&site.root), notes(), "{cut}");
        assert_eq!(ended(&site), [(id, Outcome::Restored)], "{cut}");
        // The create, the delete and one restore.
        assert_eq!(remote.space.head_seq(), 3, "{cut}");
        assert_eq!(a.state(), FolderState::UpToDate, "{cut}");
    }
}

#[test]
fn a_restore_of_an_earlier_version_rewrites_the_file_in_the_folder() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let path = site.root.join("notes.txt");
    a.create(&mut remote, &path, b"the first words");
    fs::write(&path, b"and the second").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["upload", "send"]);
    let node = node_named(&a, "notes.txt");
    let request = ListVersions {
        node_id: node.clone(),
        ..Default::default()
    };
    let versions = remote.space.list_versions(&request).unwrap().versions;
    assert_eq!(versions.len(), 2);
    let first = versions[1].version_id.clone();

    let id = ask(&site, &RestoreIntent::version(node, first).unwrap());
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send", "stage"]);
    assert_eq!(kinds(a.sent.last().unwrap()), ["restore"]);
    assert_eq!(
        files(&site.root),
        [("notes.txt".to_owned(), "the first words".to_owned())]
    );
    assert_eq!(ended(&site), [(id, Outcome::Restored)]);

    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), 3);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_folder_with_a_queued_restore_is_not_up_to_date() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    assert_eq!(a.state(), FolderState::UpToDate);

    let node = node_named(&a, "notes.txt");
    let intent = RestoreIntent::from_trash(node, None, None).unwrap();
    let commits = &mut a.folder.engine.commits;
    assert!(commits.restore(crate::restore::new_id(NOW as u64), intent));
    assert_eq!(a.state(), FolderState::CatchingUp);
}

#[test]
fn the_device_list_narrows_what_is_downloaded_and_what_is_uploaded() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"notes");
    a.create(&mut remote, &site_a.root.join("debug.log"), b"log of a");

    let sync = SyncRules {
        include: Vec::new(),
        exclude: vec!["*.log".into()],
    };
    let site = Site::new(&remote, "b");
    let mut b = site.start_with(&sync);
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), ["notes.txt"]);

    fs::write(site.root.join("mine.log"), b"log of b").unwrap();
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(remote.names(), ["debug.log", "notes.txt"]);
    assert_eq!(
        b.state(),
        FolderState::UpToDate,
        "what the device leaves out does not keep the folder catching up"
    );
}

#[test]
fn a_downloaded_rules_file_is_in_force_before_anything_else_is_planned() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".fbignore"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), [".fbignore"]);
    assert!(b.folder.disk.awaits_rules());
    assert_eq!(b.state(), FolderState::CatchingUp);

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(
        b.run(&mut remote).is_empty(),
        "the rules leave cache.tmp out"
    );
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(names(&site.root), [".fbignore"]);
    assert_eq!(remote.names(), [".fbignore", "cache.tmp"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// After a restart the scan cannot read a rules file over the size limit, and no earlier scan of this run read it, so
/// what it leaves out is unknown. Nothing below its folder is planned: the excluded log keeps this device's bytes, and
/// the new file waits too. Once the file can be read, the log stays out and the new file arrives.
#[test]
fn a_rules_file_a_restart_cannot_read_holds_everything_below_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    a.create(&mut remote, &site_a.root.join(".fbignore"), b"*.log\n");
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    drop(b);
    fs::write(site.root.join(".fbignore"), "*.log\n".repeat(200_000)).unwrap();
    a.create(&mut remote, &site_a.root.join("notes.txt"), b"notes");
    c.create(&mut remote, &site_c.root.join("build.log"), b"v2 from c");

    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["subscribe"]);
    assert_eq!(fs::read(site.root.join("build.log")).unwrap(), b"v1");
    assert_eq!(names(&site.root), [".fbignore", "build.log"]);

    fs::write(site.root.join(".fbignore"), "*.log\n").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.settle();
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(names(&site.root), [".fbignore", "build.log", "notes.txt"]);
    assert_eq!(fs::read(site.root.join("build.log")).unwrap(), b"v1");
    assert!(b.work.is_empty() && b.sent.is_empty());
}

/// A restart cannot read the rules file of `proj`, so what is below it is held. The user renames the folder to `PROJ`
/// and fixes the file, and the rules are known again under the new spelling. Renamed back, the folder is not held:
/// the hold is by `name_key`, as the rules are, so a later remote edit arrives.
#[test]
fn the_rules_hold_of_a_folder_ends_also_after_it_was_renamed_only_in_case() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(&mut remote, &site_a.root.join("proj/.fbignore"), b"*.log\n");
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.fbignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::rename(site.root.join("proj"), site.root.join("PROJ")).unwrap();
    fs::write(site.root.join("PROJ/.fbignore"), b"*.log\n").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    fs::rename(site.root.join("PROJ"), site.root.join("proj")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

#[test]
fn review_removing_unknown_rules_while_renaming_the_folder_does_not_hold_remote_updates_forever() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(&mut remote, &site_a.root.join("proj/.fbignore"), b"*.log\n");
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.fbignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::rename(site.root.join("proj"), site.root.join("PROJ")).unwrap();
    fs::remove_file(site.root.join("PROJ/.fbignore")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    fs::rename(site.root.join("PROJ"), site.root.join("proj")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

/// A restart cannot read the rules file of `proj`, so what is below it is held. The user deletes the file. No rules
/// were known for the folder, so no rules changed, but the scan that lists the folder without a rules file ends the
/// hold, and a later remote edit arrives.
#[test]
fn deleting_a_rules_file_a_restart_cannot_read_ends_the_hold_of_its_folder() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/file.txt"), b"v1");
    a.create(&mut remote, &site_a.root.join("proj/.fbignore"), b"*.log\n");
    let (site, mut b) = linked(&mut remote, "b");
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    b.run(&mut remote);
    drop(b);
    fs::write(site.root.join("proj/.fbignore"), "*.log\n".repeat(200_000)).unwrap();
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    b.run(&mut remote);
    fs::remove_file(site.root.join("proj/.fbignore")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    a.create(
        &mut remote,
        &site_a.root.join("proj/file.txt"),
        b"v2 from a",
    );
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        fs::read(site.root.join("proj/file.txt")).unwrap(),
        b"v2 from a"
    );
}

/// An effect creates the folder `b` from the server. The user then moves `a` into it and puts a rules file that cannot
/// be read in it. The batch that reports the move began before the effect ended, so its move is dropped and both paths
/// are read again. The removals the scan reported below `a` belong to the move and are dropped with it, so nothing
/// below the folder is deleted on the server.
#[test]
fn removals_below_a_folder_move_that_is_dropped_are_dropped_with_it() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("a")).unwrap();
    a.create(&mut remote, &site_a.root.join("a/x"), b"x");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("b")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.scan_until(|b| (b.observer.snapshot().entries).contains_key(Path::new("a/x")));
    let id = b.observer.snapshot().entries[Path::new("a")].id;
    let sections = b.folder.sections();
    let stamp = sections.stamp();
    b.pull(&remote);
    b.run(&mut remote);
    assert!(site.root.join("b").is_dir());
    fs::rename(site.root.join("a"), site.root.join("b/a")).unwrap();
    fs::write(site.root.join("b/a/.fbignore"), "*.log\n".repeat(200_000)).unwrap();
    let stale = Batch {
        stamp,
        answered: None,
        observation: crate::observe::Observation {
            changes: vec![
                Observed::Removed {
                    path: "a/x".into(),
                    kind: EntryKind::File,
                },
                Observed::Renamed {
                    from: "a".into(),
                    to: "b/a".into(),
                    kind: EntryKind::Dir,
                    id,
                    linked: false,
                },
            ],
            root: Ok(()),
            unreadable: vec![PathBuf::from("b/a")],
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
        },
    };
    let step = b.folder.observed(stale, 1);
    b.take(step).unwrap();
    assert!(!b.run(&mut remote).contains(&"send"));
    b.settle();
    b.run(&mut remote);
    let live = (b.folder.engine.trees.remote().iter())
        .any(|(_, node)| !node.trashed && node.shape.name == "x");
    assert!(live, "x stays on the server");
}

/// A folder the user deletes stays on the server while it holds a file the rules exclude here (design §5.10), so
/// nothing is left to send for it, and the folder is up to date.
#[test]
fn a_deleted_folder_that_keeps_an_excluded_file_on_the_server_leaves_the_folder_up_to_date() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("d")).unwrap();
    a.create(&mut remote, &site.root.join("d/a.log"), b"log");
    a.create(&mut remote, &site.root.join("d/b.txt"), b"text");
    a.create(&mut remote, &site.root.join(".fbignore"), b"*.log\n");
    fs::remove_dir_all(site.root.join("d")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert!(a.run(&mut remote).is_empty());
    let mut live: Vec<&str> = (a.folder.engine.trees.remote().iter())
        .filter(|(_, node)| !node.trashed)
        .map(|(_, node)| node.shape.name.as_str())
        .collect();
    live.sort();
    assert_eq!(live, [".fbignore", "a.log", "d"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// Whether L, as the site's store holds it, has an entry named `name`.
fn local_holds(site: &Site, name: &str) -> bool {
    (site.stored().trees.local().iter()).any(|(_, node)| node.shape.name == name)
}

#[test]
fn a_batch_that_fails_the_root_check_stops_the_folder_before_l_loses_a_file() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");

    fs::remove_file(site.root.join(flarebox_fs::PRIVATE_DIR).join("space.json")).unwrap();
    fs::remove_file(site.root.join("notes.txt")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let stop = loop {
        assert!(Instant::now() < deadline, "the folder did not stop");
        if let Err(stop) = a.try_scan(Duration::from_millis(100)) {
            break stop;
        }
    };
    assert!(
        matches!(stop, Stop::Root(RootProblem::NoMarker)),
        "{stop:?}"
    );
    assert!(local_holds(&site, "notes.txt"));
    assert!(a.work.is_empty());
    assert_eq!(remote.names(), ["notes.txt"]);
}

#[test]
fn a_folder_the_scan_may_not_read_keeps_its_files_in_the_space() {
    use std::os::unix::fs::PermissionsExt;

    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    let folder = site.root.join("private");
    fs::create_dir(&folder).unwrap();
    a.create(&mut remote, &folder.join("secret.txt"), b"written on a");
    let sent = a.sent.len();

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read_dir(&folder).is_ok() {
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block reads");
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while !a
        .try_scan(Duration::from_millis(100))
        .unwrap()
        .contains(&"private".into())
    {
        assert!(
            Instant::now() < deadline,
            "the folder was not found unreadable"
        );
    }
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), sent);
    assert!(local_holds(&site, "secret.txt"));

    fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
    a.settle();
    assert!(a.work.is_empty());
    assert_eq!(a.sent.len(), sent);
    assert_eq!(remote.names(), ["private", "secret.txt"]);
}

#[test]
fn a_restart_before_the_upload_of_a_commit_sends_it_once_with_its_chunks() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert!(matches!(a.work.front(), Some(Work::Upload(_))));
    let built = a
        .folder
        .engine
        .commits
        .in_flight()
        .unwrap()
        .commit
        .commit_id
        .clone();
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    assert!(a.work.is_empty());
    assert!(a.sent.iter().all(|sent| sent.commit_id == built));
    assert_eq!(remote.space.head_seq(), 1);
    assert_eq!(remote.names(), ["notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_rules_file_named_in_another_case_is_downloaded_and_in_force_first() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".FBIGNORE"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert_eq!(names(&site.root), [".FBIGNORE"]);
    assert!(b.folder.disk.awaits_rules());

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(names(&site.root), [".FBIGNORE"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn planning_goes_on_after_a_folder_move_once_a_scan_that_found_nothing_is_reported() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("old")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["old"]);

    fs::rename(site_a.root.join("old"), site_a.root.join("new")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(names(&site.root), ["new"]);
    assert!(
        b.folder.disk.awaits_rules(),
        "the trees hold a folder's rules by its path"
    );
    assert_eq!(b.state(), FolderState::CatchingUp);

    a.create(&mut remote, &site_a.root.join("later.txt"), b"later");
    b.pull(&remote);
    assert!(b.work.is_empty(), "nothing is planned before the scan");

    let sections = b.folder.sections();
    let batch = (sections
        .observe(&mut b.observer, Duration::from_secs(10))
        .unwrap())
    .expect("a reset is answered");
    assert!(batch.observation.changes.is_empty() && batch.observation.rules.is_empty());
    let step = b.folder.observed(batch, 1);
    b.take(step).unwrap();
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert_eq!(names(&site.root), ["later.txt", "new"]);
}

#[test]
fn a_remote_edit_of_an_excluded_file_is_not_applied_while_its_folder_moves() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    b.settle();
    c.settle();

    a.create(&mut remote, &site_a.root.join("proj/.fbignore"), b"*.log\n");
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());

    fs::rename(site_a.root.join("proj"), site_a.root.join("work")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    c.create(
        &mut remote,
        &site_c.root.join("proj/build.log"),
        b"v2 from c",
    );

    b.pull(&remote);
    assert_eq!(
        b.run(&mut remote),
        ["stage"],
        "only the folder moves until its rules file is read at the new path"
    );
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    b.settle();
    assert!(b.work.is_empty() && b.sent.is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");
}

#[test]
fn a_batch_scanned_before_a_rules_file_was_written_does_not_end_the_wait_for_its_rules() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("cache.tmp"), b"cache");
    a.create(&mut remote, &site_a.root.join(".fbignore"), b"*.tmp\n");

    let site = Site::new(&remote, "b");
    let mut b = site.start();
    b.connect(&remote);
    b.scan_until(|b| b.folder.scanned);
    fs::write(site.root.join("mine.txt"), b"mine").unwrap();
    let sections = b.folder.sections();
    let deadline = Instant::now() + Duration::from_secs(10);
    let batch = loop {
        assert!(Instant::now() < deadline, "not observed in time");
        let wait = Duration::from_millis(100);
        if let Some(batch) = sections.observe(&mut b.observer, wait).unwrap() {
            break batch;
        }
    };
    assert_eq!(b.run(&mut remote), ["list", "subscribe", "stage"]);
    assert!(b.folder.disk.awaits_rules());

    let step = b.folder.observed(batch, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_rules(),
        "the batch was scanned before the rules file was there"
    );
    assert!(b.work.is_empty());

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert_eq!(b.run(&mut remote), ["upload", "send"]);
    assert_eq!(names(&site.root), [".fbignore", "mine.txt"]);
}

/// The observer's thread takes its resets, and the effect that moves the folder ends before the thread reads its
/// stamp. The batch that follows began after the effect, but its scans were never asked to read the moved folder.
#[test]
fn a_batch_that_was_not_asked_to_scan_a_moved_folder_does_not_end_the_wait_for_its_rules() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("proj")).unwrap();
    a.create(&mut remote, &site_a.root.join("proj/build.log"), b"v1");
    let (site, mut b) = linked(&mut remote, "b");
    let (site_c, mut c) = linked(&mut remote, "c");
    b.settle();
    c.settle();
    a.create(&mut remote, &site_a.root.join("proj/.fbignore"), b"*.log\n");
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    fs::rename(site_a.root.join("proj"), site_a.root.join("work")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    c.create(
        &mut remote,
        &site_c.root.join("proj/build.log"),
        b"v2 from c",
    );

    let sections = b.folder.sections();
    let (resets, requests) = sections.take_resets();
    assert!(resets.is_empty() && b.observer.answered());
    b.pull(&remote);
    assert_eq!(b.run(&mut remote), ["stage"]);
    assert!(b.folder.disk.awaits_rules());
    let unrelated = Batch {
        stamp: sections.stamp(),
        answered: Some(requests),
        observation: crate::observe::Observation {
            changes: Vec::new(),
            root: Ok(()),
            unreadable: vec![PathBuf::from("unrelated")],
            scanned: b.observer.scanned(),
            busy: Vec::new(),
            rules: Vec::new(),
            read: Vec::new(),
            all_rules: false,
        },
    };
    let step = b.folder.observed(unrelated, 1);
    b.take(step).unwrap();
    assert!(
        b.folder.disk.awaits_rules(),
        "no scan has read the rules file at the new path"
    );
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");

    b.scan_until(|b| !b.folder.disk.awaits_rules());
    assert!(b.run(&mut remote).is_empty());
    assert_eq!(fs::read(site.root.join("work/build.log")).unwrap(), b"v1");
}

#[test]
fn a_restart_during_a_bootstrap_reads_the_tree_again() {
    for listed in [false, true] {
        let mut remote = Remote::new();
        let (site_a, mut a) = linked(&mut remote, "a");
        a.create(&mut remote, &site_a.root.join("notes.txt"), b"written on a");

        let site = Site::new(&remote, "b");
        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        assert!(matches!(b.work.pop_front(), Some(Work::List { .. })));
        if listed {
            let step = b.folder.listed(Ok(Listed::Done {
                pages: remote.pages(TREE_PAGE),
                trash: vec![],
            }));
            b.take(step).unwrap();
            assert!(matches!(b.work.front(), Some(Work::Subscribe { .. })));
        }
        drop(b);

        let mut b = site.start();
        b.connect(&remote);
        b.scan_until(|b| b.folder.scanned);
        assert_eq!(
            b.run(&mut remote),
            ["list", "subscribe", "stage"],
            "listed: {listed}"
        );
        b.settle();
        assert!(b.work.is_empty());
        assert_eq!(files(&site.root), notes());
        assert_eq!(b.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_reconnect_while_a_commit_is_sent_starts_no_second_send() {
    for reached_the_server in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
        a.scan_until(|a| !a.work.is_empty());
        let upload = a.work.pop_front().unwrap();
        a.perform(upload, &mut remote).unwrap();
        let Some(Work::Send(commit)) = a.work.pop_front() else {
            panic!("the commit is sent after its upload");
        };
        if reached_the_server {
            remote.commit(&a.caller, &commit).unwrap();
        }

        a.event(Event::Disconnected {
            reason: flarebox_net::DisconnectReason::Timeout,
            retry_in: Duration::from_secs(1),
        })
        .unwrap();
        a.connect(&remote);
        assert!(
            !a.work.iter().any(|work| matches!(work, Work::Send(_))),
            "reached the server: {reached_the_server}"
        );

        let result = remote.commit(&a.caller, &commit).unwrap();
        let step = a.folder.sent(Ok(result));
        a.take(step).unwrap();
        a.pull(&remote);
        a.run(&mut remote);
        a.settle();
        assert!(a.work.is_empty());
        assert_eq!(remote.space.head_seq(), 1);
        assert_eq!(remote.names(), ["notes.txt"]);
        assert_eq!(a.state(), FolderState::UpToDate);
    }
}

#[test]
fn a_commit_too_large_to_send_is_split() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    for name in ["1.txt", "2.txt", "3.txt", "4.txt"] {
        fs::write(site.root.join(name), name).unwrap();
    }
    a.scan_until(|a| a.folder.engine.trees.local().iter().count() == 4 && !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    assert_eq!(commit.ops.len(), 4);
    let step = a.folder.sent(Err(CallError::TooLarge { bytes: 9 << 20 }));
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    let sizes: Vec<usize> = a.sent.iter().map(|commit| commit.ops.len()).collect();
    assert_eq!(sizes, [2, 2]);
    assert_eq!(remote.names(), ["1.txt", "2.txt", "3.txt", "4.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_read_only_device_keeps_downloading() {
    let mut remote = Remote::new();
    let (site_b, mut b) = linked(&mut remote, "b");
    let (site, mut a) = linked(&mut remote, "a");
    a.caller.role = Role::Viewer;
    a.connect(&remote);
    a.pull(&remote);
    assert_eq!(a.folder.engine.commits.halted(), Some(CommitHalt::ReadOnly));
    fs::write(site.root.join("mine.txt"), b"never sent").unwrap();
    a.scan_until(|a| {
        (a.folder.engine.trees.local().iter()).any(|(_, node)| node.shape.name == "mine.txt")
    });
    assert!(a.work.is_empty());

    b.create(
        &mut remote,
        &site_b.root.join("theirs.txt"),
        b"written on b",
    );
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    assert_eq!(
        files(&site.root),
        [
            ("mine.txt".to_owned(), "never sent".to_owned()),
            ("theirs.txt".to_owned(), "written on b".to_owned())
        ]
    );
    assert_eq!(remote.names(), ["theirs.txt"]);
}

/// A device whose first commit, of `notes.txt`, the server refused with `code`. Returns the folder, the refused
/// commit and why the task stopped. The device is gone, so its folder and store are free.
fn refused_first_commit(remote: &mut Remote, code: Code) -> (Site, Commit, String) {
    let (site, mut a) = linked(remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = ServerError(flarebox_proto::v1::Error {
        code: code as i32,
        message: "refused".into(),
        ..Default::default()
    });
    let step = a.folder.sent(Err(CallError::Server(refusal)));
    let stop = a.take(step).unwrap_err();
    (site, commit, stop.to_string())
}

/// Starts the folder again and runs what follows its `Welcome` and its first scan.
fn restarted(site: &Site, remote: &mut Remote) -> Device {
    let mut device = site.start();
    device.connect(remote);
    device.scan_until(|device| device.folder.scanned);
    device.run(remote);
    device
}

#[test]
fn a_refused_commit_lifted_to_resend_goes_out_unchanged() {
    let mut remote = Remote::new();
    let (site, refused, halt) = refused_first_commit(&mut remote, Code::Forbidden);
    assert!(halt.contains("flarebox lift"), "{halt}");
    let a = restarted(&site, &mut remote);
    assert!(a.sent.is_empty(), "a restart lifts nothing");
    let problem = a.folder.view().problem.unwrap();
    assert!(problem.contains("flarebox lift"), "{problem}");
    drop(a);

    lift_halt(&site.db, Lift::Resend).unwrap();
    let mut a = restarted(&site, &mut remote);
    assert_eq!(a.sent[0].commit_id, refused.commit_id);
    assert_eq!(remote.names(), ["notes.txt"]);
    a.settle();
    assert_eq!(a.state(), FolderState::UpToDate);
    assert_eq!(a.folder.view().problem, None);
}

#[test]
fn a_refused_commit_lifted_to_replan_goes_out_as_a_new_commit_in_its_slot() {
    let mut remote = Remote::new();
    let (site, refused, _) = refused_first_commit(&mut remote, Code::InvalidArgument);
    lift_halt(&site.db, Lift::Replan).unwrap();
    let mut a = restarted(&site, &mut remote);
    assert!(!a.sent.is_empty());
    assert!(
        a.sent
            .iter()
            .all(|sent| sent.commit_id != refused.commit_id)
    );
    assert_eq!(a.sent[0].device_seq, refused.device_seq);
    assert_eq!(remote.names(), ["notes.txt"]);
    a.settle();
    assert_eq!(a.state(), FolderState::UpToDate);
}

#[test]
fn a_commit_that_waits_for_the_owners_confirmation_is_not_lifted() {
    let mut remote = Remote::new();
    let (site, refused, halt) = refused_first_commit(&mut remote, Code::ConfirmationRequired);
    assert!(halt.contains("owner must confirm"), "{halt}");
    for lift in [Lift::Resend, Lift::Replan] {
        let lifted = lift_halt(&site.db, lift);
        assert!(
            matches!(lifted, Err(DriverError::NeedsConfirmation)),
            "{lifted:?}"
        );
    }
    let a = restarted(&site, &mut remote);
    assert!(a.sent.is_empty());
    let flight = a.folder.engine.commits.in_flight().unwrap();
    assert_eq!(flight.commit.commit_id, refused.commit_id);
}

/// GUN-349. Device `b` is moving `old.txt` to `remote.txt` after `a` renamed it, when `a` renames it back and the
/// user edits it on `b`. Only the edit was the user's, so the file ends as `old.txt` with the edit on both sides.
#[test]
fn a_file_edited_while_a_move_the_server_undid_was_in_flight_keeps_its_name() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("old.txt"), b"one");
    let (site_b, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::rename(site_a.root.join("old.txt"), site_a.root.join("remote.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    assert!(matches!(
        b.folder.engine.effects.in_flight().unwrap().effect,
        flarebox_engine::effects::Effect::Move { .. }
    ));
    fs::rename(site_a.root.join("remote.txt"), site_a.root.join("old.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    fs::write(site_b.root.join("old.txt"), b"user edit").unwrap();
    b.scan_until(|b| {
        (b.folder.engine.trees.local().iter())
            .any(|(_, node)| (node.shape.content.as_ref()).is_some_and(|content| content.size == 9))
    });
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site_b.root),
        [("old.txt".to_owned(), "user edit".to_owned())]
    );
    assert_eq!(remote.names(), ["old.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// Device `b` synced `one.txt` from `a`. The server then lost its log: a space with the same history and no
/// commits takes its place.
fn behind_the_device(remote: &mut Remote) -> (Site, Device) {
    let (site_a, mut a) = linked(remote, "a");
    a.create(remote, &site_a.root.join("one.txt"), b"one");
    let (site, mut b) = linked(remote, "b");
    b.settle();
    assert_eq!(names(&site.root), ["one.txt"]);
    *remote = Remote::new();
    (site, b)
}

/// A server restored from a backup taken now, which holds only the commits `device` sent so far and their chunks.
fn backup(remote: &Remote, device: &Device) -> Remote {
    let mut restored = Remote::new();
    let chunks: Vec<ChunkRef> = (remote.chunks.chunks.iter())
        .map(|(hash, bytes)| ChunkRef {
            hash: hash.clone(),
            size: bytes.len() as u32,
            ..Default::default()
        })
        .collect();
    if !chunks.is_empty() {
        (restored.space)
            .prepare_upload(&device.caller, NOW, &PrepareUpload { chunks })
            .unwrap();
    }
    for (hash, bytes) in &remote.chunks.chunks {
        restored.chunks.chunks.insert(hash.clone(), bytes.clone());
        restored.space.put_chunk(hash, bytes.len() as u32, NOW);
    }
    for commit in &device.sent {
        restored.space.commit(&device.caller, NOW, commit).unwrap();
    }
    restored
}

/// The names in the folder's local trash.
fn trashed(root: &Path) -> Vec<String> {
    let trash = root.join(flarebox_fs::PRIVATE_DIR).join("trash");
    fs::read_dir(trash).map_or(Vec::new(), |entries| {
        entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect()
    })
}

#[test]
fn a_server_behind_the_device_gets_back_the_files_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site.root),
        [("one.txt".to_owned(), "one".to_owned())]
    );
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(remote.names(), ["one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_commit_the_server_is_behind_for_reads_the_tree_again_and_sends_what_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    let ran = b.run(&mut remote);
    assert!(ran.starts_with(&["upload", "send", "list"]), "{ran:?}");
    b.settle();
    b.run(&mut remote);
    assert_eq!(
        files(&site.root),
        [
            ("one.txt".to_owned(), "one".to_owned()),
            ("two.txt".to_owned(), "two".to_owned())
        ]
    );
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(remote.names(), ["one.txt", "two.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The server lost the commits that created `albums` and moved `photos` into it, but kept the one that created
/// `photos`. Device `b` moves `photos` into `albums` again instead of downloading it at its old place.
#[test]
fn a_folder_moved_into_a_folder_the_server_lost_is_moved_there_again() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    fs::create_dir(site_a.root.join("photos")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site_a.root.join("albums")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::rename(
        site_a.root.join("photos"),
        site_a.root.join("albums/photos"),
    )
    .unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(site.root.join("albums/photos").is_dir());
    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert!(site.root.join("albums/photos").is_dir());
    assert_eq!(names(&site.root), ["albums"]);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    let nodes = remote.space.nodes();
    let albums = nodes.iter().find(|node| node.name == "albums").unwrap();
    let photos = nodes.iter().find(|node| node.name == "photos").unwrap();
    assert_eq!(photos.parent_id, albums.node_id);
    assert_eq!(remote.names(), ["albums", "photos"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The server lost a's edit of `one.txt` that `b` had downloaded. `b` keeps that content as a conflict copy beside
/// the server's older content, instead of replacing it.
#[test]
fn an_edit_the_server_lost_is_kept_as_a_conflict_copy() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"v1");
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::write(site_a.root.join("one.txt"), b"v2").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    assert_eq!(files(&site.root), [("one.txt".to_owned(), "v2".to_owned())]);
    remote = restored;
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    let copy = "one (conflict from b 2023-11-14).txt";
    assert_eq!(
        files(&site.root),
        [
            (copy.to_owned(), "v2".to_owned()),
            ("one.txt".to_owned(), "v1".to_owned())
        ]
    );
    assert_eq!(remote.names(), [copy, "one.txt"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// The server lost the commits that created `albums` and moved `one.txt` into it. While `b` replays the log
/// after its listing, the log brings device `c`'s delete of `one.txt`. `b` deletes the file instead of
/// uploading it again, and creates `albums` again.
#[test]
fn a_delete_replayed_below_a_folder_the_server_lost_still_deletes_the_file() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site_a.root.join("one.txt"), b"one");
    let restored = backup(&remote, &a);
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    fs::create_dir(site_a.root.join("albums")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    fs::rename(
        site_a.root.join("one.txt"),
        site_a.root.join("albums/one.txt"),
    )
    .unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(files(&site.root), [("albums/one.txt".into(), "one".into())]);
    let trees = &b.folder.engine.trees;
    let one = (trees.local().iter())
        .find(|(_, node)| node.shape.name == "one.txt")
        .unwrap()
        .0
        .clone();
    remote = restored;
    b.pull(&remote);
    let list = b.work.pop_front().unwrap();
    assert!(matches!(list, Work::List { .. }));
    b.perform(list, &mut remote).unwrap();
    assert!(matches!(b.folder.stream, Stream::Replaying(_)));
    let (site_c, mut c) = linked(&mut remote, "c");
    c.settle();
    fs::remove_file(site_c.root.join("one.txt")).unwrap();
    c.scan_until(|c| !c.work.is_empty());
    c.run(&mut remote);
    assert_eq!(remote.names(), Vec::<String>::new());
    let subscribe = b.work.pop_front().unwrap();
    assert!(matches!(subscribe, Work::Subscribe { .. }));
    b.perform(subscribe, &mut remote).unwrap();
    assert!(b.folder.engine.trees.remote().get(&one).unwrap().trashed);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    assert_eq!(files(&site.root), Vec::<(String, String)>::new());
    assert!(site.root.join("albums").is_dir());
    assert_eq!(remote.names(), ["albums"]);
    assert_eq!(b.state(), FolderState::UpToDate);
}

/// Device `c` commits to the server that is behind device `b`, so the server's head is no longer below `b`'s
/// cursor when `b` lists the tree.
fn catch_up(remote: &mut Remote, b: &Device) {
    let (site_c, mut c) = linked(remote, "c");
    c.create(remote, &site_c.root.join("other.txt"), b"other");
    let cursor = b.folder.engine.cursor.as_ref().unwrap();
    assert!(remote.space.head_seq() >= cursor.last_seq);
}

/// What `b` holds and the server holds once `b` sent what the server lost and took `c`'s file.
fn caught_up(site: &Site, b: &Device, remote: &Remote, also: &[&str]) {
    let mut expected = vec![("one.txt".to_owned(), "one".to_owned())];
    expected.extend(
        also.iter()
            .map(|name| (format!("{name}.txt"), name.to_string())),
    );
    expected.push(("other.txt".to_owned(), "other".to_owned()));
    expected.sort();
    assert_eq!(files(&site.root), expected);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    let names: Vec<String> = expected.into_iter().map(|(name, _)| name).collect();
    assert_eq!(remote.names(), names);
    assert_eq!(b.state(), FolderState::UpToDate);
}

#[test]
fn a_server_behind_the_device_whose_head_catches_up_before_the_listing_gets_back_the_files_it_lost()
{
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    assert_eq!(b.state(), FolderState::Bootstrapping);
    catch_up(&mut remote, &b);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &[]);
}

#[test]
fn a_commit_the_server_is_behind_for_sends_what_it_lost_when_the_head_catches_up_before_the_listing()
 {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    while !matches!(b.folder.stream, Stream::Listing { .. }) {
        let work = b.work.pop_front().expect("the commit is refused");
        b.perform(work, &mut remote).unwrap();
    }
    catch_up(&mut remote, &b);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &["two"]);
}

#[test]
fn a_device_that_restarts_after_the_server_behind_it_caught_up_still_gets_back_the_files_it_lost() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    b.pull(&remote);
    catch_up(&mut remote, &b);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    assert_eq!(b.run(&mut remote), ["list", "subscribe"]);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &[]);
}

/// The task stops right after the transaction that takes the refusal, before it lists the tree, and the server's head
/// then catches up. The store still knows that the server is behind.
#[test]
fn a_commit_the_server_is_behind_for_sends_what_it_lost_after_a_crash_right_after_the_refusal() {
    let mut remote = Remote::new();
    let (site, mut b) = behind_the_device(&mut remote);
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    b.scan_until(|b| !b.work.is_empty());
    loop {
        let work = b.work.pop_front().expect("the commit is sent");
        if matches!(work, Work::Send(_)) {
            b.folder.crash_after = Some(0);
            b.perform(work, &mut remote).unwrap_err();
            break;
        }
        b.perform(work, &mut remote).unwrap();
    }
    assert!(b.folder.engine.commits.in_flight().is_none());
    catch_up(&mut remote, &b);
    drop(b);
    let mut b = site.start();
    b.connect(&remote);
    b.run(&mut remote);
    b.settle();
    b.run(&mut remote);
    caught_up(&site, &b, &remote, &["two"]);
}

/// The server's storage is restored from a backup taken after \`a\`'s first commit, so it counts fewer commits of
/// \`a\` than \`a\`'s store holds (GUN-379). \`a\` lists the tree again and sends the commit the server lost.
#[test]
fn a_server_that_lost_a_commit_of_this_device_gets_it_back() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let welcome = restored.space.welcome(&a.caller);
    assert_eq!(
        (welcome.device_seq, a.folder.engine.commits.accepted()),
        (1, 2)
    );
    a.event(Event::Connected(welcome)).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt", "two.txt"]);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// As above, but \`b\` commits to the restored server twice before \`a\` reconnects, so the server's head is past
/// \`a\`'s cursor and nothing in the \`Welcome\` but the count shows what was lost. \`a\` still lists the tree again
/// instead of following the log from its cursor, and both devices' files are kept.
#[test]
fn a_server_that_lost_a_commit_of_this_device_gets_it_back_after_others_committed_past_the_cursor()
{
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("three.txt"), b"three");
    b.create(&mut restored, &other.root.join("four.txt"), b"four");
    let welcome = restored.space.welcome(&a.caller);
    let cursor = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    assert!(welcome.head_seq > cursor, "{} > {cursor}", welcome.head_seq);
    a.event(Event::Connected(welcome)).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["four.txt", "one.txt", "three.txt", "two.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(trashed(&site.root), Vec::<String>::new());
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server's storage is restored from a backup taken before `a` deleted `one.txt` (GUN-359). `a` lists the tree
/// again, which holds `one.txt` live as it was deleted, and sends the delete again instead of downloading the file.
#[test]
fn a_delete_the_server_lost_is_sent_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let mut restored = backup(&remote, &a);
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    assert_eq!(remote.names(), ["two.txt"]);
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["two.txt"]);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// As above, but `b` writes new content into `one.txt` on the restored server before `a` reconnects, at the seq
/// `a`'s lost delete had. `a` does not send the delete again: it downloads `b`'s content.
#[test]
fn a_delete_the_server_lost_is_not_sent_again_over_another_devices_edit() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("one.txt"), b"edited");
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["one.txt"]);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"edited");
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server's storage is restored from a backup taken after `a` created `one.txt`, before three more commits of
/// `a` and its delete of `one.txt` at seq 5. On the restored server `b` deletes `one.txt` at seq 2 and a person
/// restores it at seq 3. The restore brings back the shape `a` deleted, at a seq below the lost delete's, so only the
/// seq of `a`'s last commit the server still holds shows it came after the loss. `a` keeps the restore.
#[test]
fn a_restore_after_the_server_lost_a_delete_is_kept() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    for pad in ["pad-two.txt", "pad-three.txt", "pad-four.txt"] {
        a.create(&mut remote, &site.root.join(pad), pad.as_bytes());
    }
    fs::remove_file(site.root.join("one.txt")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    assert_eq!(a.run(&mut remote), ["send"]);
    assert_eq!(remote.space.head_seq(), 5);
    let (other, mut b) = linked(&mut restored, "b");
    let node = (b.folder.engine.trees.local().iter())
        .find(|(_, node)| node.shape.name == "one.txt")
        .unwrap()
        .0
        .clone();
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    let intent = RestoreIntent::from_trash(node.clone(), None, None).unwrap();
    let request = ask(&other, &intent);
    let step = b.folder.requested();
    b.take(step).unwrap();
    b.run(&mut restored);
    assert_eq!(ended(&other), [(request, Outcome::Restored)]);
    assert_eq!(
        b.folder.engine.trees.remote().get(&node).unwrap().meta_rev,
        3
    );
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["one.txt", "pad-four.txt", "pad-three.txt", "pad-two.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(fs::read(site.root.join("one.txt")).unwrap(), b"one");
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The server lost `a`'s commit of `two.txt`, and `b` deleted `one.txt` on the restored server. The listing reads
/// the trash too, so `a` takes `one.txt` for a remote delete and removes it, instead of creating it again under an
/// ID the server holds in its trash, which it would refuse with `ID_EXISTS`.
#[test]
fn a_file_another_device_deleted_after_the_server_lost_commits_is_removed_here() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    let mut restored = backup(&remote, &a);
    a.create(&mut remote, &site.root.join("two.txt"), b"two");
    let (other, mut b) = linked(&mut restored, "b");
    fs::remove_file(other.root.join("one.txt")).unwrap();
    b.scan_until(|b| !b.work.is_empty());
    assert_eq!(b.run(&mut restored), ["send"]);
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["two.txt"]);
    assert_eq!(names(&site.root), ["two.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// `a` lists the tree after its log was truncated, and reconnects to a server that lost its commit and took two of
/// `b`'s at the same seqs before the listing returns. The pages read from the old server are dropped and the tree is
/// listed again, so R holds the restored server's files, not the old snapshot with its seq 1.
#[test]
fn a_listing_out_when_the_server_lost_this_devices_commits_is_read_again() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    a.event(Event::ResyncRequired(ResyncRequired {
        reason: Reason::LogTruncated as i32,
        ..ResyncRequired::default()
    }))
    .unwrap();
    let Some(Work::List { .. }) = a.work.pop_front() else {
        panic!("list");
    };
    let old = remote.pages(TREE_PAGE);
    let mut restored = Remote::new();
    let (other, mut b) = linked(&mut restored, "b");
    b.create(&mut restored, &other.root.join("new-one.txt"), b"new one");
    b.create(&mut restored, &other.root.join("new-two.txt"), b"new two");
    a.event(Event::Connected(restored.space.welcome(&a.caller)))
        .unwrap();
    let step = a.folder.listed(Ok(Listed::Done {
        pages: old,
        trash: vec![],
    }));
    a.take(step).unwrap();
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    let all = ["new-one.txt", "new-two.txt", "one.txt"];
    assert_eq!(restored.names(), all);
    assert_eq!(names(&site.root), all);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// A reconnect's `Welcome` counts one commit of `a`, and the commit in flight is accepted after it. When the send
/// finishes while the `Connected` event is still queued, the folder takes the event first, so the count is not older
/// than what it acknowledged, and the healthy server is not taken for one that lost a commit.
#[test]
fn a_welcome_queued_before_a_finished_send_is_taken_first() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    fs::write(site.root.join("two.txt"), b"two").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    let queued = remote.space.welcome(&a.caller);
    assert_eq!(queued.device_seq, 1);
    let result = remote.commit(&a.caller, &commit).unwrap();
    let step = sent_in_order(&mut a.folder, [Event::Connected(queued)], Ok(result));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.accepted(), 2);
    assert!(!a.work.iter().any(|work| matches!(work, Work::List { .. })));
    a.pull(&remote);
    a.run(&mut remote);
    a.settle();
    fs::write(site.root.join("three.txt"), b"three").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.try_run(&mut remote).unwrap();
    assert_eq!(remote.names(), ["one.txt", "three.txt", "two.txt"]);
    assert_eq!(a.folder.engine.commits.accepted(), 3);
}

/// A commit in flight when the task stopped has a slot the restored server cannot account for. It is dropped
/// rather than sent, and what it held is planned again once the tree is listed.
#[test]
fn a_commit_in_flight_when_the_server_lost_this_devices_commits_is_planned_again() {
    let mut remote = Remote::new();
    let (site, commit) = stopped_with_a_commit_in_flight(&mut remote);
    let mut restored = Remote::new();
    let mut a = site.start();
    let welcome = restored.space.welcome(&a.caller);
    assert_eq!((welcome.device_seq, commit.device_seq), (0, 2));
    a.event(Event::Connected(welcome)).unwrap();
    assert!(a.folder.engine.commits.in_flight().is_none());
    assert_eq!(a.folder.engine.commits.accepted(), 0);
    let ran = a.run(&mut restored);
    assert!(!ran.is_empty() && ran[0] == "list", "{ran:?}");
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["draft.txt", "notes.txt"]);
    assert!(a.sent.iter().all(|sent| sent.commit_id != commit.commit_id));
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The session reports that it did not send the commit in flight again before the folder has taken the \`Welcome\`
/// with the server's lower count. The folder waits for that \`Welcome\` instead of stopping, and then goes on as
/// above.
#[test]
fn a_commit_the_session_did_not_send_again_to_a_server_that_lost_commits_waits_for_the_welcome() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("notes.txt"), b"written on a");
    let mut restored = Remote::new();
    fs::write(site.root.join("draft.txt"), b"in flight").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let upload = a.work.pop_front().unwrap();
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent after its upload");
    };
    let not_resent = NotResent::DeviceSeqMismatch {
        slot: commit.device_seq,
        welcome: 0,
    };
    let step = a.folder.sent(Err(CallError::NotResent(not_resent)));
    a.take(step).unwrap();
    a.connect(&restored);
    a.run(&mut restored);
    a.settle();
    a.run(&mut restored);
    assert_eq!(restored.names(), ["draft.txt", "notes.txt"]);
    assert_eq!(a.state(), FolderState::UpToDate);
}

/// The store is put back to a copy taken before the folder downloaded a file, and the server's storage is restored
/// from before this device's commit, so the server counts fewer commits than the store and the store is older than
/// the disk. The root marker shows the latter when the folder opens, before any \`Welcome\`, and the task stops for
/// recovery instead of sending what the store holds.
#[test]
fn a_store_older_than_the_disk_stops_for_recovery_even_when_the_server_lost_its_commits() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("one.txt"), b"one");
    drop(a);
    let copy = site.db.with_extension("copy");
    fs::copy(&site.db, &copy).unwrap();
    let (other, mut b) = linked(&mut remote, "b");
    b.create(&mut remote, &other.root.join("two.txt"), b"two");
    let mut a = site.start();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(names(&site.root), ["one.txt", "two.txt"]);
    drop(a);
    fs::copy(&copy, &site.db).unwrap();
    let restored = Remote::new();
    assert_eq!(restored.space.welcome(&site.caller).device_seq, 0);

    let lock = claim::lock(&site.root)
        .unwrap()
        .expect("the folder is not locked");
    let local = local_rules(&SyncRules::default()).unwrap();
    match Folder::open(site.link.clone(), &lock, &site.db, local) {
        Err(Stop::StoreBehind { .. }) => {}
        Err(stop) => panic!("{stop}"),
        Ok(_) => panic!("the folder opened with a store older than the disk"),
    }
}

/// The user moves `source/file` into the ignored folder `ignored` and removes `source`, and one batch reports both,
/// in either order. The file is outside what this device syncs, so it stays on the server, and so does `source`.
#[test]
fn a_file_moved_into_an_ignored_folder_stays_on_the_server_when_its_folder_is_removed_in_the_same_batch()
 {
    for removed_first in [false, true] {
        let mut remote = Remote::new();
        let (site, mut a) = linked(&mut remote, "a");
        fs::create_dir(site.root.join("source")).unwrap();
        a.create(&mut remote, &site.root.join("source/file"), b"kept");
        a.create(&mut remote, &site.root.join(".fbignore"), b"ignored/\n");
        let id = a.observer.snapshot().entries[Path::new("source/file")].id;
        fs::create_dir(site.root.join("ignored")).unwrap();
        fs::rename(
            site.root.join("source/file"),
            site.root.join("ignored/file"),
        )
        .unwrap();
        fs::remove_dir(site.root.join("source")).unwrap();
        let renamed = Observed::Renamed {
            from: "source/file".into(),
            to: "ignored/file".into(),
            kind: EntryKind::File,
            id,
            linked: false,
        };
        let removed = Observed::Removed {
            path: "source".into(),
            kind: EntryKind::Dir,
        };
        let changes = match removed_first {
            false => vec![renamed, removed],
            true => vec![removed, renamed],
        };
        let sections = a.folder.sections();
        let batch = Batch {
            stamp: sections.stamp(),
            answered: None,
            observation: crate::observe::Observation {
                changes,
                root: Ok(()),
                unreadable: Vec::new(),
                read: vec![PathBuf::new()],
                scanned: a.observer.scanned(),
                busy: Vec::new(),
                rules: Vec::new(),
                all_rules: false,
            },
        };
        let step = a.folder.observed(batch, 1);
        a.take(step).unwrap();
        a.run(&mut remote);
        assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
        assert_eq!(
            remote.names(),
            [".fbignore", "file", "source"],
            "{removed_first}"
        );
        assert_eq!(a.folder.engine.trees.check(), Ok(()));
    }
}

/// As above, with the move and the removal in two batches, and a restart after them: the restart scans the folder
/// again from L, which still holds `source/file`, and finds the file in the ignored folder again.
#[test]
fn a_file_moved_into_an_ignored_folder_stays_on_the_server_when_its_folder_is_removed_later_and_after_a_restart()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.folder.engine.trees.ignored_local().len()) == 2);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "file", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
    assert_eq!(remote.names(), [".fbignore", "file", "source"]);
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The user hard-links `source/file` into the ignored folder, moves the file itself to `moved`, and removes
/// `source`. The file is still synced at `moved`, so the server follows the move; the link stays out.
#[test]
#[ignore = "GUN-368"]
fn a_file_moved_out_of_a_removed_folder_with_a_link_in_an_ignored_folder_is_moved_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ignored/\n");
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::hard_link(
        site.root.join("source/file"),
        site.root.join("ignored/alias"),
    )
    .unwrap();
    fs::rename(site.root.join("source/file"), site.root.join("moved")).unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("moved")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "moved"]);
    assert_eq!(fs::read(site.root.join("moved")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("ignored/alias")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The file moved into an ignored folder has a synced link, `also`, that stays. The file is still left out, so
/// the server keeps it and its folder.
#[test]
fn a_file_moved_into_an_ignored_folder_out_of_a_removed_folder_with_an_included_link_that_stays_is_kept_on_the_server()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir(site.root.join("source")).unwrap();
    a.create(&mut remote, &site.root.join("source/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ignored/\n");
    fs::hard_link(site.root.join("source/file"), site.root.join("also")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "also", "file", "source"]);
    fs::create_dir(site.root.join("ignored")).unwrap();
    fs::rename(
        site.root.join("source/file"),
        site.root.join("ignored/file"),
    )
    .unwrap();
    fs::remove_dir(site.root.join("source")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("source")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "also", "file", "source"]);
    assert_eq!(fs::read(site.root.join("ignored/file")).unwrap(), b"kept");
    assert_eq!(fs::read(site.root.join("also")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The file is moved into an ignored folder, its own folder is removed, and the folder above that is moved to
/// `dest`. The move of that folder does not carry the file, so the server keeps it.
#[test]
fn a_file_moved_into_an_ignored_folder_before_a_folder_above_it_was_moved_is_kept_on_the_server() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a/b")).unwrap();
    fs::create_dir(site.root.join("dest")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/b/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ig/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::rename(site.root.join("d/a/b/file"), site.root.join("ig/file")).unwrap();
    fs::remove_dir(site.root.join("d/a/b")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("dest/a")).unwrap();
    fs::remove_dir(site.root.join("d")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("dest/a")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert!(
        remote.names().contains(&"file".to_owned()),
        "{:?}",
        remote.names()
    );
    assert_eq!(fs::read(site.root.join("ig/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The user moves the folder `d/a` into the ignored folder `ig`, moves its file from there into the ignored folder
/// `other`, brings the empty folder back as `returned` and removes it. The file stays out the whole time, so the
/// server keeps it, with the folder it is in there.
#[test]
fn a_file_moved_out_of_its_folder_while_the_folder_was_ignored_stays_on_the_server_when_the_folder_returns_and_is_removed()
 {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ig/\nother/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::create_dir(site.root.join("other")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("ig/a")).unwrap();
    fs::rename(site.root.join("ig/a/file"), site.root.join("other/file")).unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::rename(site.root.join("ig/a"), site.root.join("returned")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "d", "file", "returned"]);
    fs::remove_dir(site.root.join("returned")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "d", "file", "returned"]);
    assert_eq!(fs::read(site.root.join("other/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// As above, with a restart after the folder returns: the snapshot rebuilt from L holds the file in `returned`, and
/// the first scan finds it in `other`.
#[test]
fn a_file_moved_out_of_its_folder_while_the_folder_was_ignored_stays_on_the_server_after_a_restart()
{
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::create_dir_all(site.root.join("d/a")).unwrap();
    a.create(&mut remote, &site.root.join("d/a/file"), b"kept");
    a.create(&mut remote, &site.root.join(".fbignore"), b"ig/\nother/\n");
    fs::create_dir(site.root.join("ig")).unwrap();
    fs::create_dir(site.root.join("other")).unwrap();
    fs::rename(site.root.join("d/a"), site.root.join("ig/a")).unwrap();
    fs::rename(site.root.join("ig/a/file"), site.root.join("other/file")).unwrap();
    a.scan_until(|a| !a.folder.engine.trees.ignored_local().is_empty());
    a.run(&mut remote);
    fs::rename(site.root.join("ig/a"), site.root.join("returned")).unwrap();
    a.scan_until(|a| (a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    drop(a);

    let mut a = site.start();
    a.connect(&remote);
    a.scan_until(|a| a.folder.scanned);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "d", "file", "returned"]);
    fs::remove_dir(site.root.join("returned")).unwrap();
    a.scan_until(|a| !(a.observer.snapshot().entries).contains_key(Path::new("returned")));
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(remote.names(), [".fbignore", "d", "file", "returned"]);
    assert_eq!(fs::read(site.root.join("other/file")).unwrap(), b"kept");
    assert_eq!(a.folder.engine.trees.check(), Ok(()));
}

/// The server's nodes by name, with their IDs and whether each is live.
type Nodes = Vec<(String, String, bool)>;

/// Every node on the server by name, with its ID and whether it is live, in order.
fn server_nodes(remote: &Remote) -> Nodes {
    let live = flarebox_proto::v1::NodeStatus::Live as i32;
    let mut nodes: Nodes = (remote.space.nodes().into_iter())
        .filter(|node| !node.parent_id.is_empty())
        .map(|node| (node.name, node.node_id, node.status == live))
        .collect();
    nodes.sort();
    nodes
}

/// The ID of the live node named `name` in `nodes`.
fn live_id(nodes: &[(String, String, bool)], name: &str) -> String {
    let found = nodes.iter().find(|(named, _, live)| named == name && *live);
    found
        .unwrap_or_else(|| panic!("no live {name} in {nodes:?}"))
        .1
        .clone()
}

/// Device `a` makes `files`, then device `b` makes each rename of `renames` in turn before its observer looks, and
/// both sync. Returns both sites and the server's nodes before and after the renames.
fn renamed_on_b(files: &[(&str, &[u8])], renames: &[(&str, &str)]) -> (Site, Site, Nodes, Nodes) {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    for (path, bytes) in files {
        let path = site_a.root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        a.create(&mut remote, &path, bytes);
    }
    let (site, mut b) = linked(&mut remote, "b");
    b.settle();
    let before = server_nodes(&remote);
    for (from, to) in renames {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    sync(&mut remote, (&site_a, &mut a), (&site, &mut b));
    (site_a, site, before, server_nodes(&remote))
}

/// Device `b` swaps two files through a third name. Each file keeps its node, and so its version history, and both
/// names arrive as they are, with no conflict name.
#[test]
fn two_files_swapped_through_a_third_name_keep_their_nodes() {
    let (site_a, site, before, after) = renamed_on_b(
        &[("one", b"first file"), ("two", b"second file")],
        &[("one", "tmp"), ("two", "one"), ("tmp", "two")],
    );
    let swapped = [("one", "second file"), ("two", "first file")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(live_id(&after, "one"), live_id(&before, "two"));
    assert_eq!(live_id(&after, "two"), live_id(&before, "one"));
}

/// As above for three files rotated through a third name.
#[test]
fn three_files_rotated_through_a_third_name_keep_their_nodes() {
    let (_, site, before, after) = renamed_on_b(
        &[("a", b"from a"), ("b", b"from b"), ("c", b"from c")],
        &[("a", "tmp"), ("c", "a"), ("b", "c"), ("tmp", "b")],
    );
    let rotated = [("a", "from c"), ("b", "from a"), ("c", "from b")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site.root), rotated);
    assert_eq!(after.len(), 3, "{after:?}");
    assert_eq!(live_id(&after, "a"), live_id(&before, "c"));
    assert_eq!(live_id(&after, "b"), live_id(&before, "a"));
    assert_eq!(live_id(&after, "c"), live_id(&before, "b"));
}

/// Device `b` swaps two folders that each hold a file named `note`. The folders and their files keep their nodes,
/// so nothing goes to the server's trash, and no name carries a conflict suffix.
#[test]
fn two_folders_swapped_through_a_third_name_keep_their_nodes() {
    let (site_a, site, before, after) = renamed_on_b(
        &[("x/note", b"in x"), ("y/note", b"in y")],
        &[("x", "tmp"), ("y", "x"), ("tmp", "y")],
    );
    let swapped = [("x/note", "in y"), ("y/note", "in x")]
        .map(|(path, text)| (path.to_owned(), text.to_owned()));
    assert_eq!(files(&site_a.root), swapped);
    assert_eq!(files(&site.root), swapped);
    assert!(after.iter().all(|(_, _, live)| *live), "{after:?}");
    let names: Vec<&str> = after.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, ["note", "note", "x", "y"]);
    assert_eq!(live_id(&after, "x"), live_id(&before, "y"));
    assert_eq!(live_id(&after, "y"), live_id(&before, "x"));
    let ids = |nodes: &[(String, String, bool)]| -> std::collections::BTreeSet<String> {
        nodes.iter().map(|(_, id, _)| id.clone()).collect()
    };
    assert_eq!(ids(&after), ids(&before));
}

/// An editor saves `doc` by writing a new file next to it, swapping the two, and removing the old one, and the
/// observer sees the new file before the swap. `doc` keeps its node, so its version history stays with it.
#[test]
fn a_file_saved_by_swapping_in_a_new_file_keeps_its_node() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    a.create(&mut remote, &site.root.join("doc"), b"first");
    let before = server_nodes(&remote);
    a.event(Event::Disconnected {
        reason: flarebox_net::DisconnectReason::Timeout,
        retry_in: Duration::from_secs(1),
    })
    .unwrap();
    fs::write(site.root.join(".doc.tmp"), b"second").unwrap();
    a.scan_until(|a| a.folder.engine.trees.dirty().len() == 1);
    assert!(a.work.is_empty());
    for (from, to) in [(".doc.tmp", "x"), ("doc", ".doc.tmp"), ("x", "doc")] {
        fs::rename(site.root.join(from), site.root.join(to)).unwrap();
    }
    a.settle();
    fs::remove_file(site.root.join(".doc.tmp")).unwrap();
    a.settle();
    a.connect(&remote);
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);
    assert_eq!(files(&site.root), [("doc".to_owned(), "second".to_owned())]);
    let after = server_nodes(&remote);
    assert_eq!(live_id(&after, "doc"), live_id(&before, "doc"), "{after:?}");
}
/// GUN-388. The status named only how many items could not be synced, so the user could not tell which ones or why.
#[test]
fn the_status_names_a_node_the_server_rejected_an_op_for_and_why() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    assert_eq!(kinds(&commit), ["create file"]);
    let last_seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: last_seq + 1,
        results: vec![flarebox_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_NAME".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
    };
    let step = a.folder.sent(Ok(result));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: notes.txt (the server rejected it: INVALID_NAME); run flarebox restart once it is fixed"
        )
    );
}

/// GUN-388, for a commit the server refused as a whole: the node it leaves out of commits is named with the refusal.
#[test]
fn the_status_names_a_node_of_a_refused_commit_and_the_refusal() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(_)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    let refusal = flarebox_proto::v1::Error {
        code: Code::LimitExceeded as i32,
        message: "the commit is too large".into(),
        ..Default::default()
    };
    let step = a.folder.sent(Err(CallError::Server(ServerError(refusal))));
    a.take(step).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: notes.txt (the server refused it: CODE_LIMIT_EXCEEDED: the commit is too large); run flarebox restart once it is fixed"
        )
    );
}

/// GUN-388, for an effect that failed: the node is named with why the effect failed.
#[test]
fn the_status_names_a_node_whose_effect_failed_and_why() {
    let mut remote = Remote::new();
    let (_site, b, _) = failed_at_a_taken_name(&mut remote);
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: notes.txt (the server's change could not be made here: another entry holds its name); run flarebox restart once it is fixed"
        )
    );
}
/// GUN-388, for a node L does not hold yet: a write the server made in a folder fails, and the status names the
/// write by its whole path, not by its name alone.
#[test]
fn the_status_names_a_failed_write_in_a_folder_by_its_whole_path() {
    let mut remote = Remote::new();
    let (site_a, mut a) = linked(&mut remote, "a");
    let (site, mut b) = linked(&mut remote, "b");
    fs::create_dir(site_a.root.join("docs")).unwrap();
    a.scan_until(|a| !a.work.is_empty());
    a.run(&mut remote);
    b.pull(&remote);
    b.run(&mut remote);
    b.settle();
    a.create(
        &mut remote,
        &site_a.root.join("docs/notes.txt"),
        b"written on a",
    );
    b.pull(&remote);
    let Some(Work::Stage(pending)) = b.work.pop_front() else {
        panic!("the effect that writes docs/notes.txt is staged");
    };
    fs::write(site.root.join("docs/notes.txt"), b"the user's").unwrap();
    let staged = ready(pending.stage(&remote.chunks));
    let step = b.folder.staged(pending, staged);
    b.take(step).unwrap();
    assert_eq!(b.folder.engine.effects.blocked().len(), 1);
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: docs/notes.txt (the server's change could not be made here: another entry \
             holds its name); run flarebox restart once it is fixed"
        )
    );
}
/// GUN-388. A node whose effect failed and that the server also left out of commits is one item, with both reasons.
#[test]
fn the_status_names_a_node_stuck_for_two_reasons_once() {
    let mut remote = Remote::new();
    let (_site, mut b, _) = failed_at_a_taken_name(&mut remote);
    let id = b
        .folder
        .engine
        .effects
        .blocked()
        .keys()
        .next()
        .unwrap()
        .clone();
    let mut state = b.folder.engine.commits.state();
    state.problems.insert(id.clone());
    b.folder.engine.commits = flarebox_engine::commit::Commits::from_state(state).unwrap();
    (b.folder.refused_nodes).insert(id, "the server rejected it: INVALID_NAME".into());
    assert_eq!(
        b.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: notes.txt (the server's change could not be made here: another entry holds \
             its name; the server rejected it: INVALID_NAME); run flarebox restart once it is fixed"
        )
    );
}

/// GUN-388. A result that waits for the log keeps the server's reason through the first scan of a start, so the
/// node it leaves out of commits once the log reaches it is named with that reason.
#[test]
fn the_reason_of_a_result_that_waits_for_the_log_outlasts_the_first_scan() {
    let mut remote = Remote::new();
    let (site, mut a) = linked(&mut remote, "a");
    fs::write(site.root.join("notes.txt"), b"written on a").unwrap();
    a.scan_until(|a| !a.work.is_empty());
    let Some(upload @ Work::Upload(_)) = a.work.pop_front() else {
        panic!("the new file is uploaded");
    };
    a.perform(upload, &mut remote).unwrap();
    let Some(Work::Send(commit)) = a.work.pop_front() else {
        panic!("the commit is sent");
    };
    a.folder.scanned = false;
    let seq = a.folder.engine.cursor.as_ref().unwrap().last_seq;
    let result = CommitResult {
        seq: seq + 2,
        results: vec![flarebox_proto::v1::OpResult {
            code: op_result::Code::Rejected as i32,
            reject_reason: "INVALID_NAME".into(),
            ..Default::default()
        }],
        changes: Vec::new(),
    };
    let step = a.folder.sent(Ok(result.clone()));
    a.take(step).unwrap();
    assert!(a.folder.engine.commits.problems().is_empty());
    let step = a.folder.scanned();
    a.take(step).unwrap();
    let record = CommitRecord {
        seq: seq + 1,
        ..Default::default()
    };
    a.event(Event::Changes(vec![record])).unwrap();
    let committed = Committed {
        commit_id: commit.commit_id,
        result,
    };
    a.event(Event::Committed(committed)).unwrap();
    assert_eq!(a.folder.engine.commits.problems().len(), 1);
    assert_eq!(
        a.folder.view().problem.as_deref(),
        Some(
            "1 item could not be synced: notes.txt (the server rejected it: INVALID_NAME); run flarebox restart once \
             it is fixed"
        )
    );
}

/// Device `a` synced the folder `d` with 15 files and 5 files beside it. The folder was removed while `a` was
/// stopped, so the first scan of the next start reports the whole removal. The guard holds it (design §6.1): the
/// server keeps every file and the store holds one question about `d`.
fn with_a_held_folder(remote: &mut Remote) -> (Site, Device) {
    let (site, mut a) = linked(remote, "a");
    fs::create_dir(site.root.join("d")).unwrap();
    for i in 0..15 {
        fs::write(site.root.join(format!("d/{i}.txt")), format!("in d {i}")).unwrap();
    }
    for i in 0..5 {
        fs::write(site.root.join(format!("{i}.txt")), format!("beside d {i}")).unwrap();
    }
    a.scan_until(|a| !a.work.is_empty());
    a.run(remote);
    a.settle();
    a.run(remote);
    assert_eq!(remote.names().len(), 21);
    drop(a);
    fs::remove_dir_all(site.root.join("d")).unwrap();

    let mut a = restarted(&site, remote);
    a.settle();
    a.run(remote);
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(remote.names().len(), 21, "nothing is deleted on the server");
    assert_eq!(a.folder.view().questions, 1);
    (site, a)
}

/// Answers the one question of the site's store, as `Driver::answer` records it, and returns it.
fn answer(site: &Site, answer: Answer) -> crate::question::Question {
    let mut store = Store::open(&site.db).unwrap();
    store
        .transaction(|tx| {
            let questions = tx.questions()?;
            let [stored] = &questions[..] else {
                panic!("one question: {questions:?}");
            };
            let given = flarebox_store::GivenAnswer {
                answer: answer.as_str().into(),
                answered_at_ms: NOW,
            };
            assert!(tx.answer_question(&stored.question_id, &given)?);
            Ok::<_, flarebox_store::Error>(serde_json::from_slice(&stored.body).unwrap())
        })
        .unwrap()
}

#[test]
fn a_removed_folder_that_holds_most_of_the_space_comes_back_when_the_person_restores_it() {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);

    let question = answer(&site, Answer::Restore);
    assert_eq!(
        question.kind,
        crate::question::QuestionKind::MassDelete {
            deletions: 1,
            affected_files: 15,
            space_files: 20,
        }
    );
    let held: Vec<_> = (question.items.iter())
        .map(|item| (item.path.as_str(), item.files))
        .collect();
    assert_eq!(held, [("d", 15)]);
    let step = a.folder.requested();
    a.take(step).unwrap();
    a.run(&mut remote);
    a.settle();
    a.run(&mut remote);

    assert_eq!(names(&site.root.join("d")).len(), 15);
    assert_eq!(remote.names().len(), 21);
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 0);
    let mut store = Store::open(&site.db).unwrap();
    assert_eq!(store.transaction(|tx| tx.questions()).unwrap(), vec![]);
}

#[test]
fn a_removed_folder_that_holds_most_of_the_space_is_deleted_when_the_person_says_so() {
    let mut remote = Remote::new();
    let (site, mut a) = with_a_held_folder(&mut remote);

    answer(&site, Answer::Delete);
    let step = a.folder.requested();
    a.take(step).unwrap();
    assert_eq!(a.run(&mut remote), ["send"]);

    assert_eq!(kinds(a.sent.last().unwrap()), ["delete"]);
    assert_eq!(remote.names().len(), 5);
    assert_eq!(a.folder.view().questions, 0);
}

/// A folder held while the device was stopped, a file removed beside it, and an answer given before
/// the device starts again.
fn answered_while_stopped(remote: &mut Remote, given: Answer) -> Device {
    let (site, a) = with_a_held_folder(remote);
    drop(a);
    fs::remove_file(site.root.join("0.txt")).unwrap();
    assert_eq!(answer(&site, given).items.len(), 1);
    let mut a = restarted(&site, remote);
    a.settle();
    a.run(remote);
    a
}

#[test]
fn a_delete_given_while_stopped_covers_only_the_deletions_its_question_listed() {
    let mut remote = Remote::new();
    let a = answered_while_stopped(&mut remote, Answer::Delete);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(
        a.folder.view().questions,
        1,
        "asked again about d and 0.txt"
    );
}

#[test]
fn a_restore_given_while_stopped_holds_a_deletion_made_since() {
    let mut remote = Remote::new();
    let a = answered_while_stopped(&mut remote, Answer::Restore);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 1, "asked about 0.txt");
}

#[test]
fn a_taken_answer_survives_a_second_restart_before_it_is_applied() {
    let mut remote = Remote::new();
    let (site, a) = with_a_held_folder(&mut remote);
    drop(a);
    answer(&site, Answer::Delete);
    let a = site.start();
    assert_eq!(
        a.folder.engine.commits.questions()[0].answer,
        Some(Answer::Delete)
    );
    drop(a);
    fs::remove_file(site.root.join("0.txt")).unwrap();

    let mut a = restarted(&site, &mut remote);
    a.settle();
    a.run(&mut remote);

    assert!(
        remote.names().contains(&"0.txt".to_owned()),
        "{:?}",
        remote.names()
    );
    assert!(
        a.sent
            .iter()
            .all(|commit| !kinds(commit).contains(&"delete"))
    );
    assert_eq!(a.folder.view().questions, 1);
}
