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
//! Scans never trust a path spelling or follow a link. They walk the tree
//! through directory handles opened without following symbolic links, take
//! names from directory listings rather than from event paths, and check that
//! each directory they enter is the one they listed. A scan that sees its root
//! disappear or a directory change under it publishes nothing.
//!
//! A scanner in userspace cannot take an atomic snapshot of a tree that keeps
//! changing, so the guarantee is eventual consistency: once the tree is quiet,
//! a rescan reports it exactly. While it changes, no removal is published for
//! a path that exists when the scan is published. Before publishing, every
//! path the scan saw disappear is looked up again from the root without
//! following links; one that is present or cannot be read is kept and handed
//! back for another rescan. What remains is the window between that lookup and
//! the caller acting on the change, which the sync engine's mass-delete guard
//! and the server trash cover.
//!
//! Symbolic links and special files are not synced in v0 (spec §5.1) and are
//! skipped.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use notify::event::{AccessKind, AccessMode, EventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};
use rustix::fs::{FileType, Mode, OFlags, RawMode};

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
    /// A directory was replaced or removed while the scan was inside it. The
    /// snapshot is unchanged; scan again once the tree is quiet.
    #[error("the tree changed during the scan")]
    Changed,
    /// The root path now names a different directory than the one scanned.
    #[error("the root directory was replaced during the scan")]
    RootReplaced,
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
    pub birth_ns: Option<i64>,
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

/// What `stat` reports about one directory entry, before deciding whether it is
/// something the snapshot records.
struct Stat {
    file_type: FileType,
    id: FileId,
    size: u64,
    mtime_ns: i128,
    mode: u32,
}

impl Stat {
    fn entry(&self) -> Option<Entry> {
        Some(match self.file_type {
            FileType::Directory => Entry {
                kind: EntryKind::Dir,
                id: self.id,
                size: 0,
                mtime_ns: 0,
                executable: false,
            },
            FileType::RegularFile => Entry {
                kind: EntryKind::File,
                id: self.id,
                size: self.size,
                mtime_ns: self.mtime_ns,
                executable: self.mode & 0o111 != 0,
            },
            _ => return None,
        })
    }

    fn same_inode(&self, id: &FileId) -> bool {
        self.id.dev == id.dev && self.id.ino == id.ino
    }
}

