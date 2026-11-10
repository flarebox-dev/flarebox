// SPDX-License-Identifier: Apache-2.0

//! Carrying out one disk effect without losing user bytes (design §5.2), and
//! recovering the effect in flight after a crash (design §5.3).
//!
//! Every name change in the sync root is one rename that never replaces an
//! entry, or one `mkdir`; nothing in the root is unlinked or overwritten.
//! An entry an effect displaces goes to `.flarebox/quarantine/<effect id>`
//! first, is compared with what the caller expected, and then goes to the
//! local trash, or back to its name, or, when the name was taken in the
//! meantime, to a recovered name beside it. After every rename the source
//! folder and then the destination folder are flushed.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;
use std::time::SystemTime;

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

use crate::resolve::{Folder, Location, Space, Unresolved, check_name};
use crate::root::{Marker, flush, remove_if_present, rename_no_replace, write_marker_at};
use crate::{Entry, EntryKind, FileId, Stat, list, stat_at, stat_fd};

/// The folder inside the private folder that holds staged downloads.
pub const STAGING_DIR: &str = "staging";
/// The folder inside the private folder that holds displaced entries until
/// they are compared. It is never swept.
pub const QUARANTINE_DIR: &str = "quarantine";

/// An entry an effect displaces, as the caller last observed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Displaced {
    pub at: Location,
    /// Identity, kind, size, modification time and executable bit. The
    /// content is compared through [`Contents::displaced`].
    pub entry: Entry,
}

/// One disk change, in the terms of core/fs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    CreateDir {
        at: Location,
    },
    /// Puts the staged file `.flarebox/staging/<effect id>` at `at`. With
    /// `replaces`, that entry goes to quarantine first; it sits elsewhere when
    /// the node also moves.
    WriteFile {
        at: Location,
        replaces: Option<Displaced>,
    },
    /// Renames the entry with identity `id`.
    Move {
        from: Location,
        id: FileId,
        to: Location,
    },
    RemoveFile {
        displaced: Displaced,
    },
    /// Removes a folder, which must be empty once quarantined.
    RemoveDir {
        at: Location,
        id: FileId,
    },
}

/// A disk effect as its record holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effect {
    /// Names the effect's staging and quarantine entries, and its trash entry,
    /// so it must be a single file name.
    pub id: String,
    pub action: Action,
}

/// Compares file contents with the ones an effect expects. core/fs does not
/// chunk files, so the caller compares manifest digests.
pub trait Contents {
    /// Whether `file` holds the content the effect displaces.
    fn displaced(&mut self, file: &File) -> io::Result<bool>;
    /// Whether `file` holds the content the effect writes.
    fn written(&mut self, file: &File) -> io::Result<bool>;
}

/// The points between the steps of design §5.2 where a hook runs. Each is
/// reached once the step it names is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The root marker holds the effect's counter.
    Marked,
    /// The folders resolved and the entry the effect acts on matched.
    Checked,
    /// The displaced entry is in quarantine.
    Quarantined,
    /// The quarantined entry was compared with the expected one.
    Compared,
    /// The new entry has its name.
    Placed,
    /// The quarantined entry went back to its name or to a recovered name.
    Returned,
    /// The quarantined entry is in the local trash.
    Trashed,
}

/// How an effect ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `identity` is the identity of the entry the effect created or wrote
    /// at its target, read through the descriptor that made or verified it:
    /// the staged file before its rename, the new folder through its parent,
    /// or the file whose content recovery compared. It is `None` for a move
    /// or a removal.
    Done { identity: Option<FileId> },
    /// The disk did not match what the effect expected. The caller observes
    /// the disk again; `recovered` is the path of a file made from the
    /// displaced entry, which is a new local file.
    Failed {
        reason: Reason,
        recovered: Option<PathBuf>,
    },
}

/// Why an effect failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// A folder on the way is missing, is not a folder, or is another one.
    Moved,
    /// The entry the effect acts on is not there.
    Missing,
    /// The entry the effect acts on differs from the one expected.
    Changed,
    /// The name the effect puts an entry at is taken.
    Taken,
    /// Recovery found the effect stopped before it displaced anything.
    Interrupted,
}

