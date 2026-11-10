// SPDX-License-Identifier: Apache-2.0

//! Recovered files (design §5.2): an entry that cannot go back to its name is
//! given a free name of its own, so it becomes an ordinary new local file.

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::fd::BorrowedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::root::{flush, rename_no_replace};

/// The longest file name most file systems accept, in bytes.
const NAME_MAX: usize = 255;
/// How many numbered names a recovery tries before giving up.
const ATTEMPTS: u32 = 10_000;

/// The name a recovered copy of `original` gets on `date` (`YYYY-MM-DD`):
/// `<stem> (recovered <date>)<ext>`, and `<stem> (recovered <date> <n>)<ext>`
/// for the `n`th attempt after a collision. A folder or a name without an
/// extension keeps its whole name as the stem. The stem is shortened, on a
/// character boundary when it is UTF-8, so the name fits in 255 bytes.
pub fn recovered_name(original: &OsStr, folder: bool, date: &str, n: u32) -> OsString {
    let bytes = original.as_bytes();
    let split = match bytes.iter().rposition(|&byte| byte == b'.') {
        Some(dot) if dot > 0 && !folder => dot,
        _ => bytes.len(),
    };
    let (stem, ext) = bytes.split_at(split);
    let label = match n {
        1 => format!(" (recovered {date})"),
        n => format!(" (recovered {date} {n})"),
    };
    let room = NAME_MAX.saturating_sub(label.len() + ext.len());
    let mut keep = stem.len().min(room);
    while keep > 0 && keep < stem.len() && stem[keep] & 0xC0 == 0x80 {
        keep -= 1;
    }
    let mut name = stem[..keep].to_vec();
    name.extend_from_slice(label.as_bytes());
    name.extend_from_slice(ext);
    OsString::from_vec(name)
}

/// Renames `name` in `from` to the first free recovered name for
/// `original` in `to`, never replacing an entry, flushes both folders and
/// returns the name it took.
pub(crate) fn recover(
    from: BorrowedFd<'_>,
    name: &OsStr,
    to: BorrowedFd<'_>,
    original: &OsStr,
    folder: bool,
    now: SystemTime,
) -> io::Result<OsString> {
    let date = utc_date(now);
    for n in 1..=ATTEMPTS {
        let candidate = recovered_name(original, folder, &date, n);
        match rename_no_replace(from, name, to, &candidate) {
            Ok(()) => {
                flush(from)?;
                flush(to)?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{ATTEMPTS} recovered names for {original:?} are taken"),
    ))
}

/// The UTC calendar date of `time` as `YYYY-MM-DD`.
pub(crate) fn utc_date(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    };
    // Howard Hinnant's civil_from_days.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}
