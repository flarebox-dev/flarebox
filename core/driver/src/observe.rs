// SPDX-License-Identifier: Apache-2.0

//! Local observation: what the disk under a linked folder looks like now, as the engine's `Observed` input
//! describes it (design §1): created, modified, renamed and removed entries with their file identities, the
//! `Content` of every file that was created or modified, chunked with `core/chunk`, and the root check of design
//! §6.2 made for the batch.
//!
//! [`FsObserver`] runs the watchers of `core/fs`: inotify on Linux and FSEvents on macOS, which only choose what to
//! rescan, with a periodic full rescan when the platform watcher cannot cover the tree (design §5.1).
//!
//! Excluded paths (design §5.10) are left out, except where a rename crosses the boundary: a rename into an excluded
//! path is reported so the engine can freeze the node at its synced position, and a rename out of one is reported
//! with the content of every file it brings into the sync tree.

use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flarebox_chunk::ChunkerConfig;
use flarebox_fs::{
    Change, Entry, EntryKind, Error as FsError, FileId, Known, NativeWatcher, RescanWatcher,
    RootProblem, Snapshot, Watcher, check_root,
};
use flarebox_proto::v1::{ChunkRef, Content};
use rustix::fs::{Mode, OFlags};

use crate::ignore::Matcher;

/// How often the fallback watcher rescans the whole tree.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);
/// The wait before a file whose content could not be read is read again; it doubles with each failure.
const RETRY_FIRST: Duration = Duration::from_millis(250);
/// The longest wait between two reads of such a file.
const RETRY_MAX: Duration = Duration::from_secs(30);

/// One batch of observations.
#[derive(Debug)]
pub struct Observation {
    pub changes: Vec<Observed>,
    /// The root check of design §6.2, made after the scans of this batch. Removals in a batch whose check failed
    /// are not trusted.
    pub root: Result<(), RootProblem>,
    /// Folders the scans may not read. What is below them is unknown, not absent.
    pub unreadable: Vec<PathBuf>,
    /// The identity of the root folder the observer's scans opened, from the latest poll that scanned. A batch
    /// whose root is not the folder the task locked describes another folder. `None` only before the first scan.
    pub scanned: Option<FileId>,
    /// Files that changed while they were chunked, or that could not be opened or read as the entry the scan saw.
    /// Their changes carry no content. The observer keeps reading them, waiting longer after each failure, and
    /// reports a `Modified` with the content once a read succeeds, unless a later change to the path replaces it
    /// (design §5.8).
    pub busy: Vec<PathBuf>,
}

/// One change, in the terms of `core/fs` (paths relative to the root, previous-state paths first).
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// `content` is set for a file that could be read whole. `linked` is set for a file that another path also
    /// links, a hard link.
    Created {
        path: PathBuf,
        kind: EntryKind,
        id: FileId,
        linked: bool,
        content: Option<Content>,
    },
    Modified {
        path: PathBuf,
        id: FileId,
        content: Option<Content>,
    },
    Renamed {
        from: PathBuf,
        to: PathBuf,
        kind: EntryKind,
        id: FileId,
    },
    Removed {
        path: PathBuf,
        kind: EntryKind,
    },
}

/// What the observer is to believe about one scope, a path relative to the root, before it scans the scope again:
/// the entries L holds at and below it, by kind and identity. The scan then reports how the disk differs from L
/// there, and reads every file L holds there again, since L's content for a file does not prove what the disk holds.
#[derive(Clone, Debug, PartialEq)]
pub struct Reset {
    pub scope: PathBuf,
    pub known: BTreeMap<PathBuf, Known>,
}

/// A source of observations for one linked folder.
pub trait Observer: Send {
    /// Waits up to about `timeout` and returns the next batch, or `None` when nothing changed.
    fn observe(&mut self, timeout: Duration) -> Result<Option<Observation>, FsError>;

    /// The disk as the returned observations leave it. Keep it to start the next observer from.
    fn snapshot(&self) -> &Snapshot;
}

/// Observes a folder through the watchers of `core/fs`.
pub struct FsObserver {
    root: PathBuf,
    space_id: String,
    dev: u64,
    watcher: Box<dyn Watcher + Send>,
    native: bool,
    matcher: Arc<dyn Matcher>,
    /// The root folder the latest scan opened.
    scanned: Option<FileId>,
    /// Files whose content could not be read, with when to read them again.
    unread: BTreeMap<PathBuf, Retry>,
}