fn failed(reason: Reason) -> Outcome {
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

/// The outcome for a location that does not resolve. Only an answer from
/// the disk fails the effect; an error is returned.
fn unresolved(err: Unresolved) -> io::Result<Outcome> {
    match err {
        Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. } => {
            Ok(failed(Reason::Moved))
        }
        err => Err(err.into()),
    }
}

macro_rules! resolve {
    ($resolved:expr) => {
        match $resolved {
            Ok(resolved) => resolved,
            Err(err) => return unresolved(err),
        }
    };
}

impl Space {
    /// Creates the staged file for effect `id`, replacing a leftover one.
    pub fn stage(&self, id: &str) -> io::Result<File> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        remove_if_present(staging.fd(), id)?;
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        Ok(rustix::fs::openat(staging.fd(), id, flags, Mode::from_raw_mode(0o644))?.into())
    }

    /// Sets the staged file's executable bit and flushes it, which design
    /// §5.2 requires before the file is renamed into place. The caller has
    /// written and verified its content.
    pub fn finish_staged(&self, id: &str, executable: bool) -> io::Result<()> {
        check_name(OsStr::new(id))?;
        let staging = self.private(&[STAGING_DIR])?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let file = rustix::fs::openat(staging.fd(), id, flags, Mode::empty())?;
        let mode = rustix::fs::fstat(&file)?.st_mode & 0o7777;
        let mode = if executable {
            mode | (mode & 0o444) >> 2
        } else {
            mode & !0o111
        };
        rustix::fs::fchmod(&file, Mode::from_raw_mode(mode as _))?;
        flush(rustix::fd::AsFd::as_fd(&file))
    }

    /// Deletes every staged file except `keep`, the effect in flight. Staged
    /// files only hold bytes downloaded from the server (design §5.3).
    pub fn clear_staging(&self, keep: Option<&str>) -> io::Result<Vec<OsString>> {
        let staging = self.private(&[STAGING_DIR])?;
        let mut cleared = Vec::new();
        for name in list(staging.fd())? {
            if keep.is_some_and(|keep| name == OsStr::new(keep)) {
                continue;
            }
            self.recheck(&staging)?;
            rustix::fs::unlinkat(staging.fd(), &name, AtFlags::empty())?;
            cleared.push(name);
        }
        Ok(cleared)
    }

    /// Carries out `effect` after writing `marker`, which holds the effect's
    /// counter (design §2.8). `hook` runs at every [`Step`] reached.
    ///
    /// An error leaves the effect anywhere between its steps: run
    /// [`Space::recover`] for it before anything else. A replacement whose
    /// staged file is missing fails with [`io::ErrorKind::NotFound`] before it
    /// changes anything. A root that is no longer held fails the effect with
    /// [`Unresolved::RootWithdrawn`] before the next change.
    pub fn apply(
        &self,
        marker: &Marker,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        check_name(OsStr::new(&effect.id))?;
        self.check_held()?;
        write_marker_at(self.root(), marker)?;
        hook(Step::Marked);
        let outcome = match &effect.action {
            Action::CreateDir { at } => {
                let (parent, name) = resolve!(self.parent(at));
                hook(Step::Checked);
                resolve!(self.recheck(&parent));
                match rustix::fs::mkdirat(parent.fd(), name, Mode::from_raw_mode(0o755)) {
                    Ok(()) => flush(parent.fd())?,
                    Err(Errno::EXIST) => return Ok(failed(Reason::Taken)),
                    Err(err) => return Err(err.into()),
                }
                let identity = stat_at(parent.fd(), name)?.id;
                hook(Step::Placed);
                Outcome::Done {
                    identity: Some(identity),
                }
            }
            Action::WriteFile { at, replaces: None } => {
                let staging = self.private(&[STAGING_DIR])?;
                let (parent, name) = resolve!(self.parent(at));
                hook(Step::Checked);
                self.place(&staging, &effect.id, &parent, name, hook)?
            }
            Action::WriteFile {
                at,
                replaces: Some(displaced),
            } => self.displace(effect, displaced, Some(at), contents, now, hook)?,
            Action::RemoveFile { displaced } => {
                self.displace(effect, displaced, None, contents, now, hook)?
            }
            Action::RemoveDir { at, id } => {
                let displaced = Displaced {
                    at: at.clone(),
                    entry: folder_entry(*id),
                };
                self.displace(effect, &displaced, None, contents, now, hook)?
            }
            Action::Move { from, id, to } => {
                let (source, name) = resolve!(self.parent(from));
                let (target, new_name) = resolve!(self.parent(to));
                match lookup(source.fd(), name)? {
                    None => return Ok(failed(Reason::Missing)),
                    Some(found) if found.id != *id => return Ok(failed(Reason::Changed)),
                    Some(_) => {}
                }
                hook(Step::Checked);
                resolve!(self.recheck(&source));
                resolve!(self.recheck(&target));
                match rename_no_replace(source.fd(), name, target.fd(), new_name) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                        return Ok(failed(Reason::Taken));
                    }
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {
                        return Ok(failed(Reason::Missing));
                    }
                    Err(err) => return Err(err),
                }
                flush(source.fd())?;
                flush(target.fd())?;
                hook(Step::Placed);
                Outcome::Done { identity: None }
            }
        };
        if !matches!(outcome, Outcome::Done { .. }) {
            self.drop_staged(&effect.id)?;
        }
        Ok(outcome)
    }

    /// Steps 2 to 5 of design §5.2 for an effect that displaces an entry,
    /// with step 4 placing the staged file at `place` when there is one.
    fn displace(
        &self,
        effect: &Effect,
        displaced: &Displaced,
        place: Option<&Location>,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        let (parent, name) = resolve!(self.parent(&displaced.at));
        let target = match place {
            Some(at) => {
                let target = resolve!(self.parent(at));
                let staging = self.private(&[STAGING_DIR])?;
                if lookup(staging.fd(), OsStr::new(&effect.id))?.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("effect {} has no staged file", effect.id),
                    ));
                }
                Some((staging, target))
            }
            None => None,
        };
        let quarantine = self.private(&[QUARANTINE_DIR])?;
        match lookup(parent.fd(), name)? {
            None => return Ok(failed(Reason::Missing)),
            Some(found) if found.entry() != Some(displaced.entry) => {
                return Ok(failed(Reason::Changed));
            }
            Some(_) => {}
        }
        hook(Step::Checked);

        resolve!(self.recheck(&parent));
        self.recheck(&quarantine)?;
        let id = OsStr::new(&effect.id);
        match rename_no_replace(parent.fd(), name, quarantine.fd(), id) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Ok(failed(Reason::Missing));
            }
            Err(err) => return Err(err),
        }
        flush(parent.fd())?;
        flush(quarantine.fd())?;
        hook(Step::Quarantined);

        let matches = matches(quarantine.fd(), id, &displaced.entry, contents)?;
        hook(Step::Compared);
        if !matches {
            let recovered = self.put_back(&quarantine, id, &displaced.at, now)?;
            hook(Step::Returned);
            return Ok(Outcome::Failed {
                reason: Reason::Changed,
                recovered,
            });
        }
        let outcome = match target {
            Some((staging, (target, new_name))) => {
                self.place(&staging, &effect.id, &target, new_name, hook)?
            }
            None => Outcome::Done { identity: None },
        };
        self.trash(&quarantine, id, &effect.id, name, now)?;
        hook(Step::Trashed);
        Ok(outcome)
    }

    /// Renames the staged file to `name` in `target`, never replacing an entry,
    /// and returns the staged file's identity with the outcome.
    fn place(
        &self,
        staging: &Folder,
        id: &str,
        target: &Folder,
        name: &OsStr,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        resolve!(self.recheck(target));
        self.recheck(staging)?;
        let identity = stat_at(staging.fd(), OsStr::new(id))?.id;
        match rename_no_replace(staging.fd(), OsStr::new(id), target.fd(), name) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(failed(Reason::Taken));
            }
            Err(err) => return Err(err),
        }
        flush(staging.fd())?;
        flush(target.fd())?;
        hook(Step::Placed);
        Ok(Outcome::Done {
            identity: Some(identity),
        })
    }

    /// Returns the quarantined entry `id` to `at`. When that name is taken, or
    /// its folder is gone, the entry gets a recovered name in that folder or,
    /// failing that, at the root. Returns the recovered path. A root that is no
    /// longer held is an error, never a reason to recover the entry.
    fn put_back(
        &self,
        quarantine: &Folder,
        id: &OsStr,
        at: &Location,
        now: SystemTime,
    ) -> io::Result<Option<PathBuf>> {
        let folder = stat_at(quarantine.fd(), id)?.file_type == FileType::Directory;
        self.recheck(quarantine)?;
        let (parent, name) = match self.parent(at).and_then(|(parent, name)| {
            self.recheck(&parent)?;
            Ok((parent, name))
        }) {
            Ok((parent, name)) => (parent, name),
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => {
                let original = location_name(at);
                let root = self.root_folder()?;
                let path =
                    self.move_to_recovered_name(quarantine, id, &root, original, folder, now)?;
                return Ok(Some(path));
            }
            Err(err) => return Err(err.into()),
        };
        match rename_no_replace(quarantine.fd(), id, parent.fd(), name) {
            Ok(()) => {
                flush(quarantine.fd())?;
                flush(parent.fd())?;
                Ok(None)
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                let path =
                    self.move_to_recovered_name(quarantine, id, &parent, name, folder, now)?;
                Ok(Some(path))
            }
            Err(err) => Err(err),
        }
    }

    /// Recovers `effect` after a crash (design §5.3), from what the disk
    /// holds at its target, its staging path and its quarantine path. It
    /// writes `marker` again, since the crash may have come before the
    /// marker write. Recovery is idempotent: a crash during it is recovered by
    /// running it again. `hook` runs at every [`Step`] reached. A root that is
    /// no longer held fails recovery with [`Unresolved::RootWithdrawn`] before
    /// the next change, and recovery runs again once the root is bound anew.
    pub fn recover(
        &self,
        marker: &Marker,
        effect: &Effect,
        contents: &mut dyn Contents,
        now: SystemTime,
        hook: &mut dyn FnMut(Step),
    ) -> io::Result<Outcome> {
        check_name(OsStr::new(&effect.id))?;
        self.check_held()?;
        write_marker_at(self.root(), marker)?;
        hook(Step::Marked);
        let quarantine = self.private(&[QUARANTINE_DIR])?;
        let staging = self.private(&[STAGING_DIR])?;
        let id = OsStr::new(&effect.id);
        let staged = lookup(staging.fd(), id)?.is_some();
        let displacing = match &effect.action {
            Action::WriteFile {
                at,
                replaces: Some(displaced),
            } => Some((displaced.clone(), Some(at))),
            Action::RemoveFile { displaced } => Some((displaced.clone(), None)),
            Action::RemoveDir { at, id } => Some((
                Displaced {
                    at: at.clone(),
                    entry: folder_entry(*id),
                },
                None,
            )),
            _ => None,
        };
        if let Some((displaced, place)) = displacing
            && lookup(quarantine.fd(), id)?.is_some()
        {
            let matches = matches(quarantine.fd(), id, &displaced.entry, contents)?;
            hook(Step::Compared);
            if !matches {
                let recovered = self.put_back(&quarantine, id, &displaced.at, now)?;
                hook(Step::Returned);
                self.drop_staged(&effect.id)?;
                return Ok(Outcome::Failed {
                    reason: Reason::Changed,
                    recovered,
                });
            }
            let outcome = match place {
                Some(at) if staged => match self.parent(at) {
                    Ok((target, name)) => self.place(&staging, &effect.id, &target, name, hook)?,
                    Err(err) => unresolved(err)?,
                },
                Some(at) => match self.written_at(at, contents)? {
                    Some(identity) => Outcome::Done {
                        identity: Some(identity),
                    },
                    None => failed(Reason::Taken),
                },
                None => Outcome::Done { identity: None },
            };
            self.drop_staged(&effect.id)?;
            let original = location_name(&displaced.at);
            self.trash(&quarantine, id, &effect.id, original, now)?;
            hook(Step::Trashed);
            return Ok(outcome);
        }
        if !staged && let Some(done) = self.finished(&effect.action, contents)? {
            return Ok(done);
        }
        self.drop_staged(&effect.id)?;
        Ok(failed(Reason::Interrupted))
    }

    /// The outcome of an effect that left nothing in quarantine, when the disk
    /// holds `action`'s final state.
    fn finished(
        &self,
        action: &Action,
        contents: &mut dyn Contents,
    ) -> io::Result<Option<Outcome>> {
        let entry_at = |at: &Location| -> io::Result<Option<Option<Stat>>> {
            match self.parent(at) {
                Ok((parent, name)) => Ok(Some(lookup(parent.fd(), name)?)),
                Err(
                    Unresolved::Missing(_)
                    | Unresolved::NotAFolder(_)
                    | Unresolved::Replaced { .. },
                ) => Ok(None),
                Err(err) => Err(err.into()),
            }
        };
        let identity = match action {
            Action::CreateDir { at } => match entry_at(at)? {
                Some(Some(stat)) if stat.file_type == FileType::Directory => Some(stat.id),
                _ => return Ok(None),
            },
            Action::WriteFile { at, .. } => match self.written_at(at, contents)? {
                Some(identity) => Some(identity),
                None => return Ok(None),
            },
            Action::Move { to, id, .. } => match entry_at(to)? {
                Some(Some(stat)) if stat.id == *id => None,
                _ => return Ok(None),
            },
            Action::RemoveFile {
                displaced: Displaced { at, .. },
            }
            | Action::RemoveDir { at, .. } => match entry_at(at)? {
                Some(None) => None,
                _ => return Ok(None),
            },
        };
        Ok(Some(Outcome::Done { identity }))
    }

    /// The identity of the file at `at` when it holds the content the effect
    /// writes, read from the descriptor the content was compared through.
    fn written_at(&self, at: &Location, contents: &mut dyn Contents) -> io::Result<Option<FileId>> {
        let (parent, name) = match self.parent(at) {
            Ok(resolved) => resolved,
            Err(
                Unresolved::Missing(_) | Unresolved::NotAFolder(_) | Unresolved::Replaced { .. },
            ) => {
                return Ok(None);
            }
            Err(err) => return Err(err.into()),
        };
        match lookup(parent.fd(), name)? {
            Some(stat) if stat.file_type == FileType::RegularFile => {
                let file = open_file(parent.fd(), name)?;
                if !contents.written(&file)? {
                    return Ok(None);
                }
                Ok(Some(stat_fd(file.as_fd())?.id))
            }
            _ => Ok(None),
        }
    }

    fn drop_staged(&self, id: &str) -> io::Result<()> {
        let staging = self.private(&[STAGING_DIR])?;
        remove_if_present(staging.fd(), id)
    }
}

