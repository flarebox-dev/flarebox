// SPDX-License-Identifier: Apache-2.0

//! The local trash `.flarebox/trash/` (design §5.2): every entry an effect
//! displaced, kept for [`TRASH_RETENTION`] or until the caller's size cap
//! evicts it, whichever comes first.
//!
//! The trash keeps its own index of each entry's size and ctime when it
//! entered, in `.flarebox/trash.json`. A process that held a descriptor open
//! across the effect can still write to a trashed file, so before reclaiming
//! an entry the collector compares it with the index and turns an entry that
//! changed into a recovered file at the sync root instead. That check is best
//! effort: a write between the check and the unlink is lost. An entry with no
//! recorded values, such as one moved in just before a crash, is never
//! deleted: it becomes a recovered file when it is due.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsFd, BorrowedFd};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustix::fs::{AtFlags, Mode, OFlags};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};

use crate::recovered::recover;
use crate::resolve::{Folder, Space, check_name};
use crate::root::{flush, rename_no_replace, replace_file};
use crate::{FileType, list, stat_at};

/// The folder inside the private folder that holds the local trash.
pub const TRASH_DIR: &str = "trash";
/// The longest an entry stays in the local trash.
pub const TRASH_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const INDEX: &str = "trash.json";
const INDEX_TEMP: &str = "trash.json.tmp";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    entries: BTreeMap<String, Kept>,
}

/// What the trash recorded about one entry when it entered.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Kept {
    /// The entry's name before it was displaced, for a recovered copy.
    name: String,
    folder: bool,
    size: u64,
    ctime_ns: i64,
    trashed_s: u64,
    /// Whether `size` and `ctime_ns` were taken when the entry entered. An
    /// entry found later has no such evidence and is never deleted.
    recorded: bool,
}

/// What one reclamation did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reclaimed {
    /// Trash entries that were deleted.
    pub removed: Vec<String>,
    /// Names at the sync root of entries that changed after they entered the
    /// trash, or whose values were never recorded, and were recovered instead
    /// of deleted. Report them as observations.
    pub recovered: Vec<OsString>,
}

impl Space {
    /// Moves `name` in `from` into the local trash as `entry`, never
    /// replacing anything there, flushes both folders, and records the entry's
    /// size and ctime with `original`, the name it had before it was
    /// displaced. All three must be single file names. `from` and the trash
    /// are checked from the root again first.
    pub fn trash(
        &self,
        from: &Folder,
        name: &OsStr,
        entry: &str,
        original: &OsStr,
        now: SystemTime,
    ) -> io::Result<()> {
        for name in [name, OsStr::new(entry), original] {
            check_name(name)?;
        }
        let trash = self.private(&[TRASH_DIR])?;
        self.recheck(from)?;
        self.recheck(&trash)?;
        rename_no_replace(from.fd(), name, trash.fd(), OsStr::new(entry))?;
        flush(from.fd())?;
        flush(trash.fd())?;
        let private = self.private(&[])?;
        let mut index = read_index(private.fd())?;
        let kept = kept(trash.fd(), entry, &original.to_string_lossy(), now, true)?;
        index.entries.insert(entry.to_owned(), kept);
        self.recheck(&private)?;
        write_index(private.fd(), &index)
    }

