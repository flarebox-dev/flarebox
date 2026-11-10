// SPDX-License-Identifier: Apache-2.0

//! The engine's private folder at the root of a space, the root marker in it,
//! the root check of design §6.2, and the rename that never replaces its target
//! which effects need (design §5.2).

use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::{FileId, list, open_dir_at, open_root, stat_at, stat_fd};

/// The engine's private folder at the root of a space, for the marker,
/// staging, quarantine, parked entries and the local trash. Scans and watchers
/// never report anything in it.
pub const PRIVATE_DIR: &str = ".flarebox";

const MARKER: &str = "space.json";
const MARKER_TEMP: &str = "space.json.tmp";
const PROBE: [&str; 3] = [
    "no-replace-probe-a",
    "no-replace-probe-b",
    "no-replace-probe-c",
];

/// The root marker `.flarebox/space.json`: the space the folder holds, and the
/// effect counter *m* of design §2.8.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub space_id: String,
    pub effects: u64,
}

/// Why a root fails the check of design §6.2. No operation may be built from a
/// scan of a root that fails it.
#[derive(Debug, thiserror::Error)]
pub enum RootProblem {
    #[error("the root folder is missing")]
    Missing,
    #[error("the root is not a folder")]
    NotAFolder,
    #[error("the root folder cannot be read: {0}")]
    Unreadable(io::Error),
    #[error("the root folder is on device {found}, not on device {expected}")]
    OtherDevice { expected: u64, found: u64 },
    #[error("the root folder has no marker")]
    NoMarker,
    #[error("the root marker cannot be read: {0}")]
    BadMarker(io::Error),
    #[error("the root folder belongs to space {0}")]
    OtherSpace(String),
}

/// Checks the root of space `space_id` before a scan that can report removals
/// (design §6.2): it exists, is a folder that can be listed, is on device
/// `dev`, and its marker names the space. Returns the marker. The scan itself
/// must also come back without unreadable folders
/// ([`Rescan::unreadable`](crate::Rescan::unreadable)) before its removals are
/// acted on.
pub fn check_root(root: &Path, space_id: &str, dev: u64) -> Result<Marker, RootProblem> {
    let dir = open_root(root).map_err(|err| match err.kind() {
        io::ErrorKind::NotFound => RootProblem::Missing,
        io::ErrorKind::NotADirectory => RootProblem::NotAFolder,
        _ => RootProblem::Unreadable(err),
    })?;
    let stat = stat_fd(dir.as_fd()).map_err(RootProblem::Unreadable)?;
    list(dir.as_fd()).map_err(RootProblem::Unreadable)?;
    if stat.id.dev != dev {
        return Err(RootProblem::OtherDevice {
            expected: dev,
            found: stat.id.dev,
        });
    }
    let marker = read_marker_at(dir.as_fd())
        .map_err(RootProblem::BadMarker)?
        .ok_or(RootProblem::NoMarker)?;
    if marker.space_id != space_id {
        return Err(RootProblem::OtherSpace(marker.space_id));
    }
    Ok(marker)
}

/// Reads the root marker, or returns `None` if there is none.
pub fn read_marker(root: &Path) -> io::Result<Option<Marker>> {
    read_marker_at(open_root(root)?.as_fd())
}

