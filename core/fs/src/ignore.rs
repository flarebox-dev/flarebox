// SPDX-License-Identifier: Apache-2.0

//! The rules that leave paths out of scans (design §5.10): the matcher scans
//! ask, the rules files they read for it, and the search that tells a move
//! into a left-out path from a removal.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rustix::fs::{Dir, FileType, Mode, OFlags};

use crate::{
    Cut, Entry, EntryKind, FileId, Stat, is_gone, open_attached, open_dir_at, stat_at, stat_fd,
};

/// The name of a rules file.
pub const RULES_FILE: &str = ".koiignore";

/// The largest rules file a scan reads.
const MAX_RULES: u64 = 1 << 20;

/// Why a path is left out of scans.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ignored {
    No,
    /// A platform file such as `.DS_Store`. No rule can bring it back.
    BuiltIn,
    /// A `.koiignore` rule or the device's own list.
    Rule,
}

/// The matcher scans ask. The driver implements it over
/// `koidrive_ignore::Matcher`.
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
    budget: usize,
    search: Search,
}

#[derive(Default)]
struct Search {
    walks: u64,
    walk: Option<Walk>,
    pending: HashMap<Key, u64>,
    decided: HashMap<Key, Option<PathBuf>>,
    held: BTreeMap<PathBuf, Entry>,
    scanned: Vec<PathBuf>,
    reseeded: Vec<PathBuf>,
}

impl Search {
    fn prune(&mut self) {
        let keys: HashSet<Key> = (self.held.values())
            .map(|entry| (entry.id, entry.kind))
            .collect();
        self.pending.retain(|key, _| keys.contains(key));
        self.decided.retain(|key, _| keys.contains(key));
        if self.pending.is_empty() {
            self.walk = None;
        }
    }
}

pub(crate) struct Seeded {
    scanned: Vec<PathBuf>,
    reseeded: Vec<PathBuf>,
}

impl Seeded {
    pub(crate) fn unscanned(&self, path: &Path) -> bool {
        let under = |paths: &[PathBuf]| paths.iter().any(|scope| path.starts_with(scope));
        !under(&self.scanned) || under(&self.reseeded)
    }
}

struct Listing {
    dir: PathBuf,
    names: Vec<OsString>,
}

struct Walk {
    listings: Vec<Listing>,
    folders: Vec<Folder>,
}

struct Folder {
    dir: PathBuf,
    id: FileId,
    entries: Entries,
}

enum Entries {
    Open(Dir),
    Read(Vec<OsString>),
}

const OPEN_FOLDERS: usize = 16;

const SEARCH_BUDGET: usize = 20_000;

impl Rules {
    pub fn new(ignore: Box<dyn Ignore + Send>) -> Self {
        Self {
            rules_file: ignore.key(OsStr::new(RULES_FILE)),
            ignore,
            files: BTreeMap::new(),
            ignored: BTreeMap::new(),
            budget: SEARCH_BUDGET,
            search: Search::default(),
        }
    }

    /// The same rules with a search that looks at `budget` left-out entries
    /// per rescan, and at least one.
    pub fn with_search_budget(mut self, budget: usize) -> Self {
        self.budget = budget.max(1);
        self
    }

    pub(crate) fn searching(&self) -> bool {
        !self.search.pending.is_empty()
    }

    pub(crate) fn held(&self) -> Option<&BTreeMap<PathBuf, Entry>> {
        Some(&self.search.held).filter(|held| !held.is_empty())
    }

    pub(crate) fn forget(&mut self, scope: &Path) {
        let search = &mut self.search;
        search.held.retain(|path, _| !path.starts_with(scope));
        search.prune();
        search.reseeded.push(scope.to_path_buf());
        search.reseeded = crate::outermost(std::mem::take(&mut search.reseeded));
    }