#[derive(Clone, Copy, Debug)]
struct Retry {
    failures: u32,
    due: Instant,
}

impl Retry {
    fn after(failures: u32, now: Instant) -> Self {
        let wait = RETRY_FIRST
            .saturating_mul(1 << failures.min(16))
            .min(RETRY_MAX);
        Retry {
            failures,
            due: now + wait,
        }
    }
}

impl FsObserver {
    /// Starts watching the folder of space `space_id` on device `dev`. The first observation compares the whole tree
    /// with `previous`.
    pub fn start(
        root: &Path,
        space_id: &str,
        dev: u64,
        previous: Snapshot,
        matcher: Arc<dyn Matcher>,
    ) -> Result<Self, FsError> {
        let (watcher, native): (Box<dyn Watcher + Send>, bool) = match NativeWatcher::start(
            root,
            previous.clone(),
        ) {
            Ok(watcher) => (Box::new(watcher), true),
            Err(FsError::Watch(error)) => {
                tracing::warn!(%error, "the platform watcher is unavailable; rescanning periodically");
                (
                    Box::new(RescanWatcher::new(root, previous, RESCAN_INTERVAL)?),
                    false,
                )
            }
            Err(error) => return Err(error),
        };
        Ok(FsObserver {
            root: std::fs::canonicalize(root)?,
            space_id: space_id.to_owned(),
            dev,
            watcher,
            native,
            matcher,
            scanned: None,
            unread: BTreeMap::new(),
        })
    }

    /// The root folder the latest scan opened, `None` until a scan has opened it. A folder with nothing to report
    /// gives no [`Observation`], so this is how a caller learns that the first scan is done.
    pub fn scanned(&self) -> Option<FileId> {
        self.scanned
    }

    /// Resets the snapshot at each scope of `resets`, in order, to the entries L holds there, so the next observation
    /// reports how the disk differs from L in those scopes: what L lacks as created, what the disk lacks as removed,
    /// and every file both hold as modified, each file with the content read from the disk. Nothing outside the
    /// scopes is reported again unless it changed.
    pub fn reset(&mut self, resets: Vec<Reset>) {
        for Reset { scope, known } in resets {
            self.unread.retain(|path, _| !path.starts_with(&scope));
            self.watcher.reset(&scope, known);
        }
    }

    fn excluded(&self, path: &Path, kind: EntryKind) -> bool {
        self.matcher.is_excluded(path, kind == EntryKind::Dir)
    }

    /// Adds what `change` means for the engine to `out`.
    fn observed(&self, change: Change, out: &mut Vec<Observed>, busy: &mut Vec<PathBuf>) {
        let entry = |path: &Path| self.watcher.snapshot().entries.get(path).copied();
        match change {
            Change::Created { path, kind } => {
                let Some(entry) = entry(&path) else { return };
                if self.excluded(&path, kind) {
                    return;
                }
                let content = (kind == EntryKind::File)
                    .then(|| self.content(&path, &entry, busy))
                    .flatten();
                out.push(Observed::Created {
                    linked: self.linked(&path, kind),
                    path,
                    kind,
                    id: entry.id,
                    content,
                });
            }
            Change::Modified { path } => {
                let Some(entry) = entry(&path) else { return };
                if entry.kind != EntryKind::File || self.excluded(&path, entry.kind) {
                    return;
                }
                let content = self.content(&path, &entry, busy);
                out.push(Observed::Modified {
                    content,
                    id: entry.id,
                    path,
                });
            }
            Change::Renamed { from, to, kind } => {
                let Some(entry) = entry(&to) else { return };
                let leaves = self.excluded(&from, kind);
                if leaves && self.excluded(&to, kind) {
                    return;
                }
                out.push(Observed::Renamed {
                    from,
                    to: to.clone(),
                    kind,
                    id: entry.id,
                });
                if leaves {
                    self.brought_in(&to, out, busy);
                }
            }
            Change::Removed { path, kind } => {
                if !self.excluded(&path, kind) {
                    out.push(Observed::Removed { path, kind });
                }
            }
        }
    }