fn read_marker_at(root: BorrowedFd<'_>) -> io::Result<Option<Marker>> {
    let private = match open_dir_at(root, OsStr::new(PRIVATE_DIR)) {
        Ok(private) => private,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(&private, MARKER, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    File::from(file).read_to_string(&mut text)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

/// Writes the root marker and flushes it, with the private folder that holds
/// it, before returning. The marker is replaced in one rename, so a crash
/// leaves the old marker or the new one.
pub fn write_marker(root: &Path, marker: &Marker) -> io::Result<()> {
    write_marker_at(open_root(root)?.as_fd(), marker)
}

pub(crate) fn write_marker_at(root: BorrowedFd<'_>, marker: &Marker) -> io::Result<()> {
    let private = private_dir(root)?;
    replace_file(
        private.as_fd(),
        MARKER,
        MARKER_TEMP,
        &serde_json::to_vec_pretty(marker)?,
    )
}

/// Replaces the private file `name` in `dir` with `bytes` through the
/// temporary file `temp`, flushing the file and then `dir`, so a crash leaves
/// the old contents or the new ones.
pub(crate) fn replace_file(
    dir: BorrowedFd<'_>,
    name: &str,
    temp: &str,
    bytes: &[u8],
) -> io::Result<()> {
    remove_if_present(dir, temp)?;
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(rustix::fs::openat(
        dir,
        temp,
        flags,
        Mode::from_raw_mode(0o644),
    )?);
    file.write_all(bytes)?;
    file.sync_all()?;
    rustix::fs::renameat(dir, temp, dir, name)?;
    flush(dir)
}

/// Whether the file system under `root` has a rename that fails instead of
/// replacing its target. Effects need it, so a space on a file system without
/// it is refused (design §5.2). The probe works on files of its own in the
/// private folder, which it creates if needed, and removes them again. It
/// requires both halves: a no-replace rename onto an existing file fails with
/// `EEXIST` and leaves both files where they were, and one onto a free name
/// moves the file. Linux refuses the first before the file system sees the
/// flag, so only the second shows a file system that does not support it.
pub fn probe_no_replace(root: &Path) -> io::Result<bool> {
    let root = open_root(root)?;
    let private = private_dir(root.as_fd())?;
    let private = private.as_fd();
    let [a, b, _] = PROBE;
    for name in PROBE {
        remove_if_present(private, name)?;
    }
    for name in [a, b] {
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        rustix::fs::openat(private, name, flags, Mode::from_raw_mode(0o600))?;
    }
    let supported = probe_renames(private)?;
    for name in PROBE {
        remove_if_present(private, name)?;
    }
    Ok(supported)
}

fn probe_renames(private: BorrowedFd<'_>) -> io::Result<bool> {
    let [a, b, c] = PROBE.map(OsStr::new);
    let (id_a, id_b) = (probe_id(private, a)?, probe_id(private, b)?);
    match rename_no_replace(private, a, private, b) {
        Err(err) if err.raw_os_error() == Some(Errno::EXIST.raw_os_error()) => {}
        Err(err) if !unsupported(&err) => return Err(err),
        _ => return Ok(false),
    }
    if probe_id(private, a)? != id_a || probe_id(private, b)? != id_b {
        return Ok(false);
    }
    match rename_no_replace(private, a, private, c) {
        Ok(()) => {}
        Err(err) if unsupported(&err) => return Ok(false),
        Err(err) => return Err(err),
    }
    Ok(probe_id(private, a)?.is_none() && probe_id(private, c)? == id_a)
}

/// The identity of the probe file `name`, or `None` when it is gone.
fn probe_id(private: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<FileId>> {
    match stat_at(private, name) {
        Ok(stat) => Ok(Some(stat.id)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Renames `from` in `from_dir` to `to` in `to_dir`, failing with
/// `AlreadyExists` instead of replacing an entry at the target. Fails on a file
/// system without that rename; [`probe_no_replace`] tells in advance.
pub fn rename_no_replace(
    from_dir: BorrowedFd<'_>,
    from: &OsStr,
    to_dir: BorrowedFd<'_>,
    to: &OsStr,
) -> io::Result<()> {
    Ok(rustix::fs::renameat_with(
        from_dir,
        from,
        to_dir,
        to,
        RenameFlags::NOREPLACE,
    )?)
}

/// Whether the error means the file system has no rename that refuses to
/// replace its target.
fn unsupported(err: &io::Error) -> bool {
    [Errno::INVAL, Errno::NOSYS, Errno::NOTSUP, Errno::OPNOTSUPP]
        .iter()
        .any(|errno| err.raw_os_error() == Some(errno.raw_os_error()))
}

/// Opens the private folder at the root, creating it, and flushing the root
/// after creating it, when it is missing.
pub(crate) fn private_dir(root: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    make_dir(root, PRIVATE_DIR)
}

/// Opens the folder `name` in `dir` without following a link, creating it, and
/// flushing `dir` after creating it, when it is missing.
pub(crate) fn make_dir(dir: BorrowedFd<'_>, name: &str) -> io::Result<OwnedFd> {
    match rustix::fs::mkdirat(dir, name, Mode::from_raw_mode(0o755)) {
        Ok(()) => flush(dir)?,
        Err(Errno::EXIST) => {}
        Err(err) => return Err(err.into()),
    }
    open_dir_at(dir, OsStr::new(name))
}

pub(crate) fn remove_if_present(dir: BorrowedFd<'_>, name: &str) -> io::Result<()> {
    match rustix::fs::unlinkat(dir, name, AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Flushes a file or a folder's entries. On macOS `fsync` does not ask the
/// drive to write its cache, so this uses `F_FULLFSYNC` there (design §5.2).
pub(crate) fn flush(fd: BorrowedFd<'_>) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    rustix::fs::fcntl_fullfsync(fd)?;
    #[cfg(not(target_vendor = "apple"))]
    rustix::fs::fsync(fd)?;
    Ok(())
}