/// Whether the quarantined entry `id` is `expected`: the same identity, kind,
/// size, modification time and executable bit, and, for a file, the content
/// the effect displaces, or, for a folder, nothing inside.
fn matches(
    quarantine: BorrowedFd<'_>,
    id: &OsStr,
    expected: &Entry,
    contents: &mut dyn Contents,
) -> io::Result<bool> {
    if lookup(quarantine, id)?.and_then(|stat| stat.entry()) != Some(*expected) {
        return Ok(false);
    }
    match expected.kind {
        EntryKind::File => contents.displaced(&open_file(quarantine, id)?),
        EntryKind::Dir => {
            let dir = rustix::fs::openat(quarantine, id, crate::DIR_FLAGS, Mode::empty())?;
            Ok(list(rustix::fd::AsFd::as_fd(&dir))?.is_empty())
        }
    }
}

/// What has `name` in `dir`, or `None` when nothing has. Only "not found"
/// means absent; any other error is returned.
fn lookup(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Stat>> {
    match stat_at(dir, name) {
        Ok(stat) => Ok(Some(stat)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

fn open_file(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<File> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    Ok(rustix::fs::openat(dir, name, flags, Mode::empty())?.into())
}

fn folder_entry(id: FileId) -> Entry {
    Entry {
        kind: EntryKind::Dir,
        id,
        size: 0,
        mtime_ns: 0,
        executable: false,
    }
}

fn location_name(at: &Location) -> &OsStr {
    match at {
        Location::Parked(name) | Location::At { name, .. } => name,
    }
}
