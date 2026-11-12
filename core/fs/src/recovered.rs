// SPDX-License-Identifier: Apache-2.0

use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::resolve::{Folder, Space, Unresolved};
use crate::root::{flush, rename_no_replace};

const NAME_MAX: usize = 255;
const ATTEMPTS: u32 = 10_000;

pub fn recovered_name(original: &OsStr, folder: bool, date: &str, n: u32) -> OsString {
    let bytes = original.as_bytes();
    let label = match n {
        1 => format!(" (recovered {date})"),
        n => format!(" (recovered {date} {n})"),
    };
    let split = match bytes.iter().rposition(|&byte| byte == b'.') {
        Some(dot) if dot > 0 && !folder && label.len() + bytes.len() - dot <= NAME_MAX => dot,
        _ => bytes.len(),
    };
    let (stem, ext) = bytes.split_at(split);
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

impl Space {
    pub(crate) fn move_to_recovered_name(
        &self,
        from: &Folder,
        name: &OsStr,
        to: &Folder,
        original: &OsStr,
        folder: bool,
        now: SystemTime,
    ) -> io::Result<PathBuf> {
        let date = utc_date(now);
        let root = self.root_folder()?;
        let mut to = to;
        let mut n = 1;
        while n <= ATTEMPTS {
            self.recheck(from)?;
            match self.recheck(to) {
                Ok(()) => {}
                Err(
                    Unresolved::Missing(_)
                    | Unresolved::NotAFolder(_)
                    | Unresolved::Replaced { .. },
                ) => {
                    to = &root;
                    n = 1;
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
            let candidate = recovered_name(original, folder, &date, n);
            match rename_no_replace(from.fd(), name, to.fd(), &candidate) {
                Ok(()) => {
                    flush(from.fd())?;
                    flush(to.fd())?;
                    return Ok(to.path().join(candidate));
                }
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => n += 1,
                Err(err) => return Err(err),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{ATTEMPTS} recovered names for {original:?} are taken"),
        ))
    }
}

pub(crate) fn utc_date(time: SystemTime) -> String {
    let secs = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_secs() as i64,
        Err(before) => -(before.duration().as_secs() as i64),
    };
    // Howard Hinnant's civil_from_days: https://howardhinnant.github.io/date_algorithms.html#civil_from_days
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
