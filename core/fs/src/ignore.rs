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

    /// The form in which the matcher compares `name`. Names with the same key
    /// are one name to it: it holds the rules of a folder under the keys of
    /// the folder's path, and a file whose name has the key of [`RULES_FILE`]
    /// is a rules file.
    fn key(&self, name: &OsStr) -> OsString;
}

/// A folder's rules file as a scan read it: the path the folder was listed at,
/// and the bytes.
type Held = (PathBuf, Vec<u8>);

/// The rules of one root and what its scans have seen of them.
///
/// A new `Rules` knows no rules file. A scan reads the rules file of each
/// folder it lists, so scan the whole tree before any part of it.
pub struct Rules {
    ignore: Box<dyn Ignore + Send>,
    /// The key of [`RULES_FILE`].
    rules_file: OsString,
    /// Each folder's rules file as last read, by the keys of the folder's path
    /// ([`Ignore::key`]). The matcher holds the rules under the same keys, so
    /// two paths it takes for one folder never hold two entries here, and
    /// dropping the entry of a path that is gone cannot take the rules from a
    /// folder that is still there under another spelling.
    files: BTreeMap<PathBuf, Held>,
    /// The names each listed folder holds that a rule leaves out
    /// ([`Ignored::Rule`] only).
    ignored: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// The folders a scan reported unreadable, by the keys of their paths,
    /// until a scan reads them again or they are gone.
    unread: BTreeMap<PathBuf, PathBuf>,
}

impl Rules {
    pub fn new(ignore: Box<dyn Ignore + Send>) -> Self {
        Self {
            rules_file: ignore.key(OsStr::new(RULES_FILE)),
            ignore,
            files: BTreeMap::new(),
            ignored: BTreeMap::new(),
            unread: BTreeMap::new(),
        }
    }

    /// The keys of `path`, under which the rules of the folder there are held.
    fn key(&self, path: &Path) -> PathBuf {
        path.iter().map(|name| self.ignore.key(name)).collect()
    }

