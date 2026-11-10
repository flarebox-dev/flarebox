// SPDX-License-Identifier: Apache-2.0

//! Resolving the places effects act on from a descriptor of the sync root,
//! one folder at a time, without following links, checking each folder's
//! identity (design §5.2, path resolution).

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::io::Errno;

use crate::root::PRIVATE_DIR;
use crate::{DIR_FLAGS, FileId, open_root, stat_fd};

/// The folder inside the private folder that holds parked entries (design
/// §2.5).
pub const PARKED_DIR: &str = "parked";

/// A folder on the way to an entry, with the identity the caller last
/// observed for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub name: OsString,
    pub id: FileId,
}

/// Where the folders of a [`Location`] start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Base {
    /// The sync root.
    Root,
    /// The parked folder `.flarebox/parked/<name>`, which must have `id`.
    Parked { name: OsString, id: FileId },
}

/// An entry's place on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// `.flarebox/parked/<name>` itself.
    Parked(OsString),
    /// `name` inside the last of `dirs`, which start at `base`.
    At {
        base: Base,
        dirs: Vec<Dir>,
        name: OsString,
    },
}

/// Why a location does not lead to the folders the caller expects. Only
/// [`Unresolved::Missing`] says that something is not there; an
/// [`Unresolved::Io`] error says nothing about what the disk holds.
#[derive(Debug, thiserror::Error)]
pub enum Unresolved {
    #[error("{0:?} is not a single file name")]
    BadName(OsString),
    #[error("{} is missing", .0.display())]
    Missing(PathBuf),
    /// A file or a symbolic link has the folder's name.
    #[error("{} is not a folder", .0.display())]
    NotAFolder(PathBuf),
    #[error("{} is not the folder expected there", .path.display())]
    Replaced {
        path: PathBuf,
        expected: FileId,
        found: FileId,
    },
    #[error("{}: {err}", .path.display())]
    Io { path: PathBuf, err: io::Error },
}

/// The sync root, held open. Effects resolve every path from this descriptor
/// rather than from a path spelling.
#[derive(Debug)]
pub struct Space {
    root: OwnedFd,
}

/// A folder resolved from the root and held open, with the route that
/// reached it, so that [`Space::recheck`] can follow the route again.
#[derive(Debug)]
pub struct Folder {
    fd: OwnedFd,
    id: FileId,
    route: Vec<Dir>,
}

impl Folder {
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// The folder's identity when it was resolved.
    pub fn id(&self) -> FileId {
        self.id
    }

    /// The folder's path from the root.
    pub fn path(&self) -> PathBuf {
        self.route.iter().map(|dir| dir.name.as_os_str()).collect()
    }
}

impl Space {
    pub fn open(root: &Path) -> io::Result<Self> {
        Ok(Self {
            root: open_root(root)?,
        })
    }

    /// Resolves the folder that holds `location` and returns it with the
    /// entry's name. The entry itself is not looked at.
    pub fn parent<'a>(&self, location: &'a Location) -> Result<(Folder, &'a OsStr), Unresolved> {
        match location {
            Location::Parked(name) => {
                check_name(name)?;
                Ok((self.private(&[PARKED_DIR])?, name))
            }
            Location::At { base, dirs, name } => {
                check_name(name)?;
                Ok((self.folder(base, dirs)?, name))
            }
        }
    }

    /// Resolves `dirs` from `base`, checking every folder's identity.
    pub fn folder(&self, base: &Base, dirs: &[Dir]) -> Result<Folder, Unresolved> {
        let mut folder = match base {
            Base::Root => self.root_folder()?,
            Base::Parked { name, id } => descend(
                self.private(&[PARKED_DIR])?,
                &Dir {
                    name: name.clone(),
                    id: *id,
                },
            )?,
        };
        for dir in dirs {
            folder = descend(folder, dir)?;
        }
        Ok(folder)
    }

    /// Opens `.flarebox/<parts>`, recording the identity of each folder on
    /// the way, so that [`Space::recheck`] also notices one of them moved.
    /// Each part must be a single file name.
    pub fn private(&self, parts: &[&str]) -> Result<Folder, Unresolved> {
        let mut folder = self.root_folder()?;
        for name in std::iter::once(PRIVATE_DIR).chain(parts.iter().copied()) {
            let name = OsStr::new(name);
            check_name(name)?;
            let fd = open_dir(folder.fd(), name, &folder.route)?;
            folder.id = identity(&fd, &folder.route, name)?;
            folder.fd = fd;
            folder.route.push(Dir {
                name: name.to_owned(),
                id: folder.id,
            });
        }
        Ok(folder)
    }

    /// Follows `folder`'s route from the root again and checks that every
    /// folder on it keeps its identity. A descriptor follows its folder when
    /// the folder is moved, even out of the root, so call this immediately
    /// before each rename through `folder`.
    pub fn recheck(&self, folder: &Folder) -> Result<(), Unresolved> {
        let mut again = self.root_folder()?;
        for dir in &folder.route {
            again = descend(again, dir)?;
        }
        Ok(())
    }

    fn root_folder(&self) -> Result<Folder, Unresolved> {
        let io = |err| Unresolved::Io {
            path: PathBuf::new(),
            err,
        };
        Ok(Folder {
            fd: self.root.try_clone().map_err(io)?,
            id: stat_fd(self.root.as_fd()).map_err(io)?.id,
            route: Vec::new(),
        })
    }
}