    /// Reports what a rename out of an excluded path brought into the sync tree at `to`: the content of a file, or
    /// every included entry under a folder. Nothing was observed there while it was excluded, so edits made in the
    /// meantime arrive this way.
    fn brought_in(&self, to: &Path, out: &mut Vec<Observed>, busy: &mut Vec<PathBuf>) {
        let entries = &self.watcher.snapshot().entries;
        for (path, entry) in entries
            .range(to.to_path_buf()..)
            .take_while(|(path, _)| path.starts_with(to))
        {
            if self.excluded(path, entry.kind) {
                continue;
            }
            let content = (entry.kind == EntryKind::File)
                .then(|| self.content(path, entry, busy))
                .flatten();
            if path == to {
                if entry.kind == EntryKind::File {
                    out.push(Observed::Modified {
                        path: path.clone(),
                        id: entry.id,
                        content,
                    });
                }
                continue;
            }
            out.push(Observed::Created {
                path: path.clone(),
                kind: entry.kind,
                id: entry.id,
                linked: self.linked(path, entry.kind),
                content,
            });
        }
    }

    /// Forgets or moves the unread files that `change` replaces: a new change to a path is read afresh, and a rename
    /// carries the files under it along unless it moves them into an excluded path.
    fn supersede(&mut self, change: &Change) {
        let take = |unread: &mut BTreeMap<PathBuf, Retry>, under: &Path| {
            let paths: Vec<PathBuf> = unread
                .range(under.to_path_buf()..)
                .take_while(|(path, _)| path.starts_with(under))
                .map(|(path, _)| path.clone())
                .collect();
            paths
                .into_iter()
                .map(|path| {
                    let retry = unread.remove(&path).expect("listed above");
                    (path, retry)
                })
                .collect::<Vec<_>>()
        };
        match change {
            Change::Created { path, .. } | Change::Modified { path } => {
                self.unread.remove(path);
            }
            Change::Removed { path, .. } => {
                take(&mut self.unread, path);
            }
            Change::Renamed { from, to, .. } => {
                for (path, retry) in take(&mut self.unread, from) {
                    let rest = path.strip_prefix(from).expect("taken under from");
                    let moved = if rest.as_os_str().is_empty() {
                        to.clone()
                    } else {
                        to.join(rest)
                    };
                    if !self.excluded(&moved, EntryKind::File) {
                        self.unread.insert(moved, retry);
                    }
                }
            }
        }
    }

    /// Reads again the unread files that are due, and reports each one that can now be read.
    fn retry(&mut self, now: Instant, out: &mut Vec<Observed>) {
        let due: Vec<PathBuf> = self
            .unread
            .iter()
            .filter(|(_, retry)| retry.due <= now)
            .map(|(path, _)| path.clone())
            .collect();
        for path in due {
            let entry = self.watcher.snapshot().entries.get(&path).copied();
            let Some(entry) = entry.filter(|entry| entry.kind == EntryKind::File) else {
                self.unread.remove(&path);
                continue;
            };
            match read_content(&self.root, &path, &entry) {
                Ok(Some(content)) => {
                    self.unread.remove(&path);
                    out.push(Observed::Modified {
                        path,
                        id: entry.id,
                        content: Some(content),
                    });
                }
                _ => {
                    let failures = self.unread[&path].failures + 1;
                    self.unread.insert(path, Retry::after(failures, now));
                }
            }
        }
    }

    /// Chunks the file at `path` if it is still the entry the scan saw and does not change while it is read.
    /// True for a file with more than one link. A file that cannot be read is taken to have one.
    fn linked(&self, path: &Path, kind: EntryKind) -> bool {
        kind == EntryKind::File
            && std::fs::symlink_metadata(self.root.join(path)).is_ok_and(|meta| meta.nlink() > 1)
    }

    fn content(&self, path: &Path, entry: &Entry, busy: &mut Vec<PathBuf>) -> Option<Content> {
        match read_content(&self.root, path, entry) {
            Ok(Some(content)) => Some(content),
            Ok(None) => {
                busy.push(path.to_path_buf());
                None
            }
            Err(error) => {
                tracing::debug!(path = %path.display(), %error, "a file could not be chunked");
                busy.push(path.to_path_buf());
                None
            }
        }
    }
}

