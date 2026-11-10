// SPDX-License-Identifier: Apache-2.0

//! Local observation: what the disk under a linked folder looks like now, as the engine's `Observed` input
//! describes it (design §1): created, modified, renamed and removed entries with their file identities, the
//! `Content` of every file that was created or modified, chunked with `core/chunk`, and the root check of design
//! §6.2 made for the batch.
//!
//! [`FsObserver`] runs the watchers of `core/fs`: inotify on Linux and FSEvents on macOS, which only choose what to
//! rescan, with a periodic full rescan when the platform watcher cannot cover the tree (design §5.1).

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use flarebox_chunk::ChunkerConfig;
use flarebox_fs::{
    Change, Entry, EntryKind, Error as FsError, FileId, NativeWatcher, RescanWatcher, RootProblem,
    Snapshot, Watcher, check_root,
};
use flarebox_proto::v1::{ChunkRef, Content};
use rustix::fs::{Mode, OFlags};

use crate::ignore::Matcher;

/// How often the fallback watcher rescans the whole tree.
const RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// One batch of observations.
#[derive(Debug)]
pub struct Observation {
    pub changes: Vec<Observed>,
    /// The root check of design §6.2, made after the scans of this batch. Removals in a batch whose check failed
    /// are not trusted.
    pub root: Result<(), RootProblem>,
    /// Folders the scans may not read. What is below them is unknown, not absent.
    pub unreadable: Vec<PathBuf>,
    /// Files that changed while they were chunked, or that could not be opened as the entry the scan saw. Their
    /// changes carry no content; the next observation reports them again once they are quiet (design §5.8).
    pub busy: Vec<PathBuf>,
}

/// One change, in the terms of `core/fs` (paths relative to the root, previous-state paths first).
#[derive(Clone, Debug, PartialEq)]
pub enum Observed {
    /// `content` is set for a file that could be read whole.
    Created {
        path: PathBuf,
        kind: EntryKind,
        id: FileId,
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
        })
    }

    fn observed(&self, change: Change, busy: &mut Vec<PathBuf>) -> Option<Observed> {
        let entry = |path: &Path| self.watcher.snapshot().entries.get(path).copied();
        let excluded =
            |path: &Path, kind: EntryKind| self.matcher.is_excluded(path, kind == EntryKind::Dir);
        match change {
            Change::Created { path, kind } => {
                let entry = entry(&path)?;
                if excluded(&path, kind) {
                    return None;
                }
                let content = (kind == EntryKind::File)
                    .then(|| self.content(&path, &entry, busy))
                    .flatten();
                Some(Observed::Created {
                    path,
                    kind,
                    id: entry.id,
                    content,
                })
            }
            Change::Modified { path } => {
                let entry = entry(&path)?;
                if entry.kind != EntryKind::File || excluded(&path, entry.kind) {
                    return None;
                }
                let content = self.content(&path, &entry, busy);
                Some(Observed::Modified {
                    content,
                    id: entry.id,
                    path,
                })
            }
            Change::Renamed { from, to, kind } => {
                let entry = entry(&to)?;
                (!excluded(&to, kind)).then_some(Observed::Renamed {
                    from,
                    to,
                    kind,
                    id: entry.id,
                })
            }
            Change::Removed { path, kind } => {
                (!excluded(&path, kind)).then_some(Observed::Removed { path, kind })
            }
        }
    }

    /// Chunks the file at `path` if it is still the entry the scan saw and does not change while it is read.
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
        if poll.changes.is_empty() && poll.unreadable.is_empty() {
            return Ok(None);
        }
        let root = check_root(&self.root, &self.space_id, self.dev).map(|_| ());
        let mut busy = Vec::new();
        let changes = poll
            .changes
            .into_iter()
            .filter_map(|change| self.observed(change, &mut busy))
            .collect();
        Ok(Some(Observation {
            changes,
            root,
            unreadable: poll.unreadable,
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
    let mut dir: OwnedFd = rustix::fs::open(root, dir_flags, Mode::empty())?;
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
}
