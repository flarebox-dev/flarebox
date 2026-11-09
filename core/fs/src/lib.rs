// SPDX-License-Identifier: Apache-2.0

//! File system change detection for a synced folder.
//!
//! A [`Snapshot`] records every file and folder under a root with its identity
//! (device, inode, birth time), size, modification time and executable bit.
//! [`Snapshot::rescan`] compares parts of the disk against it and reports typed
//! [`Change`]s, pairing a disappeared path with an appeared one of the same
//! identity as a rename. Watchers only decide which parts to rescan:
//!
//! - [`NativeWatcher`] listens to FSEvents on macOS and inotify on Linux and
//!   rescans the paths they name.
//! - [`RescanWatcher`] rescans the whole tree on an interval, for file systems
//!   where native events are unavailable or unreliable.
//!
//! Both start from a snapshot the caller kept from the previous run, and their
//! first poll rescans the whole tree, so changes made while nothing was
//! watching are reported like any other.
//!
//! Symbolic links and special files are not synced in v0 (spec §5.1) and are
//! skipped.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

/// How long native events must be quiet before the watcher rescans, so both
/// halves of a rename and a create followed by a write land in one batch.
const SETTLE: Duration = Duration::from_millis(50);
/// The longest a batch waits for quiet under constant activity.
const MAX_SETTLE: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("file system watcher: {0}")]
    Watch(Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
}

/// What survives a rename and changes when a path is deleted and created
/// again. The birth time guards against inode numbers being reused, on file
/// systems that report it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    pub birth_ns: Option<u128>,
}

/// One file or folder. For folders, `size`, `mtime_ns` and `executable` are
/// always zero: a folder changes only through its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: EntryKind,
    pub id: FileId,
    pub size: u64,
    pub mtime_ns: i128,
    pub executable: bool,
}

impl Entry {
    fn from_metadata(meta: &Metadata) -> Option<Self> {
        let kind = if meta.is_dir() {
            EntryKind::Dir
        } else if meta.is_file() {
            EntryKind::File
        } else {
            return None;
        };
        let id = FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            birth_ns: meta
                .created()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|since| since.as_nanos()),
        };
        Some(match kind {
            EntryKind::Dir => Self {
                kind,
                id,
                size: 0,
                mtime_ns: 0,
                executable: false,
            },
            EntryKind::File => Self {
                kind,
                id,
                size: meta.len(),
                mtime_ns: i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                executable: meta.permissions().mode() & 0o111 != 0,
            },
        })
    }
}

/// A change between two states of the tree. Paths are relative to the root.
/// `Removed::path` and `Renamed::from` name the previous state; `Created::path`,
/// `Modified::path` and `Renamed::to` name the new one. Resolve previous-state
/// paths before applying any change of the same batch.
///
/// Removing or renaming a folder is one change; its contents are not listed
/// unless they changed on their own. Creating a folder lists every entry in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Created {
        path: PathBuf,
        kind: EntryKind,
    },
    Modified {
        path: PathBuf,
    },
    Renamed {
        from: PathBuf,
        to: PathBuf,
        kind: EntryKind,
    },
    Removed {
        path: PathBuf,
        kind: EntryKind,
    },
}

/// The known state of every entry under a root, keyed by relative path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: BTreeMap<PathBuf, Entry>,
}

impl Snapshot {
    pub fn scan(root: impl AsRef<Path>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        scan_into(root.as_ref(), Path::new(""), &mut entries)?;
        Ok(Self { entries })
    }