    /// The keys of `path`, under which the rules of the folder there are held.
    fn key(&self, path: &Path) -> PathBuf {
        path.iter().map(|name| self.ignore.key(name)).collect()
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
pub(crate) type Wanted = HashMap<Key, Option<PathBuf>>;

pub(crate) type Key = (FileId, EntryKind);

/// The rules during one scan. Until the scan is published, [`Pending::undo`]
/// takes back everything it changed in them.
pub(crate) struct Pending<'r> {
    rules: &'r mut Rules,
    /// What each folder had before this scan changed its rules file or the
    /// path it is listed at, by the keys of its path.
    before: BTreeMap<PathBuf, Option<Held>>,
    /// The names a rule leaves out in each folder this scan listed.
    listed: BTreeMap<PathBuf, BTreeSet<OsString>>,
    /// The folders whose rules this scan established, by their paths as
    /// listed.
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
    /// `names`: the regular file among the names the matcher takes for
    /// `.koiignore`, such as `.KOIIGNORE`. A folder or a link of such a name is
    /// not one. A rules file that cannot be read keeps the rules the folder
    /// last had. When the folder had none, its rules are [`RulesRead::Unknown`],
    /// and so are they when it holds two rules files.
    pub(crate) fn read(
        &mut self,
        dir: BorrowedFd<'_>,
        rel: &Path,
        names: &BTreeSet<OsString>,
    ) -> RulesRead {
        let files: Vec<&OsString> = (names.iter())
            .filter(|name| self.rules.ignore.key(name) == self.rules.rules_file)
            .filter(|name| match stat_at(dir, name) {
                Ok(stat) => stat.file_type == FileType::RegularFile,
                Err(err) => !is_gone(&err),
            })
            .collect();
        if files.len() > 1 {
            return RulesRead::Unknown;
        }
        let key = self.rules.key(rel);
        let read = files.first().map(|name| read_rules(dir, name));
        if matches!(read, Some(Err(_))) && !self.rules.files.contains_key(&key) {
            return RulesRead::Unknown;
        }
        self.listed.entry(rel.to_path_buf()).or_default();
        self.read.insert(rel.to_path_buf());
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
            if let Some(Walk { listings: walk, .. }) = &mut self.rules.search.walk {
                let before = self.rules.ignored.get(&folder);
                let added: Vec<OsString> = (names.iter().rev())
                    .filter(|name| !before.is_some_and(|before| before.contains(*name)))
                    .cloned()
                    .collect();
                if !added.is_empty() {
                    walk.push(Listing {
                        dir: folder.clone(),
                        names: added,
                    });
                }
            }
            if names.is_empty() {
                self.rules.ignored.remove(&folder);
            } else {
                self.rules.ignored.insert(folder, names);
            }
        }
    }

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
                        if let Some(entry) = stat.entry()
                            && let Some(found @ None) = wanted.get_mut(&(entry.id, entry.kind))
                        {
                            *found = Some(folder.join(name));
                            missing -= 1;
                        }
                        true
                    }
                    Err(err) => !is_gone(&err),
                }
            });
            !names.is_empty()
        });
    }

    pub(crate) fn search(
        &mut self,
        root: BorrowedFd<'_>,
        wanted: &mut Wanted,
        fresh: &HashSet<Key>,
        bounded: bool,
        cut: &mut dyn FnMut(Cut<'_>),
    ) -> HashSet<Key> {
        let Rules {
            ignored,
            budget,
            search,
            ..
        } = &mut *self.rules;
        let Search {
            walks,
            walk: walking,
            pending,
            decided,
            ..
        } = search;
        let next = *walks + 1;
        for (key, found) in wanted.iter_mut() {
            match decided.get(key) {
                Some(Some(path)) if holds(root, path, *key) => *found = Some(path.clone()),
                Some(None) if !fresh.contains(key) => {}
                _ => {
                    decided.remove(key);
                    let needs = pending.entry(*key).or_insert(next);
                    if fresh.contains(key) {
                        *needs = next;
                    }
                }
            }
        }
        let mut left = if bounded { *budget } else { usize::MAX };
        let mut looked = false;
        while (left > 0 || !looked) && !pending.is_empty() {
            let walk = walking.get_or_insert_with(|| {
                *walks += 1;
                Walk {
                    listings: (ignored.iter())
                        .map(|(dir, names)| Listing {
                            dir: dir.clone(),
                            names: names.iter().rev().cloned().collect(),
                        })
                        .rev()
                        .collect(),
                    folders: Vec::new(),
                }
            });
            let mut opened: Option<(PathBuf, OwnedFd)> = None;
            let mut moved = false;
            while (left > 0 || !looked) && !pending.is_empty() {
                let (dir, name) = match walk.folders.last_mut() {
                    Some(Folder {
                        dir,
                        entries: Entries::Open(entries),
                        ..
                    }) => {
                        let Some(Ok(item)) = entries.read() else {
                            walk.folders.pop();
                            continue;
                        };
                        let name = OsStr::from_bytes(item.file_name().to_bytes()).to_os_string();
                        if name == "." || name == ".." {
                            continue;
                        }
                        (dir.clone(), name)
                    }
                    Some(Folder {
                        dir,
                        entries: Entries::Read(names),
                        ..
                    }) => {
                        let Some(name) = names.pop() else {
                            walk.folders.pop();
                            continue;
                        };
                        (dir.clone(), name)
                    }
                    None => {
                        let Some(listing) = walk.listings.last_mut() else {
                            break;
                        };
                        let Some(name) = listing.names.pop() else {
                            walk.listings.pop();
                            continue;
                        };
                        (listing.dir.clone(), name)
                    }
                };
                left = left.saturating_sub(1);
                looked = true;
                let top = walk.folders.last();
                if !matches!(
                    top,
                    Some(Folder {
                        entries: Entries::Open(_),
                        ..
                    })
                ) && opened.as_ref().is_none_or(|(at, _)| *at != dir)
                {
                    left = left.saturating_sub(dir.components().count());
                    let id = top.map(|folder| folder.id);
                    opened = match open_attached(root, &dir, cut) {
                        Ok(Some((fd, chain))) if id.is_none_or(|id| chain.last() == Some(&id)) => {
                            Some((dir.clone(), fd))
                        }
                        _ => {
                            if top.is_some() {
                                walk.folders.pop();
                            } else {
                                walk.listings.pop();
                            }
                            continue;
                        }
                    };
                }
                let parent = match (walk.folders.last(), &opened) {
                    (
                        Some(Folder {
                            entries: Entries::Open(entries),
                            ..
                        }),
                        _,
                    ) => entries.fd().ok(),
                    (_, Some((_, fd))) => Some(fd.as_fd()),
                    _ => None,
                };
                let Some(parent) = parent else { continue };
                let path = dir.join(&name);
                cut(Cut::Searching(&path));
                let Some(entry) = stat_at(parent, &name).ok().and_then(|stat| stat.entry()) else {
                    continue;
                };
                let key = (entry.id, entry.kind);
                if pending.contains_key(&key) {
                    left = left.saturating_sub(path.components().count());
                    if !holds(root, &path, key) {
                        moved = true;
                        break;
                    }
                    pending.remove(&key);
                    if let Some(found) = wanted.get_mut(&key) {
                        *found = Some(path.clone());
                    }
                    decided.insert(key, Some(path.clone()));
                }
                if entry.kind != EntryKind::Dir {
                    continue;
                }
                left = left.saturating_sub(1);
                let Ok(sub) = open_dir_at(parent, &name) else {
                    continue;
                };
                let Ok(entries) = Dir::read_from(&sub) else {
                    continue;
                };
                let open_count = (walk.folders.iter())
                    .filter(|folder| matches!(folder.entries, Entries::Open(_)))
                    .count();
                if open_count >= OPEN_FOLDERS
                    && let Some(oldest) = (walk.folders.iter_mut())
                        .find(|folder| matches!(folder.entries, Entries::Open(_)))
                    && let Entries::Open(dir) = &mut oldest.entries
                {
                    let mut names = Vec::new();
                    while let Some(Ok(item)) = dir.read() {
                        let name = OsStr::from_bytes(item.file_name().to_bytes()).to_os_string();
                        if name != "." && name != ".." {
                            names.push(name);
                        }
                    }
                    left = left.saturating_sub(names.len());
                    names.reverse();
                    oldest.entries = Entries::Read(names);
                }
                walk.folders.push(Folder {
                    dir: path,
                    id: entry.id,
                    entries: Entries::Open(entries),
                });
            }
            if moved {
                *walking = None;
                let again = *walks + 1;
                for needs in pending.values_mut() {
                    *needs = again;
                }
                continue;
            }
            if walk.listings.is_empty() && walk.folders.is_empty() {
                *walking = None;
                let done = *walks;
                pending.retain(|key, needs| {
                    let open = *needs > done;
                    if !open {
                        decided.insert(*key, None);
                    }
                    open
                });
            }
        }
        if pending.is_empty() {
            *walking = None;
        }
        (wanted.keys())
            .filter(|key| pending.contains_key(key))
            .copied()
            .collect()
    }

    pub(crate) fn held_in(&self, scopes: &[PathBuf]) -> BTreeMap<PathBuf, Entry> {
        (self.rules.search.held.iter())
            .filter(|(path, _)| scopes.iter().any(|scope| path.starts_with(scope)))
            .map(|(path, entry)| (path.clone(), *entry))
            .collect()
    }

    pub(crate) fn take_held(
        &mut self,
        scopes: &[PathBuf],
        stays: &[PathBuf],
    ) -> BTreeMap<PathBuf, Entry> {
        let held = &mut self.rules.search.held;
        let (taken, kept) = std::mem::take(held).into_iter().partition(|(path, _)| {
            scopes.iter().any(|scope| path.starts_with(scope))
                && !stays.iter().any(|kept| path.starts_with(kept))
        });
        *held = kept;
        taken
    }

    pub(crate) fn hold(&mut self, held: BTreeMap<PathBuf, Entry>) {
        let search = &mut self.rules.search;
        search.held.extend(held);
        search.prune();
    }

    pub(crate) fn held(&self) -> impl Iterator<Item = &PathBuf> {
        self.rules.search.held.keys()
    }

    pub(crate) fn cover(&mut self, scopes: &[PathBuf], unread: &[PathBuf]) -> Seeded {
        let search = &mut self.rules.search;
        let seeded = Seeded {
            scanned: search.scanned.clone(),
            reseeded: search.reseeded.clone(),
        };
        let under =
            |path: &Path, paths: &[PathBuf]| paths.iter().any(|above| path.starts_with(above));
        let still: Vec<PathBuf> = (unread.iter())
            .filter(|path| seeded.unscanned(path))
            .chain(seeded.reseeded.iter().filter(|path| under(path, unread)))
            .cloned()
            .collect();
        search.scanned.extend(scopes.iter().cloned());
        search.scanned = crate::outermost(std::mem::take(&mut search.scanned));
        search.reseeded.retain(|path| !under(path, scopes));
        search.reseeded.extend(still);
        search.reseeded = crate::outermost(std::mem::take(&mut search.reseeded));
        seeded
    }

    /// Drops the rules of every folder for which `is_folder` is false, and
    /// returns the folders whose rules file this scan changed, with what each
    /// one holds now, and the folders whose rules this scan established.
    pub(crate) fn finish(
        self,
        is_folder: impl Fn(&Path) -> bool,
    ) -> (Vec<RulesFile>, Vec<PathBuf>) {
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
        let changed = (before.iter())
            .filter_map(|(key, before)| match (rules.files.get(key), before) {
                (Some((_, now)), Some((_, before))) if now == before => None,
                (Some((dir, now)), _) => Some((dir.clone(), Some(now.clone()))),
                (None, Some((dir, _))) => Some((dir.clone(), None)),
                (None, None) => None,
            })
            .collect();
        (changed, read.into_iter().filter(|dir| held(dir)).collect())
    }
}

/// A folder whose rules file a scan found changed, with the bytes it holds
/// now, or `None` for a folder that has none any more.
pub(crate) type RulesFile = (PathBuf, Option<Vec<u8>>);

/// What [`Pending::read`] found of a folder's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RulesRead {
    /// The rules the folder had.
    Same,
    Changed,
    /// The folder holds a rules file that cannot be read, and no earlier
    /// scan read one for it, or two rules files whose names differ only in
    /// case, so what the folder leaves out is not known.
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

fn holds(root: BorrowedFd<'_>, path: &Path, key: Key) -> bool {
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    let Ok(Some((dir, _))) = open_attached(root, parent, &mut |_| {}) else {
        return false;
    };
    (stat_at(dir.as_fd(), name).ok())
        .and_then(|stat| stat.entry())
        .is_some_and(|entry| (entry.id, entry.kind) == key)
}