#[cfg(target_os = "linux")]
fn statx(dir: BorrowedFd<'_>, name: &OsStr, flags: rustix::fs::AtFlags) -> io::Result<Stat> {
    use rustix::fs::StatxFlags;
    let st = rustix::fs::statx(
        dir,
        name,
        flags,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    let nanos = |sec: i64, nsec: u32| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    let birth_ns = (st.stx_mask & StatxFlags::BTIME.bits() != 0)
        .then(|| i64::try_from(nanos(st.stx_btime.tv_sec, st.stx_btime.tv_nsec)).ok())
        .flatten();
    Ok(Stat {
        file_type: FileType::from_raw_mode(RawMode::from(st.stx_mode)),
        id: FileId {
            dev: rustix::fs::makedev(st.stx_dev_major, st.stx_dev_minor),
            ino: st.stx_ino,
            birth_ns,
        },
        size: st.stx_size,
        mtime_ns: nanos(st.stx_mtime.tv_sec, st.stx_mtime.tv_nsec),
        mode: u32::from(st.stx_mode),
    })
}

/// Stats `name` inside `dir` without following a symbolic link.
#[cfg(target_os = "linux")]
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    statx(dir, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
}

#[cfg(target_os = "linux")]
fn stat_fd(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    statx(fd, OsStr::new(""), rustix::fs::AtFlags::EMPTY_PATH)
}

#[cfg(not(target_os = "linux"))]
fn from_stat(st: rustix::fs::Stat) -> Stat {
    let nanos = |sec: i64, nsec: i64| i128::from(sec) * 1_000_000_000 + i128::from(nsec);
    Stat {
        file_type: FileType::from_raw_mode(RawMode::from(st.st_mode)),
        id: FileId {
            dev: st.st_dev as u64,
            ino: st.st_ino,
            birth_ns: i64::try_from(nanos(st.st_birthtime, st.st_birthtime_nsec)).ok(),
        },
        size: st.st_size as u64,
        mtime_ns: nanos(st.st_mtime, st.st_mtime_nsec),
        mode: u32::from(st.st_mode),
    }
}

/// Stats `name` inside `dir` without following a symbolic link.
#[cfg(not(target_os = "linux"))]
fn stat_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Stat> {
    Ok(from_stat(rustix::fs::statat(
        dir,
        name,
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

#[cfg(not(target_os = "linux"))]
fn stat_fd(fd: BorrowedFd<'_>) -> io::Result<Stat> {
    Ok(from_stat(rustix::fs::fstat(fd)?))
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

fn open_root(root: &Path) -> io::Result<OwnedFd> {
    Ok(rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}

/// Opens the directory `name` inside `dir`, refusing to follow a symbolic link.
fn open_dir_at(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    Ok(rustix::fs::openat(dir, name, DIR_FLAGS, Mode::empty())?)
}

/// Whether the error means the name no longer leads to a directory: it is gone,
/// or a file or symbolic link took its place.
fn is_gone(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || err.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

fn list(dir: BorrowedFd<'_>) -> io::Result<BTreeSet<OsString>> {
    let mut names = BTreeSet::new();
    for item in rustix::fs::Dir::read_from(dir)? {
        let name = item?.file_name().to_bytes().to_vec();
        if name != b"." && name != b".." {
            names.insert(OsString::from(OsStr::from_bytes(&name)));
        }
    }
    Ok(names)
}

/// Where a scan can be interrupted in tests.
#[cfg_attr(not(test), allow(dead_code))]
enum Cut<'a> {
    RootOpened,
    OpeningDir(&'a Path),
}

/// The entries one rescan observed, and the parts of the snapshot they replace.
struct Scan<'c> {
    new: BTreeMap<PathBuf, Entry>,
    /// Every folder the scan opened, with the identity it had when opened.
    /// Each must still be at its path when the scan is published.
    bindings: Vec<(PathBuf, FileId)>,
    /// Paths whose old entry and everything below it give way to `new`.
    replaced: Vec<PathBuf>,
    cut: &'c mut dyn FnMut(Cut<'_>),
}

impl Scan<'_> {
    /// Records the entry `name` of `dir` at `rel`, and everything below it.
    fn entry(&mut self, dir: BorrowedFd<'_>, name: &OsStr, rel: PathBuf) -> Result<()> {
        let stat = match stat_at(dir, name) {
            Ok(stat) => stat,
            Err(err) if is_gone(&err) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let Some(entry) = stat.entry() else {
            return Ok(());
        };
        self.new.insert(rel.clone(), entry);
        if entry.kind == EntryKind::File {
            return Ok(());
        }
        (self.cut)(Cut::OpeningDir(&rel));
        let sub = match open_dir_at(dir, name) {
            Ok(sub) => sub,
            Err(err) if is_gone(&err) => return Err(Error::Changed),
            Err(err) => return Err(err.into()),
        };
        if !stat_fd(sub.as_fd())?.same_inode(&entry.id) {
            return Err(Error::Changed);
        }
        self.bindings.push((rel.clone(), entry.id));
        self.dir(sub.as_fd(), &rel)
    }

    fn dir(&mut self, dir: BorrowedFd<'_>, rel: &Path) -> Result<()> {
        for name in list(dir)? {
            self.entry(dir, &name, rel.join(&name))?;
        }
        Ok(())
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

/// The known state of every entry under a root, keyed by relative path with the
/// exact names the directory listings return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub entries: BTreeMap<PathBuf, Entry>,
}

/// What one [`Snapshot::rescan`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rescan {
    /// The changes, already applied to the snapshot.
    pub changes: Vec<Change>,
    /// Paths the scan saw disappear that were present, or could not be read,
    /// when it was published. The snapshot keeps what it knew about them and
    /// everything below them; rescan them later.
    pub unsettled: Vec<PathBuf>,
}

impl Snapshot {
    pub fn scan(root: impl AsRef<Path>) -> Result<Self> {
        let mut snapshot = Self::default();
        snapshot.rescan(root, [PathBuf::new()])?;
        Ok(snapshot)
    }

    /// Rescans the given relative paths, each with everything below it, updates
    /// the snapshot and returns what changed. An empty path rescans the whole
    /// tree.
    ///
    /// A path is looked up in a listing of its parent, so a spelling the file
    /// system resolves case- or normalization-insensitively counts as absent,
    /// and the listing also reveals entries of that parent the snapshot knows
    /// by a name that is gone. A path whose parent the snapshot does not know
    /// as a folder, or whose parent is no longer that folder, is widened to the
    /// parent.
    ///
    /// On any error the snapshot is unchanged. That includes a root that is
    /// missing, unreadable or replaced at any point of the scan, so a lost root
    /// never reads as everything removed, and [`Error::Changed`] when a folder
    /// is replaced while the scan is inside it.
    ///
    /// A path is only reported removed, or as the source of a rename, if a
    /// fresh lookup from the root confirms it is gone when the scan is
    /// published. Otherwise it keeps its old entry and is returned in
    /// [`Rescan::unsettled`].
    pub fn rescan(
        &mut self,
        root: impl AsRef<Path>,
        paths: impl IntoIterator<Item = PathBuf>,
    ) -> Result<Rescan> {
        self.rescan_with(root.as_ref(), paths, &mut |_| {})
    }

    fn rescan_with(
        &mut self,
        root: &Path,
        paths: impl IntoIterator<Item = PathBuf>,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> Result<Rescan> {
        let root_dir = open_root(root)?;
        let root_stat = stat_fd(root_dir.as_fd())?;
        cut(Cut::RootOpened);

        let mut scopes = Vec::new();
        for path in paths {
            if path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a relative path inside the root: {}", path.display()),
                )
                .into());
            }
            scopes.push(self.anchor(path));
        }

        let mut scan = Scan {
            new: BTreeMap::new(),
            bindings: Vec::new(),
            replaced: Vec::new(),
            cut,
        };
        let scopes = outermost(scopes);
        if scopes
            .first()
            .is_some_and(|scope| scope.as_os_str().is_empty())
        {
            scan.replaced.push(PathBuf::new());
            scan.dir(root_dir.as_fd(), Path::new(""))?;
        } else {
            let mut resolved = Vec::new();
            let mut pending = scopes;
            while let Some(scope) = pending.pop() {
                let parent = scope.parent().unwrap_or(Path::new(""));
                if parent.as_os_str().is_empty()
                    || self.open_known_dir(&root_dir, parent)?.is_some()
                {
                    resolved.push(scope);
                } else {
                    pending.push(parent.to_path_buf());
                }
            }
            let mut groups: BTreeMap<PathBuf, BTreeSet<OsString>> = BTreeMap::new();
            for scope in outermost(resolved) {
                let name = scope
                    .file_name()
                    .expect("scopes are non-empty")
                    .to_os_string();
                let parent = scope.parent().unwrap_or(Path::new("")).to_path_buf();
                groups.entry(parent).or_default().insert(name);
            }
            for (parent, focus) in groups {
                let dir = if parent.as_os_str().is_empty() {
                    root_dir.try_clone()?
                } else {
                    let (dir, chain) = self
                        .open_known_dir(&root_dir, &parent)?
                        .ok_or(Error::Changed)?;
                    scan.bindings.extend(chain);
                    dir
                };
                self.scan_children(&mut scan, dir.as_fd(), &parent, &focus)?;
            }
        }

        let again = open_root(root)?;
        if !stat_fd(again.as_fd())?.same_inode(&root_stat.id) {
            return Err(Error::RootReplaced);
        }
        revalidate(again.as_fd(), scan.bindings)?;

        let mut old = BTreeMap::new();
        for prefix in &scan.replaced {
            old.extend(
                self.entries
                    .range(prefix.clone()..)
                    .take_while(|(path, _)| path.starts_with(prefix))
                    .map(|(path, entry)| (path.clone(), *entry)),
            );
        }
        let unsettled = keep_unconfirmed(again.as_fd(), &old, &mut scan.new);
        let changes = diff(&old, &scan.new);
        for path in old.keys() {
            self.entries.remove(path);
        }
        self.entries.extend(scan.new);
        Ok(Rescan { changes, unsettled })
    }

    /// Rescans the children of the known folder `parent`: every name in
    /// `focus`, every listed name the snapshot does not know, and every known
    /// child the listing no longer contains.
    fn scan_children(
        &self,
        scan: &mut Scan<'_>,
        dir: BorrowedFd<'_>,
        parent: &Path,
        focus: &BTreeSet<OsString>,
    ) -> Result<()> {
        let names = list(dir)?;
        for name in &names {
            let child = parent.join(name);
            if focus.contains(name) || !self.entries.contains_key(&child) {
                scan.replaced.push(child.clone());
                scan.entry(dir, name, child)?;
            }
        }
        for name in focus.difference(&names) {
            scan.replaced.push(parent.join(name));
        }
        let known_children = self
            .entries
            .range(parent.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(parent))
            .filter(|(path, _)| path.parent() == Some(parent))
            .filter(|(path, _)| !path.file_name().is_some_and(|name| names.contains(name)));
        for (path, _) in known_children {
            scan.replaced.push(path.clone());
        }
        Ok(())
    }

    /// Opens the folder the snapshot knows at `rel`, one component at a time,
    /// each one listed under its exact name and opened without following links.
    /// Returns the folder with every folder opened on the way and its identity,
    /// or `None` if that path no longer leads to the same folder.
    #[allow(clippy::type_complexity)]
    fn open_known_dir(
        &self,
        root: &OwnedFd,
        rel: &Path,
    ) -> Result<Option<(OwnedFd, Vec<(PathBuf, FileId)>)>> {
        let Some(known) = self.entries.get(rel).filter(|e| e.kind == EntryKind::Dir) else {
            return Ok(None);
        };
        let mut dir = root.try_clone()?;
        let mut path = PathBuf::new();
        let mut chain = Vec::new();
        for component in rel.components() {
            if !list(dir.as_fd())?.contains(component.as_os_str()) {
                return Ok(None);
            }
            dir = match open_dir_at(dir.as_fd(), component.as_os_str()) {
                Ok(next) => next,
                Err(err) if is_gone(&err) => return Ok(None),
                Err(err) => return Err(err.into()),
            };
            path.push(component);
            chain.push((path.clone(), stat_fd(dir.as_fd())?.id));
        }
        let same = chain
            .last()
            .is_some_and(|(_, id)| id.dev == known.id.dev && id.ino == known.id.ino);
        Ok(same.then_some((dir, chain)))
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

/// Checks that every folder the scan opened is still the folder at its path,
/// opening each again from the root without following links. A folder that
/// was moved, replaced or made unreadable after the scan opened it means the
/// scan may have read a detached copy, so nothing it saw is published.
fn revalidate(root: BorrowedFd<'_>, mut bindings: Vec<(PathBuf, FileId)>) -> Result<()> {
    bindings.sort_by(|a, b| a.0.cmp(&b.0));
    bindings.dedup_by(|a, b| a.0 == b.0);
    let mut open: Vec<(PathBuf, OwnedFd)> = Vec::new();
    for (path, id) in bindings {
        while open.last().is_some_and(|(dir, _)| !path.starts_with(dir)) {
            open.pop();
        }
        let (parent_path, parent) = match open.last() {
            Some((dir, fd)) => (dir.as_path(), fd.as_fd()),
            None => (Path::new(""), root),
        };
        let (Some(name), true) = (path.file_name(), path.parent() == Some(parent_path)) else {
            return Err(Error::Changed);
        };
        let dir = open_dir_at(parent, name).map_err(|_| Error::Changed)?;
        if !stat_fd(dir.as_fd()).is_ok_and(|stat| stat.same_inode(&id)) {
            return Err(Error::Changed);
        }
        open.push((path, dir));
    }
    Ok(())
}

/// Keeps the old entries of every path the scan saw disappear, and of
/// everything below it, unless a fresh lookup from the root confirms it is gone
/// now, and returns the paths kept. Folder identities can match again after a
/// folder was swapped out and back while the scan read the detached copy, so an
/// absence seen during the scan is only published once the attached tree
/// confirms it. A path that is present or cannot be read stays as it was known.
fn keep_unconfirmed(
    root: BorrowedFd<'_>,
    old: &BTreeMap<PathBuf, Entry>,
    new: &mut BTreeMap<PathBuf, Entry>,
) -> Vec<PathBuf> {
    let gone: Vec<(PathBuf, EntryKind)> = old
        .iter()
        .filter(|(path, before)| {
            !new.get(*path)
                .is_some_and(|after| after.kind == before.kind)
        })
        .map(|(path, before)| (path.clone(), before.kind))
        .collect();
    let mut listings = HashMap::new();
    let mut kept = Vec::new();
    let mut checked: Option<PathBuf> = None;
    for (path, kind) in gone {
        if checked.as_ref().is_some_and(|done| path.starts_with(done)) {
            continue;
        }
        checked = Some(path.clone());
        if confirmed_gone(root, &path, kind, &mut listings) {
            continue;
        }
        new.retain(|inside, _| !inside.starts_with(&path));
        new.extend(
            old.range(path.clone()..)
                .take_while(|(inside, _)| inside.starts_with(&path))
                .map(|(inside, entry)| (inside.clone(), *entry)),
        );
        kept.push(path);
    }
    kept
}

/// Whether `path` no longer holds an entry of `kind`: its parent, reached from
/// the root without following links, does not list it under that exact name or
/// holds something else there. `listings` caches each parent's names, with
/// `None` for a parent that cannot be read.
fn confirmed_gone(
    root: BorrowedFd<'_>,
    path: &Path,
    kind: EntryKind,
    listings: &mut HashMap<PathBuf, Option<BTreeSet<OsString>>>,
) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let names =
        listings
            .entry(parent.to_path_buf())
            .or_insert_with(|| match open_attached(root, parent) {
                Ok(Some(dir)) => list(dir.as_fd()).ok(),
                Ok(None) => Some(BTreeSet::new()),
                Err(_) => None,
            });
    match names {
        None => false,
        Some(names) if !names.contains(name) => true,
        Some(_) => match open_attached(root, parent) {
            Ok(Some(dir)) => match stat_at(dir.as_fd(), name) {
                Ok(stat) => !stat.entry().is_some_and(|entry| entry.kind == kind),
                Err(err) => is_gone(&err),
            },
            Ok(None) => true,
            Err(_) => false,
        },
    }
}

/// Opens the folder at `rel` under the root one component at a time without
/// following links, or returns `None` if no folder is there.
fn open_attached(root: BorrowedFd<'_>, rel: &Path) -> io::Result<Option<OwnedFd>> {
    let mut dir = root.try_clone_to_owned()?;
    for component in rel.components() {
        dir = match open_dir_at(dir.as_fd(), component.as_os_str()) {
            Ok(next) => next,
            Err(err) if is_gone(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
    }
    Ok(Some(dir))
}

/// Sorts the paths and drops every path inside another one.
fn outermost(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let mut outermost: Vec<PathBuf> = Vec::new();
    for path in paths {
        if !outermost.last().is_some_and(|last| path.starts_with(last)) {
            outermost.push(path);
        }
    }
    outermost
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

/// Watches with FSEvents on macOS and inotify on Linux.
///
/// When the platform reports lost events (an inotify queue overflow, or
/// FSEvents asking for a subtree rescan), the watcher re-registers the whole
/// root before rescanning it, so no folder found by the rescan is left
/// unwatched. On Linux it also registers each new or moved folder itself and
/// rescans it after the registration, so a write between the folder's creation
/// and its watch is not missed. If [`Watcher::poll`] fails with
/// [`Error::Watch`], the platform watcher cannot cover the tree (for example
/// the inotify watch limit); continue with a [`RescanWatcher`] built from
/// [`Watcher::snapshot`].
pub struct NativeWatcher {
    root: PathBuf,
    snapshot: Snapshot,
    dirty: BTreeSet<PathBuf>,
    rewatch: bool,
    failed: Option<Error>,
    sender: mpsc::Sender<notify::Result<notify::Event>>,
    events: mpsc::Receiver<notify::Result<notify::Event>>,
    watcher: RecommendedWatcher,
    #[cfg(all(test, target_os = "linux"))]
    before_register: Option<Box<dyn FnMut()>>,
}

impl NativeWatcher {
    /// Starts watching `root` recursively. The first poll compares the whole
    /// tree with `previous`; pass `Snapshot::default()` on a first run to have
    /// everything reported as created.
    pub fn start(root: impl AsRef<Path>, previous: Snapshot) -> Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let (sender, events) = mpsc::channel();
        let watcher = watch_root(&root, &sender)?;
        Ok(Self {
            root,
            snapshot: previous,
            dirty: BTreeSet::from([PathBuf::new()]),
            rewatch: false,
            failed: None,
            sender,
            events,
            watcher,
            #[cfg(all(test, target_os = "linux"))]
            before_register: None,
        })
    }

    fn note(&mut self, event: notify::Result<notify::Event>) -> Result<()> {
        let event = event.map_err(watch_error)?;
        if event.need_rescan() {
            self.rewatch = true;
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

    /// Registers watches on the folders the changes created or moved, then
    /// rescans them, so anything written before the watch existed is seen.
    #[cfg(target_os = "linux")]
    fn cover(&mut self, changes: &[Change]) -> Vec<Change> {
        let dirs = outermost(
            changes
                .iter()
                .filter_map(|change| match change {
                    Change::Created {
                        path,
                        kind: EntryKind::Dir,
                    }
                    | Change::Renamed {
                        to: path,
                        kind: EntryKind::Dir,
                        ..
                    } => Some(path.clone()),
                    _ => None,
                })
                .collect(),
        );
        if dirs.is_empty() {
            return Vec::new();
        }
        #[cfg(test)]
        if let Some(hook) = self.before_register.as_mut() {
            hook();
        }
        for dir in &dirs {
            let path = self.root.join(dir);
            if let Err(err) = self.watcher.watch(&path, RecursiveMode::Recursive) {
                let still_a_dir = std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir());
                if still_a_dir {
                    self.failed = Some(watch_error(err));
                }
            }
        }
        match self.snapshot.rescan(&self.root, dirs.iter().cloned()) {
            Ok(rescan) => {
                self.dirty.extend(rescan.unsettled);
                rescan.changes
            }
            Err(Error::Changed) => {
                self.dirty.extend(dirs);
                Vec::new()
            }
            Err(err) => {
                self.failed = Some(err);
                Vec::new()
            }
        }
    }
}

fn watch_root(
    root: &Path,
    sender: &mpsc::Sender<notify::Result<notify::Event>>,
) -> Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(sender.clone()).map_err(watch_error)?;
    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(watch_error)?;
    Ok(watcher)
}

impl Watcher for NativeWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Change>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(err) = self.failed.take() {
                return Err(err);
            }
            if self.dirty.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                if !self.receive(left)? {
                    return Ok(Vec::new());
                }
            }
            let settle_until = Instant::now() + MAX_SETTLE;
            while Instant::now() < settle_until && self.receive(SETTLE)? {}
            if self.rewatch {
                self.watcher = watch_root(&self.root, &self.sender)?;
                self.rewatch = false;
            }
            let rescan = match self.snapshot.rescan(&self.root, self.dirty.iter().cloned()) {
                Ok(rescan) => rescan,
                Err(Error::Changed) if Instant::now() < deadline => continue,
                Err(Error::Changed) => return Ok(Vec::new()),
                Err(err) => return Err(err),
            };
            self.dirty = rescan.unsettled.into_iter().collect();
            let changes = rescan.changes;
            #[cfg(target_os = "linux")]
            let changes = {
                let mut changes = changes;
                let more = self.cover(&changes);
                changes.extend(more);
                changes
            };
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
            root: std::fs::canonicalize(root)?,
            snapshot: previous,
            interval,
        })
    }
}

impl Watcher for RescanWatcher {
    fn poll(&mut self, timeout: Duration) -> Result<Vec<Change>> {
        let deadline = Instant::now() + timeout;
        loop {
            let changes = match self.snapshot.rescan(&self.root, [PathBuf::new()]) {
                Ok(rescan) => rescan.changes,
                Err(Error::Changed) => Vec::new(),
                Err(err) => return Err(err),
            };
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

#[cfg(test)]
mod tests;