impl Observer for FsObserver {
    fn observe(&mut self, timeout: Duration) -> Result<Option<Observation>, FsError> {
        let timeout = match self.unread.values().map(|retry| retry.due).min() {
            Some(due) => timeout.min(due.saturating_duration_since(Instant::now())),
            None => timeout,
        };
        let poll = match self.watcher.poll(timeout) {
            Err(FsError::Watch(error)) if self.native => {
                tracing::warn!(%error, "the platform watcher failed; rescanning periodically");
                let snapshot = self.watcher.snapshot().clone();
                self.watcher = Box::new(RescanWatcher::new(&self.root, snapshot, RESCAN_INTERVAL)?);
                self.native = false;
                return Ok(None);
            }
            poll => poll?,
        };
        if poll.root.is_some() {
            self.scanned = poll.root;
        }
        let now = Instant::now();
        let mut changes = Vec::new();
        let mut busy = Vec::new();
        let observed = !poll.changes.is_empty();
        for change in poll.changes {
            self.supersede(&change);
            self.observed(change, &mut changes, &mut busy);
        }
        self.retry(now, &mut changes);
        for path in &busy {
            self.unread.insert(path.clone(), Retry::after(0, now));
        }
        if !observed && changes.is_empty() && poll.unreadable.is_empty() {
            return Ok(None);
        }
        let root = check_root(&self.root, &self.space_id, self.dev).map(|_| ());
        Ok(Some(Observation {
            changes,
            root,
            unreadable: poll.unreadable,
            scanned: self.scanned,
            busy,
        }))
    }

    fn snapshot(&self) -> &Snapshot {
        self.watcher.snapshot()
    }
}

/// The content of the file at `path` under `root`, or `None` when it is not the file the scan saw or changed while
/// it was read.
fn read_content(root: &Path, path: &Path, entry: &Entry) -> io::Result<Option<Content>> {
    let file = open_beneath(root, path)?;
    let before = file.metadata()?;
    let same_file =
        before.is_file() && before.dev() == entry.id.dev && before.ino() == entry.id.ino;
    if !same_file || !same_version(&before, entry) {
        return Ok(None);
    }
    let config = ChunkerConfig::V0;
    let chunks = flarebox_chunk::chunks(&file, config)
        .map(|chunk| chunk.map(|chunk| chunk.chunk_ref()))
        .collect::<io::Result<Vec<ChunkRef>>>()?;
    let after = file.metadata()?;
    if !same_version(&after, entry) {
        return Ok(None);
    }
    Ok(Some(flarebox_chunk::content(
        chunks,
        mtime_ns(&after) as i64,
        entry.executable,
        &config,
    )))
}