    /// Rescans the given relative paths, each with everything below it, updates
    /// the snapshot and returns what changed. An empty path rescans the whole
    /// tree. A path whose parent the snapshot does not know as a folder is
    /// widened to its highest unknown ancestor, so the snapshot never holds an
    /// entry without its parent. Fails without changing the snapshot if the root
    /// is missing, rather than reporting everything as removed.
    pub fn rescan(
        &mut self,
        root: impl AsRef<Path>,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Vec<Change>> {
        let root = root.as_ref();
        if !fs::metadata(root)?.is_dir() {
            return Err(io::Error::from(io::ErrorKind::NotADirectory).into());
        }
        let mut scopes: Vec<PathBuf> = paths.into_iter().map(|path| self.anchor(path)).collect();
        scopes.sort();
        scopes.dedup();
        let mut outermost: Vec<PathBuf> = Vec::new();
        for scope in scopes {
            if !outermost.last().is_some_and(|last| scope.starts_with(last)) {
                outermost.push(scope);
            }
        }

        let mut new = BTreeMap::new();
        for scope in &outermost {
            scan_into(root, scope, &mut new)?;
        }
        let mut old = BTreeMap::new();
        for scope in &outermost {
            let inside: Vec<PathBuf> = self
                .entries
                .range(scope.clone()..)
                .take_while(|(path, _)| path.starts_with(scope))
                .map(|(path, _)| path.clone())
                .collect();
            for path in inside {
                let entry = self.entries.remove(&path).expect("collected from the map");
                old.insert(path, entry);
            }
        }
        let changes = diff(&old, &new);
        self.entries.extend(new);
        Ok(changes)
    }

    fn anchor(&self, mut path: PathBuf) -> PathBuf {
        while let Some(parent) = path.parent() {
            let known_dir = self
                .entries
                .get(parent)
                .is_some_and(|entry| entry.kind == EntryKind::Dir);
            if parent.as_os_str().is_empty() || known_dir {
                break;
            }
            path = parent.to_path_buf();
        }
        path
    }
}

fn is_absent(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Adds `rel` and everything below it to `out`. The empty path stands for the
/// root, which must exist and is not an entry itself.
fn scan_into(root: &Path, rel: &Path, out: &mut BTreeMap<PathBuf, Entry>) -> Result<()> {
    if !rel.as_os_str().is_empty() {
        let meta = match fs::symlink_metadata(root.join(rel)) {
            Ok(meta) => meta,
            Err(err) if is_absent(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let Some(entry) = Entry::from_metadata(&meta) else {
            return Ok(());
        };
        out.insert(rel.to_path_buf(), entry);
        if entry.kind == EntryKind::File {
            return Ok(());
        }
    }
    let dir = match fs::read_dir(root.join(rel)) {
        Ok(dir) => dir,
        Err(err) if is_absent(&err) && !rel.as_os_str().is_empty() => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for item in dir {
        let item = match item {
            Ok(item) => item,
            Err(err) if is_absent(&err) => continue,
            Err(err) => return Err(err.into()),
        };
        scan_into(root, &rel.join(item.file_name()), out)?;
    }
    Ok(())
}

fn diff(old: &BTreeMap<PathBuf, Entry>, new: &BTreeMap<PathBuf, Entry>) -> Vec<Change> {
    let mut removed: BTreeMap<&Path, &Entry> = BTreeMap::new();
    let mut created: Vec<(&Path, &Entry)> = Vec::new();
    let mut modified: Vec<&Path> = Vec::new();
    for (path, before) in old {
        match new.get(path) {
            Some(after) if after.kind == before.kind => {
                if after.kind == EntryKind::File && after != before {
                    modified.push(path);
                }
            }
            _ => {
                removed.insert(path, before);
            }
        }
    }
    for (path, after) in new {
        if !old
            .get(path)
            .is_some_and(|before| before.kind == after.kind)
        {
            created.push((path, after));
        }
    }

    let mut by_id: HashMap<(FileId, EntryKind), VecDeque<&Path>> = HashMap::new();
    for (path, entry) in &removed {
        by_id
            .entry((entry.id, entry.kind))
            .or_default()
            .push_back(path);
    }
    let mut renames: Vec<(&Path, &Path, &Entry)> = Vec::new();
    created.retain(|&(to, after)| {
        let Some(from) = by_id
            .get_mut(&(after.id, after.kind))
            .and_then(VecDeque::pop_front)
        else {
            return true;
        };
        let before = removed.remove(from).expect("indexed from removed");
        if after.kind == EntryKind::File && after != before {
            modified.push(to);
        }
        renames.push((from, to, after));
        false
    });
    renames.sort_by_key(|&(from, _, _)| from);
    modified.sort();

    let mut changes = Vec::new();
    for (&path, entry) in &removed {
        let parent_removed = path
            .parent()
            .is_some_and(|parent| removed.contains_key(parent));
        if !parent_removed {
            changes.push(Change::Removed {
                path: path.to_path_buf(),
                kind: entry.kind,
            });
        }
    }
    let mut moved_dirs: HashMap<&Path, &Path> = HashMap::new();
    for &(from, to, entry) in &renames {
        let implied = from.file_name() == to.file_name()
            && matches!(
                (from.parent(), to.parent()),
                (Some(a), Some(b)) if moved_dirs.get(a) == Some(&b)
            );
        if entry.kind == EntryKind::Dir {
            moved_dirs.insert(from, to);
        }
        if !implied {
            changes.push(Change::Renamed {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                kind: entry.kind,
            });
        }
    }
    for (path, entry) in created {
        changes.push(Change::Created {
            path: path.to_path_buf(),
            kind: entry.kind,
        });
    }
    for path in modified {
        changes.push(Change::Modified {
            path: path.to_path_buf(),
        });
    }
    changes
}

/// A source of changes under one root.
pub trait Watcher {
    /// Waits up to `timeout` for changes and returns them, already applied to
    /// [`Watcher::snapshot`]. Returns an empty list when nothing changed.
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Change>>;

    /// The state the returned changes lead to. Keep it across restarts and
    /// pass it to the next watcher to catch changes made in between.
    fn snapshot(&self) -> &Snapshot;
}

/// Watches with FSEvents on macOS and inotify on Linux. If [`Watcher::poll`]
/// fails with [`Error::Watch`], the platform watcher lost events; continue
/// with a [`RescanWatcher`] built from [`Watcher::snapshot`].
pub struct NativeWatcher {
    root: PathBuf,
    snapshot: Snapshot,
    dirty: BTreeSet<PathBuf>,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
    _watcher: RecommendedWatcher,
}

impl NativeWatcher {
    /// Starts watching `root` recursively. The first poll compares the whole
    /// tree with `previous`; pass `Snapshot::default()` on a first run to have
    /// everything reported as created.
    pub fn start(root: impl AsRef<Path>, previous: Snapshot) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender).map_err(watch_error)?;
        watcher
            .watch(&root, RecursiveMode::Recursive)
            .map_err(watch_error)?;
        Ok(Self {
            root,
            snapshot: previous,
            dirty: BTreeSet::from([PathBuf::new()]),
            events,
            _watcher: watcher,
        })
    }

    fn note(&mut self, event: notify::Result<notify::Event>) -> Result<()> {
        let event = event.map_err(watch_error)?;
        if event.need_rescan() {
            self.dirty.insert(PathBuf::new());
        }
        if let EventKind::Access(access) = event.kind
            && access != AccessKind::Close(AccessMode::Write)
        {
            return Ok(());
        }
        for path in event.paths {
            let rel = path.strip_prefix(&self.root).unwrap_or(Path::new(""));
            self.dirty.insert(rel.to_path_buf());
        }
        Ok(())
    }

    fn receive(&mut self, timeout: Duration) -> Result<bool> {
        match self.events.recv_timeout(timeout) {
            Ok(event) => self.note(event).map(|()| true),
            Err(RecvTimeoutError::Timeout) => Ok(false),
            Err(RecvTimeoutError::Disconnected) => Err(watch_error("the platform watcher stopped")),
        }
    }
}

impl Watcher for NativeWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Change>> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.dirty.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                if !self.receive(left)? {
                    return Ok(Vec::new());
                }
            }
            let settle_until = Instant::now() + MAX_SETTLE;
            while Instant::now() < settle_until && self.receive(SETTLE)? {}
            let changes = self
                .snapshot
                .rescan(&self.root, self.dirty.iter().cloned())?;
            self.dirty.clear();
            if !changes.is_empty() || Instant::now() >= deadline {
                return Ok(changes);
            }
        }
    }

    fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

/// Rescans the whole tree every `interval`. Slower than [`NativeWatcher`] on
/// large trees, but needs nothing from the platform.
pub struct RescanWatcher {
    root: PathBuf,
    snapshot: Snapshot,
    interval: Duration,
}

impl RescanWatcher {
    pub fn new(root: impl AsRef<Path>, previous: Snapshot, interval: Duration) -> Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
            snapshot: previous,
            interval,
        })
    }
}

impl Watcher for RescanWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Change>> {
        let deadline = Instant::now() + timeout;
        loop {
            let changes = self.snapshot.rescan(&self.root, [PathBuf::new()])?;
            let left = deadline.saturating_duration_since(Instant::now());
            if !changes.is_empty() || left.is_zero() {
                return Ok(changes);
            }
            std::thread::sleep(self.interval.min(left));
        }
    }

    fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

fn watch_error(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Error {
    Error::Watch(err.into())
}