    /// Reclaims the local trash at `now`: entries older than
    /// [`TRASH_RETENTION`], then the oldest entries until the recorded sizes
    /// of those left add up to at most `cap` bytes. An entry whose size or
    /// ctime differs from the index, or a folder that is no longer empty, is
    /// recovered at the sync root instead of deleted. An entry the index does
    /// not know, such as one moved in just before a crash, is indexed now,
    /// counted as entering now, and recovered rather than deleted when it is
    /// due. The trash folder is checked from the root again before each entry.
    pub fn reclaim_trash(&self, now: SystemTime, cap: u64) -> io::Result<Reclaimed> {
        let trash = self.private(&[TRASH_DIR])?;
        let private = self.private(&[])?;
        let mut index = read_index(private.fd())?;
        let names = list(trash.fd())?;
        index
            .entries
            .retain(|entry, _| names.contains(OsStr::new(entry)));
        for name in &names {
            if let Some(entry) = name.to_str()
                && !index.entries.contains_key(entry)
            {
                let kept = kept(trash.fd(), entry, entry, now, false)?;
                index.entries.insert(entry.to_owned(), kept);
            }
        }

        let mut order: Vec<(String, Kept)> = index
            .entries
            .iter()
            .map(|(entry, kept)| (entry.clone(), kept.clone()))
            .collect();
        order.sort_by(|(a, x), (b, y)| (x.trashed_s, a).cmp(&(y.trashed_s, b)));
        let now_s = seconds(now);
        let mut total: u64 = order.iter().map(|(_, kept)| kept.size).sum();
        let mut reclaimed = Reclaimed::default();
        for (entry, kept) in order {
            let expired = kept.trashed_s.saturating_add(TRASH_RETENTION.as_secs()) <= now_s;
            if !expired && total <= cap {
                break;
            }
            total -= kept.size;
            self.recheck(&trash)?;
            if kept.recorded && remove_unchanged(trash.fd(), &entry, &kept)? {
                reclaimed.removed.push(entry.clone());
            } else {
                let original = match check_name(OsStr::new(&kept.name)) {
                    Ok(()) => &kept.name,
                    Err(_) => &entry,
                };
                let name = recover(
                    trash.fd(),
                    OsStr::new(&entry),
                    self.root(),
                    OsStr::new(original),
                    kept.folder,
                    now,
                )?;
                reclaimed.recovered.push(name);
            }
            index.entries.remove(&entry);
        }
        self.recheck(&private)?;
        write_index(private.fd(), &index)?;
        Ok(reclaimed)
    }
}

/// Deletes a trash entry that still matches the index. Returns false,
/// leaving it in place, when it changed.
fn remove_unchanged(trash: BorrowedFd<'_>, entry: &str, kept: &Kept) -> io::Result<bool> {
    let stat = stat_at(trash, OsStr::new(entry))?;
    let folder = stat.file_type == FileType::Directory;
    if folder != kept.folder
        || stat.size != kept.size && !folder
        || ctime(stat.ctime_ns) != kept.ctime_ns
    {
        return Ok(false);
    }
    let flags = if folder {
        AtFlags::REMOVEDIR
    } else {
        AtFlags::empty()
    };
    match rustix::fs::unlinkat(trash, entry, flags) {
        Ok(()) => Ok(true),
        Err(Errno::NOTEMPTY | Errno::EXIST) if folder => Ok(false),
        Err(err) => Err(err.into()),
    }
}

fn kept(
    trash: BorrowedFd<'_>,
    entry: &str,
    name: &str,
    now: SystemTime,
    recorded: bool,
) -> io::Result<Kept> {
    let stat = stat_at(trash, OsStr::new(entry))?;
    let folder = stat.file_type == FileType::Directory;
    Ok(Kept {
        name: name.to_owned(),
        folder,
        size: if folder { 0 } else { stat.size },
        ctime_ns: ctime(stat.ctime_ns),
        trashed_s: seconds(now),
        recorded,
    })
}

fn ctime(ns: i128) -> i64 {
    i64::try_from(ns).unwrap_or(i64::MAX)
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn read_index(private: BorrowedFd<'_>) -> io::Result<Index> {
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file = match rustix::fs::openat(private, INDEX, flags, Mode::empty()) {
        Ok(file) => file,
        Err(Errno::NOENT) => return Ok(Index::default()),
        Err(err) => return Err(err.into()),
    };
    let mut text = String::new();
    File::from(file).read_to_string(&mut text)?;
    serde_json::from_str(&text).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

fn write_index(private: BorrowedFd<'_>, index: &Index) -> io::Result<()> {
    replace_file(
        private.as_fd(),
        INDEX,
        INDEX_TEMP,
        &serde_json::to_vec_pretty(index)?,
    )
}
