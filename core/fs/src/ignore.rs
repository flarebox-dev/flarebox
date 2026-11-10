// SPDX-License-Identifier: Apache-2.0

//! The rules that leave paths out of scans (design §5.10): the matcher scans
//! ask, the rules files they read for it, and the search that tells a move
//! into a left-out path from a removal.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{FileType, Mode, OFlags};

use crate::{EntryKind, FileId, Stat, is_gone, list, open_attached, open_dir_at, stat_at, stat_fd};

/// The name of a rules file.
pub const RULES_FILE: &str = ".fbignore";

/// The largest rules file a scan reads.
const MAX_RULES: u64 = 1 << 20;

/// Why a path is left out of scans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    No,
    /// A platform file such as `.DS_Store`. No rule can bring it back.
    BuiltIn,
    /// A `.fbignore` rule or the device's own list.
    Rule,
}

/// The matcher scans ask. The driver implements it over
/// `flarebox_ignore::Matcher`.
pub trait Ignore {
    /// The rules file of folder `dir` (relative to the root, empty for the
    /// root) holds `rules`, or the folder has none.
    fn set_rules(&mut self, dir: &Path, rules: Option<&[u8]>);

    /// Whether the entry at `path`, relative to the root, is left out. A path
    /// below a left-out folder is left out.
    fn status(&self, path: &Path, kind: EntryKind) -> Ignored;
}

/// The rules of one root and what its scans have seen of them.
///
/// A new `Rules` knows no rules file. A scan reads the rules file of each
/// folder it lists, so scan the whole tree before any part of it.
pub struct Rules {
    ignore: Box<dyn Ignore + Send>,
    /// The bytes of each folder's rules file, as last read.
    files: BTreeMap<PathBuf, Vec<u8>>,
    /// The names each listed folder holds that a rule leaves out
    /// ([`Ignored::Rule`] only).
    ignored: BTreeMap<PathBuf, BTreeSet<OsString>>,
}

impl Rules {
    pub fn new(ignore: Box<dyn Ignore + Send>) -> Self {
        Self {
            ignore,
            files: BTreeMap::new(),
            ignored: BTreeMap::new(),
        }
    }

    /// Whether the entry at `path`, relative to the root, is left out. The
    /// root itself never is.
    pub fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        if path.as_os_str().is_empty() {
            return Ignored::No;
        }
        self.ignore.status(path, kind)
    }

    /// Gives the folder `dir` the rules file `rules`, or none, and returns
    /// what it had.
    fn set(&mut self, dir: &Path, rules: Option<Vec<u8>>) -> Option<Vec<u8>> {
        self.ignore.set_rules(dir, rules.as_deref());
        match rules {
            Some(bytes) => self.files.insert(dir.to_path_buf(), bytes),
            None => self.files.remove(dir),
        }
    }
}

/// The entries a search looks for, by identity and kind, and where it found
/// each one.
pub(crate) type Wanted = HashMap<(FileId, EntryKind), Option<PathBuf>>;

/// The rules during one scan. Until the scan is published, [`Pending::undo`]
/// takes back everything it changed in them.
pub(crate) struct Pending<'r> {
    rules: &'r mut Rules,
    /// What each folder's rules file held before this scan changed it.
    before: BTreeMap<PathBuf, Option<Vec<u8>>>,
    /// The names a rule leaves out in each folder this scan listed.
    listed: BTreeMap<PathBuf, BTreeSet<OsString>>,
}

impl<'r> Pending<'r> {
    pub(crate) fn new(rules: &'r mut Rules) -> Self {
        Self {
            rules,
            before: BTreeMap::new(),
            listed: BTreeMap::new(),
        }
    }