/// Opens `dir` inside `folder` and checks its identity.
fn descend(folder: Folder, dir: &Dir) -> Result<Folder, Unresolved> {
    check_name(&dir.name)?;
    let fd = open_dir(folder.fd(), &dir.name, &folder.route)?;
    let found = identity(&fd, &folder.route, &dir.name)?;
    let mut route = folder.route;
    route.push(dir.clone());
    if found != dir.id {
        return Err(Unresolved::Replaced {
            path: route.iter().map(|dir| dir.name.as_os_str()).collect(),
            expected: dir.id,
            found,
        });
    }
    Ok(Folder {
        fd,
        id: found,
        route,
    })
}

fn open_dir(parent: BorrowedFd<'_>, name: &OsStr, route: &[Dir]) -> Result<OwnedFd, Unresolved> {
    open_beneath(parent, name).map_err(|err| match err {
        Errno::NOENT => Unresolved::Missing(join(route, name)),
        Errno::NOTDIR | Errno::LOOP => Unresolved::NotAFolder(join(route, name)),
        err => Unresolved::Io {
            path: join(route, name),
            err: err.into(),
        },
    })
}

/// Opens the folder `name` inside `parent` without following a symbolic
/// link.
#[cfg(target_os = "linux")]
fn open_beneath(parent: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Errno> {
    use rustix::fs::{Mode, ResolveFlags};
    let resolve = ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS;
    match rustix::fs::openat2(parent, name, DIR_FLAGS, Mode::empty(), resolve) {
        Err(Errno::NOSYS) => rustix::fs::openat(parent, name, DIR_FLAGS, Mode::empty()),
        opened => opened,
    }
}

#[cfg(not(target_os = "linux"))]
fn open_beneath(parent: BorrowedFd<'_>, name: &OsStr) -> Result<OwnedFd, Errno> {
    rustix::fs::openat(parent, name, DIR_FLAGS, rustix::fs::Mode::empty())
}

fn identity(fd: &OwnedFd, route: &[Dir], name: &OsStr) -> Result<FileId, Unresolved> {
    stat_fd(fd.as_fd())
        .map(|stat| stat.id)
        .map_err(|err| Unresolved::Io {
            path: join(route, name),
            err,
        })
}

fn join(route: &[Dir], name: &OsStr) -> PathBuf {
    route
        .iter()
        .map(|dir| dir.name.as_os_str())
        .chain(std::iter::once(name))
        .collect()
}

/// Refuses anything but one ordinary file name, so a name can neither climb
/// out of its folder nor reach past it.
fn check_name(name: &OsStr) -> Result<(), Unresolved> {
    let bytes = name.as_bytes();
    if matches!(bytes, b"" | b"." | b"..") || bytes.contains(&b'/') || bytes.contains(&0) {
        return Err(Unresolved::BadName(name.to_owned()));
    }
    Ok(())
}
