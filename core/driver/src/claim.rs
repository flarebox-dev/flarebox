// SPDX-License-Identifier: Apache-2.0

//! Which profile may sync a linked folder. Both facts live in the folder's own `.flarebox/` directory, so they follow
//! the physical folder whatever path, symbolic link or mount reaches it:
//!
//! - `.flarebox/device.json` names the device that linked the folder. Another device's profile refuses to link or
//!   sync it until it is unlinked there.
//! - `.flarebox/lock` is held, as an `flock`, by the process that syncs the folder for as long as its task and
//!   observer run, so two processes never sync one folder at once.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use flarebox_fs::PRIVATE_DIR;
use rustix::fs::{FlockOperation, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

const OWNER: &str = "device.json";
const OWNER_TEMP: &str = "device.json.tmp";
const LOCK: &str = "lock";

/// The device that linked a folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Owner {
    pub server: String,
    pub device_id: String,
}

/// The folder's private directory, opened without following a symbolic link.
fn private_dir(root: &Path) -> io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let root = rustix::fs::open(root, flags, Mode::empty())?;
    Ok(rustix::fs::openat(
        &root,
        PRIVATE_DIR,
        flags | OFlags::NOFOLLOW,
        Mode::empty(),
    )?)
}

/// Takes the folder's lock without waiting. Returns `None` while another process, or another open of the lock in
/// this one, holds it. The lock is released when the returned file is closed.
pub(crate) fn lock(root: &Path) -> io::Result<Option<File>> {
    let dir = private_dir(root)?;
    let flags = OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = File::from(rustix::fs::openat(
        &dir,
        LOCK,
        flags,
        Mode::from_raw_mode(0o600),
    )?);
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(Errno::WOULDBLOCK) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The device that linked the folder, or `None` if none is recorded.
pub(crate) fn owner(root: &Path) -> io::Result<Option<Owner>> {
    let dir = private_dir(root)?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(&dir, OWNER, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut text = String::new();
    File::from(file).read_to_string(&mut text)?;
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Records `owner` as the device that linked the folder, replacing the record in one rename and flushing it.
pub(crate) fn set_owner(root: &Path, owner: &Owner) -> io::Result<()> {
    let dir = private_dir(root)?;
    match rustix::fs::unlinkat(&dir, OWNER_TEMP, rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut file = File::from(rustix::fs::openat(
        &dir,
        OWNER_TEMP,
        flags,
        Mode::from_raw_mode(0o644),
    )?);
    file.write_all(&serde_json::to_vec_pretty(owner)?)?;
    file.sync_all()?;
    rustix::fs::renameat(&dir, OWNER_TEMP, &dir, OWNER)?;
    rustix::fs::fsync(dir.as_fd())?;
    Ok(())
}

/// Removes the record if it names `owner`, so another device may link the folder.
pub(crate) fn release(root: &Path, owner: &Owner) -> io::Result<()> {
    if self::owner(root)?.as_ref() != Some(owner) {
        return Ok(());
    }
    let dir = private_dir(root)?;
    match rustix::fs::unlinkat(&dir, OWNER, rustix::fs::AtFlags::empty()) {
        Ok(()) | Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn folder() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(PRIVATE_DIR)).unwrap();
        dir
    }

    fn device(id: &str) -> Owner {
        Owner {
            server: "https://files.example.com/".into(),
            device_id: id.into(),
        }
    }

    #[test]
    fn one_holder_at_a_time_takes_the_lock_whatever_path_reaches_the_folder() {
        let dir = folder();
        let other_path = tempfile::tempdir().unwrap();
        let alias = other_path.path().join("alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();

        let held = lock(dir.path()).unwrap().unwrap();
        assert!(lock(&alias).unwrap().is_none());
        assert!(lock(dir.path()).unwrap().is_none());
        drop(held);
        assert!(lock(&alias).unwrap().is_some());
    }

    #[test]
    fn the_owner_record_is_replaced_and_released_only_by_its_device() {
        let dir = folder();
        assert_eq!(owner(dir.path()).unwrap(), None);
        set_owner(dir.path(), &device("d1")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), Some(device("d1")));
        release(dir.path(), &device("d2")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), Some(device("d1")));
        release(dir.path(), &device("d1")).unwrap();
        assert_eq!(owner(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_symbolic_link_in_place_of_the_private_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(PRIVATE_DIR)).unwrap();
        assert!(lock(dir.path()).is_err());
        assert!(set_owner(dir.path(), &device("d1")).is_err());
        assert!(fs::read_dir(elsewhere.path()).unwrap().next().is_none());
    }
}