    pub(crate) fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        self.rules.status(path, kind)
    }

    /// Reads the rules file of the folder `dir` at `rel`, which lists
    /// `names`, and returns whether the folder's rules changed. A rules file
    /// that cannot be read keeps the rules the folder last had.
    pub(crate) fn read(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        names: &BTreeSet<OsString>,
    ) -> bool {
        self.listed.entry(rel.to_path_buf()).or_default();
        let found = if names.contains(OsStr::new(RULES_FILE)) {
            match read_rules(dir) {
                Ok(found) => found,
                Err(_) => return false,
            }
        } else {
            None
        };
        if found.as_ref() == self.rules.files.get(rel) {
            return false;
        }
        let before = self.rules.set(rel, found);
        self.before.entry(rel.to_path_buf()).or_insert(before);
        true
    }

    /// Whether the rules leave out the listed entry at `rel`. A name a rule
    /// leaves out is recorded for its folder.
    pub(crate) fn leaves_out(&mut self, rel: &Path, kind: EntryKind) -> bool {
        match self.rules.status(rel, kind) {
            Ignored::No => false,
            Ignored::BuiltIn => true,
            Ignored::Rule => {
                if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
                    self.listed
                        .entry(parent.to_path_buf())
                        .or_default()
                        .insert(name.to_os_string());
                }
                true
            }
        }
    }

    /// Restores every rules file the scan changed.
    pub(crate) fn undo(self) {
        for (dir, before) in self.before {
            self.rules.set(&dir, before);
        }
    }

    /// Replaces the left-out names of every folder the scan listed.
    pub(crate) fn settle(&mut self) {
        for (folder, names) in std::mem::take(&mut self.listed) {
            if names.is_empty() {
                self.rules.ignored.remove(&folder);
            } else {
                self.rules.ignored.insert(folder, names);
            }
        }
    }

    /// Looks for the `wanted` entries at and below the entries the rules
    /// leave out, and sets where each one is. The search lists and stats, and
    /// reads no file. It skips what it cannot open and forgets a recorded
    /// name that is gone.
    pub(crate) fn find(&mut self, root: BorrowedFd<'_>, wanted: &mut Wanted) {
        let mut missing = wanted.len();
        self.rules.ignored.retain(|folder, names| {
            if missing == 0 {
                return true;
            }
            let dir = match open_attached(root, folder, &mut |_| {}) {
                Ok(Some((dir, _))) => dir,
                Ok(None) => return false,
                Err(_) => return true,
            };
            names.retain(|name| {
                if missing == 0 {
                    return true;
                }
                match stat_at(dir.as_fd(), name) {
                    Ok(stat) => {
                        look(
                            dir.as_fd(),
                            name,
                            folder.join(name),
                            &stat,
                            wanted,
                            &mut missing,
                        );
                        true
                    }
                    Err(err) => !is_gone(&err),
                }
            });
            !names.is_empty()
        });
    }

    /// Drops the rules of every folder for which `is_folder` is false, and
    /// returns the folders whose rules file this scan changed, with what each
    /// one holds now.
    pub(crate) fn finish(
        self,
        is_folder: impl Fn(&Path) -> bool,
    ) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let Self {
            rules, mut before, ..
        } = self;
        let held = |dir: &Path| dir.as_os_str().is_empty() || is_folder(dir);
        let stale: Vec<PathBuf> = rules
            .files
            .keys()
            .filter(|dir| !held(dir))
            .cloned()
            .collect();
        for dir in stale {
            let dropped = rules.set(&dir, None);
            before.entry(dir).or_insert(dropped);
        }
        rules.ignored.retain(|dir, _| held(dir));
        before
            .into_iter()
            .filter_map(|(dir, before)| {
                let now = rules.files.get(&dir);
                (now != before.as_ref()).then(|| (dir, now.cloned()))
            })
            .collect()
    }
}

/// Reads the rules file of the folder `dir`, or returns `None` when the folder
/// holds no regular file of that name. Fails when the file cannot be read or
/// is larger than [`MAX_RULES`].
fn read_rules(dir: BorrowedFd<'_>) -> io::Result<Option<Vec<u8>>> {
    let regular = |stat: &Stat| stat.file_type == FileType::RegularFile;
    match stat_at(dir, OsStr::new(RULES_FILE)) {
        Ok(stat) if regular(&stat) => {}
        Ok(_) => return Ok(None),
        Err(err) if is_gone(&err) => return Ok(None),
        Err(err) => return Err(err),
    }
    // Without NONBLOCK, opening a FIFO that took the file's place would wait
    // for a writer.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(dir, RULES_FILE, flags, Mode::empty()) {
        Ok(file) => file,
        Err(err) if is_gone(&err.into()) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let stat = stat_fd(file.as_fd())?;
    if !regular(&stat) {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    if stat.size <= MAX_RULES {
        std::fs::File::from(file)
            .take(MAX_RULES + 1)
            .read_to_end(&mut bytes)?;
    }
    if stat.size > MAX_RULES || bytes.len() as u64 > MAX_RULES {
        return Err(io::ErrorKind::FileTooLarge.into());
    }
    Ok(Some(bytes))
}

/// Checks the entry `name` of `dir`, at `path` under the root, and everything
/// below it against `wanted`, until none is `missing`.
fn look(
    dir: BorrowedFd<'_>,
    name: &OsStr,
    path: PathBuf,
    stat: &Stat,
    wanted: &mut Wanted,
    missing: &mut usize,
) {
    let Some(entry) = stat.entry() else {
        return;
    };
    if let Some(found @ None) = wanted.get_mut(&(entry.id, entry.kind)) {
        *found = Some(path.clone());
        *missing -= 1;
    }
    if entry.kind != EntryKind::Dir || *missing == 0 {
        return;
    }
    let Ok(sub) = open_dir_at(dir, name) else {
        return;
    };
    let Ok(names) = list(sub.as_fd()) else {
        return;
    };
    for name in names {
        if *missing == 0 {
            return;
        }
        if let Ok(stat) = stat_at(sub.as_fd(), &name) {
            look(sub.as_fd(), &name, path.join(&name), &stat, wanted, missing);
        }
    }
}