    /// The rules file among the `names` a folder lists: `.fbignore` itself, or
    /// else the first name the matcher takes for the same name, such as
    /// `.FBIGNORE`.
    fn rules_file<'n>(&self, names: &'n BTreeSet<OsString>) -> Option<&'n OsStr> {
        names
            .get(OsStr::new(RULES_FILE))
            .or_else(|| {
                names
                    .iter()
                    .find(|name| self.ignore.key(name) == self.rules_file)
            })
            .map(OsString::as_os_str)
    }

    /// Whether the entry at `path`, relative to the root, is left out. The
    /// root itself never is.
    pub fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        if path.as_os_str().is_empty() {
            return Ignored::No;
        }
        self.ignore.status(path, kind)
    }

    /// Gives the folder at `dir` the rules file `rules`, or none, and returns
    /// what the folder with those keys had.
    fn set(&mut self, dir: &Path, rules: Option<Vec<u8>>) -> Option<Held> {
        self.ignore.set_rules(dir, rules.as_deref());
        let key = self.key(dir);
        match rules {
            Some(bytes) => self.files.insert(key, (dir.to_path_buf(), bytes)),
            None => self.files.remove(&key),
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
    /// What each folder had before this scan changed its rules file or the
    /// path it is listed at, by the keys of its path.
    before: BTreeMap<PathBuf, Option<Held>>,
    /// The names a rule leaves out in each folder this scan listed.
    listed: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// The keys of the folders whose rules this scan read.
    read: BTreeSet<PathBuf>,
}

impl<'r> Pending<'r> {
    pub(crate) fn new(rules: &'r mut Rules) -> Self {
        Self {
            rules,
            before: BTreeMap::new(),
            listed: BTreeMap::new(),
            read: BTreeSet::new(),
        }
    }

    pub(crate) fn status(&self, path: &Path, kind: EntryKind) -> Ignored {
        self.rules.status(path, kind)
    }

    /// Reads the rules file of the folder `dir` at `rel`, which lists
    /// `names`. A rules file that cannot be read keeps the rules the folder
    /// last had. When the folder had none, its rules are [`RulesRead::Unknown`].
    pub(crate) fn read(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        names: &BTreeSet<OsString>,
    ) -> RulesRead {
        let key = self.rules.key(rel);
        let read = (self.rules.rules_file(names)).map(|name| read_rules(dir, name));
        if matches!(read, Some(Err(_))) && !self.rules.files.contains_key(&key) {
            return RulesRead::Unknown;
        }
        self.listed.entry(rel.to_path_buf()).or_default();
        self.read.insert(key.clone());
        let found = match read {
            Some(Ok(found)) => found,
            Some(Err(_)) => return RulesRead::Same,
            None => None,
        };
        let held = self.rules.files.get(&key);
        let same = found.as_ref() == held.map(|(_, bytes)| bytes);
        if same && held.is_none_or(|(dir, _)| dir == rel) {
            return RulesRead::Same;
        }
        let before = self.rules.set(rel, found);
        self.before.entry(key).or_insert(before);
        if same {
            RulesRead::Same
        } else {
            RulesRead::Changed
        }
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
        for (key, before) in self.before {
            let now = self.rules.files.get(&key).map(|(dir, _)| dir.clone());
            match (before, now) {
                (Some((dir, bytes)), _) => self.rules.set(&dir, Some(bytes)),
                (None, Some(dir)) => self.rules.set(&dir, None),
                (None, None) => None,
            };
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
    /// one holds now. A folder an earlier scan reported unreadable is
    /// returned too once this scan read it or it is gone, so the caller
    /// learns that its rules are known again. The folders in `unreadable` are
    /// remembered for that.
    pub(crate) fn finish(
        self,
        is_folder: impl Fn(&Path) -> bool,
        unreadable: &[PathBuf],
    ) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let Self {
            rules,
            mut before,
            read,
            ..
        } = self;
        let held = |dir: &Path| dir.as_os_str().is_empty() || is_folder(dir);
        let stale: Vec<PathBuf> = rules
            .files
            .values()
            .filter(|(dir, _)| !held(dir))
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in stale {
            let key = rules.key(&dir);
            let dropped = rules.set(&dir, None);
            before.entry(key).or_insert(dropped);
        }
        rules.ignored.retain(|dir, _| held(dir));
        let mut changed: Vec<(PathBuf, Option<Vec<u8>>)> = (before.iter())
            .filter_map(|(key, before)| match (rules.files.get(key), before) {
                (Some((_, now)), Some((_, before))) if now == before => None,
                (Some((dir, now)), _) => Some((dir.clone(), Some(now.clone()))),
                (None, Some((dir, _))) => Some((dir.clone(), None)),
                (None, None) => None,
            })
            .collect();
        let known: Vec<PathBuf> = (rules.unread.iter())
            .filter(|(key, dir)| read.contains(*key) || !held(dir))
            .map(|(key, _)| key.clone())
            .collect();
        for key in known {
            let unread = rules.unread.remove(&key).expect("an unread folder");
            let now = rules.files.get(&key);
            if !(changed.iter()).any(|(reported, _)| rules.key(reported) == key) {
                let dir = now.map_or(unread, |(dir, _)| dir.clone());
                changed.push((dir, now.map(|(_, bytes)| bytes.clone())));
            }
        }
        for dir in unreadable {
            rules.unread.insert(rules.key(dir), dir.clone());
        }
        changed
    }
}

/// What [`Pending::read`] found of a folder's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RulesRead {
    /// The rules the folder had.
    Same,
    Changed,
    /// The folder holds a rules file that cannot be read, and no earlier
    /// scan read one for it, so what the folder leaves out is not known.
    Unknown,
}

/// Reads the rules file `name` of the folder `dir`, or returns `None` when the
/// folder holds no regular file of that name. Fails when the file cannot be
/// read or is larger than [`MAX_RULES`].
fn read_rules(dir: BorrowedFd<'_>, name: &OsStr) -> io::Result<Option<Vec<u8>>> {
    let regular = |stat: &Stat| stat.file_type == FileType::RegularFile;
    match stat_at(dir, name) {
        Ok(stat) if regular(&stat) => {}
        Ok(_) => return Ok(None),
        Err(err) if is_gone(&err) => return Ok(None),
        Err(err) => return Err(err),
    }
    // Without NONBLOCK, opening a FIFO that took the file's place would wait
    // for a writer.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(dir, name, flags, Mode::empty()) {
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