fn mtime_ns(meta: &std::fs::Metadata) -> i128 {
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

fn same_version(meta: &std::fs::Metadata, entry: &Entry) -> bool {
    meta.size() == entry.size && mtime_ns(meta) == entry.mtime_ns
}

/// Opens the file at the relative `path` under `root` for reading, one folder at a time, following no symbolic link.
fn open_beneath(root: &Path, path: &Path) -> io::Result<File> {
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    open_under(rustix::fs::open(root, dir_flags, Mode::empty())?, path)
}

/// Opens the file at the relative `path` under the open folder `root` for reading, one folder at a time, following
/// no symbolic link.
pub(crate) fn open_under(root: impl AsFd, path: &Path) -> io::Result<File> {
    let dir_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let mut dir: OwnedFd = rustix::fs::openat(&root, ".", dir_flags, Mode::empty())?;
    let mut parts = path.components().peekable();
    while let Some(part) = parts.next() {
        let Component::Normal(name) = part else {
            break;
        };
        if parts.peek().is_none() {
            let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
            return Ok(File::from(rustix::fs::openat(
                &dir,
                name,
                flags,
                Mode::empty(),
            )?));
        }
        dir = rustix::fs::openat(&dir, name, dir_flags | OFlags::NOFOLLOW, Mode::empty())?;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{} is not a relative file path", path.display()),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::time::Instant;

    use flarebox_fs::{Marker, write_marker};

    use super::*;
    use crate::ignore::Unfiltered;

    const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

    fn observer(root: &Path) -> FsObserver {
        let dev = fs::metadata(root).unwrap().dev();
        FsObserver::start(root, SPACE, dev, Snapshot::default(), Arc::new(Unfiltered)).unwrap()
    }

    fn marked() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_marker(
            dir.path(),
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        dir
    }

    /// Observes until `done` accepts the changes seen so far, for at most ten seconds.
    fn observe_until(
        observer: &mut FsObserver,
        done: impl Fn(&[Observed]) -> bool,
    ) -> (Vec<Observed>, Observation) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            assert!(Instant::now() < deadline, "not observed in time: {seen:?}");
            if let Some(observation) = observer.observe(Duration::from_millis(500)).unwrap() {
                seen.extend(observation.changes.iter().cloned());
                if done(&seen) {
                    return (seen, observation);
                }
            }
        }
    }

    #[test]
    fn the_first_observation_reports_every_entry_with_file_contents() {
        let dir = marked();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs/a.txt"), b"hello").unwrap();
        fs::write(dir.path().join("empty"), b"").unwrap();
        let mut observer = observer(dir.path());
        let (seen, last) = observe_until(&mut observer, |seen| seen.len() >= 3);
        assert!(last.root.is_ok(), "{:?}", last.root);

        let created = |name: &str| {
            seen.iter()
                .find_map(|change| match change {
                    Observed::Created {
                        path,
                        kind,
                        content,
                        ..
                    } if path == Path::new(name) => Some((*kind, content.clone())),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{name} not created in {seen:?}"))
        };
        assert_eq!(created("docs"), (EntryKind::Dir, None));
        let (kind, content) = created("docs/a.txt");
        assert_eq!(kind, EntryKind::File);
        let content = content.unwrap();
        assert_eq!(content.size, 5);
        assert_eq!(content.chunks.len(), 1);
        assert_eq!(
            content.chunks[0].hash,
            flarebox_chunk::chunk_hash(b"hello").to_vec()
        );
        assert_eq!(created("empty").1.unwrap().chunks.len(), 0);
        assert!(
            seen.iter().all(|change| !matches!(change, Observed::Created { path, .. } if path.starts_with(".flarebox"))),
            "{seen:?}"
        );
    }

    #[test]
    fn renames_keep_the_identity_and_edits_carry_new_content() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());
        let id = fs::metadata(dir.path().join("a.txt")).unwrap().ino();

        fs::rename(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        assert!(
            seen.iter().any(|change| matches!(
                change,
                Observed::Renamed { from, to, id: renamed, .. }
                    if from == Path::new("a.txt") && to == Path::new("b.txt") && renamed.ino == id
            )),
            "{seen:?}"
        );

        fs::write(dir.path().join("b.txt"), b"two two").unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let content = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { content, .. } => content.clone(),
                _ => None,
            })
            .unwrap();
        assert_eq!(content.size, 7);
    }

    #[test]
    fn a_batch_without_its_marker_fails_the_root_check() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());

        fs::remove_dir_all(dir.path().join(".flarebox")).unwrap();
        fs::remove_file(dir.path().join("a.txt")).unwrap();
        let (_, last) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Removed { .. }))
        });
        assert!(
            matches!(last.root, Err(RootProblem::NoMarker)),
            "{:?}",
            last.root
        );
    }

    #[test]
    fn a_file_replaced_after_the_scan_is_not_chunked() {
        let dir = marked();
        fs::write(dir.path().join("a.txt"), b"one").unwrap();
        let mut snapshot = Snapshot::default();
        snapshot.rescan(dir.path(), [PathBuf::new()]).unwrap();
        let entry = snapshot.entries[Path::new("a.txt")];
        assert!(
            read_content(dir.path(), Path::new("a.txt"), &entry)
                .unwrap()
                .is_some()
        );

        fs::remove_file(dir.path().join("a.txt")).unwrap();
        fs::write(dir.path().join("a.txt"), b"other bytes").unwrap();
        assert_eq!(
            read_content(dir.path(), Path::new("a.txt"), &entry).unwrap(),
            None
        );
    }

    #[test]
    fn symbolic_links_are_not_followed() {
        let dir = marked();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        assert!(open_beneath(dir.path(), Path::new("link/secret")).is_err());
        std::os::unix::fs::symlink(outside.path().join("secret"), dir.path().join("file")).unwrap();
        assert!(open_beneath(dir.path(), Path::new("file")).is_err());
        assert!(open_beneath(dir.path(), Path::new("../x")).is_err());
    }
    #[test]
    fn a_file_that_could_not_be_read_is_read_again_once_it_can_be() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        let file = dir.path().join("locked");
        fs::write(&file, b"content").unwrap();
        fs::write(dir.path().join("gone"), b"x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        fs::set_permissions(dir.path().join("gone"), fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        let (seen, _) = observe_until(&mut observer, |seen| seen.len() >= 2);
        assert!(
            seen.iter()
                .all(|change| matches!(change, Observed::Created { content: None, .. })),
            "{seen:?}"
        );
        assert_eq!(observer.unread.len(), 2);

        fs::remove_file(dir.path().join("gone")).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
                && seen
                    .iter()
                    .any(|change| matches!(change, Observed::Removed { .. }))
        });
        let content = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { path, content, .. } if path == Path::new("locked") => {
                    content.clone()
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("no content in {seen:?}"));
        assert_eq!(content.size, 7);
        assert!(observer.unread.is_empty());
    }

    #[test]
    fn an_unread_file_that_is_renamed_is_read_at_its_new_path() {
        use std::os::unix::fs::PermissionsExt;

        let dir = marked();
        let file = dir.path().join("locked");
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read(&file).is_ok() {
            assert!(rustix::process::geteuid().is_root());
            eprintln!("SKIPPED: running as root, so permissions do not block reads");
            return;
        }
        let mut observer = observer(dir.path());
        observe_until(&mut observer, |seen| !seen.is_empty());
        assert_eq!(observer.unread.len(), 1);

        fs::rename(&file, dir.path().join("moved")).unwrap();
        observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Renamed { .. }))
        });
        fs::set_permissions(dir.path().join("moved"), fs::Permissions::from_mode(0o600)).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            seen.iter()
                .any(|change| matches!(change, Observed::Modified { .. }))
        });
        let path = seen
            .iter()
            .find_map(|change| match change {
                Observed::Modified { path, .. } => Some(path.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(path.as_os_str(), "moved");
    }

    #[test]
    fn renames_across_an_exclusion_reach_the_engine_both_ways() {
        struct IgnoredFolder;
        impl Matcher for IgnoredFolder {
            fn is_excluded(&self, path: &Path, _dir: bool) -> bool {
                path.starts_with("ignored")
            }
        }
        let dir = marked();
        let root = dir.path();
        fs::create_dir_all(root.join("ignored/folder")).unwrap();
        fs::write(root.join("ignored/folder/inner"), b"inner").unwrap();
        fs::write(root.join("tracked"), b"one").unwrap();
        let dev = fs::metadata(root).unwrap().dev();
        let mut observer = FsObserver::start(
            root,
            SPACE,
            dev,
            Snapshot::default(),
            Arc::new(IgnoredFolder),
        )
        .unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| !seen.is_empty());
        assert_eq!(seen.len(), 1, "{seen:?}");

        let renamed = |seen: &[Observed], from: &str, to: &str| {
            seen.iter().any(|change| {
                matches!(change, Observed::Renamed { from: f, to: t, .. } if f == Path::new(from) && t == Path::new(to))
            })
        };
        fs::rename(root.join("tracked"), root.join("ignored/tracked")).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            renamed(seen, "tracked", "ignored/tracked")
        });
        assert_eq!(seen.len(), 1, "{seen:?}");

        fs::write(root.join("ignored/tracked"), b"edited").unwrap();
        fs::rename(root.join("ignored/tracked"), root.join("back")).unwrap();
        fs::rename(root.join("ignored/folder"), root.join("folder")).unwrap();
        let (seen, _) = observe_until(&mut observer, |seen| {
            renamed(seen, "ignored/tracked", "back") && renamed(seen, "ignored/folder", "folder")
        });
        let content = |name: &str| {
            seen.iter()
                .find_map(|change| match change {
                    Observed::Modified { path, content, .. }
                    | Observed::Created { path, content, .. }
                        if path == Path::new(name) =>
                    {
                        content.clone()
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no content for {name} in {seen:?}"))
        };
        assert_eq!(content("back").size, 6);
        assert_eq!(content("folder/inner").size, 5);
        assert!(
            seen.iter()
                .all(|change| !matches!(change, Observed::Removed { .. })),
            "{seen:?}"
        );
    }
}
