// SPDX-License-Identifier: Apache-2.0

//! Records that let an agent run a change again without making it twice. `--request-id` names one change: the first
//! run records the command, its arguments and its outcome under the ID, and a later run with the same ID and arguments
//! returns that outcome and changes nothing, also after a restart.
//!
//! Each record is one file in the profile's state directory, 0600 in a 0700 directory, named by the SHA-256 of the ID
//! so that IDs differing only in case stay apart on a file system that folds case. A file is written under a
//! temporary name and then moved into place, so a reader never sees half of one. A request's record is created before
//! the change, which fails when another run created it first, and gets the outcome after the change. A run that stops
//! in between, or that cannot tell whether its change happened, leaves a record without an outcome, and the next run
//! with that ID refuses instead of guessing. A change known to have failed records nothing, so the next run with its
//! ID tries again.

use std::fs::{DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use koidrive_driver::Paths;
use koidrive_ipc::{Code, Key, RequestId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::CliError;

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    id: RequestId,
    key: Key,
    /// None while the change runs, and after a run that stopped before it recorded what happened.
    outcome: Option<Value>,
}

/// The outcome of the earlier run of `id`, or none when no run of it is recorded. It fails as a stale request when
/// the ID was used with other arguments, and as interrupted when the earlier run has not recorded its outcome.
pub fn recorded(paths: &Paths, id: &RequestId, key: &Key) -> Result<Option<Value>, CliError> {
    let path = request_file(paths, id);
    let record = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Record>(&bytes).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display()).into()),
    };
    match record {
        Some(record) if record.key != *key => Err(CliError::new(
            Code::StaleDecision,
            format!(
                "stale request: the request ID {id} was used for {} with other arguments; repeat that call as it was, \
                 or pick a new request ID for this one",
                record.key.command
            ),
        )),
        Some(Record {
            outcome: Some(outcome),
            ..
        }) => Ok(Some(outcome)),
        _ => Err(CliError::new(
            Code::Interrupted,
            format!(
                "the request {id} has no recorded outcome: an earlier run of it is still going, stopped before it \
                 finished, or could not tell whether its change happened. Read the state to see whether the change happened; to make it, run the command with \
                 a new request ID"
            ),
        )
        .then(&["koi", "status", "--json"])),
    }
}

/// What [`begin`] found.
pub enum Begun {
    /// An earlier run of the request finished with this outcome.
    Done(Value),
    /// No run of the request is recorded. This one makes the change and records it.
    Claimed(Claim),
}

/// Claims `id` for a run of `key` before it makes the change, or returns the outcome of the run that claimed it first.
pub fn begin(paths: &Paths, id: &RequestId, key: &Key) -> Result<Begun, CliError> {
    let path = request_file(paths, id);
    let record = Record {
        id: id.clone(),
        key: key.clone(),
        outcome: None,
    };
    loop {
        if write(&path, &record, false).map_err(|error| format!("{}: {error}", path.display()))? {
            return Ok(Begun::Claimed(Claim { path, record }));
        }
        if let Some(outcome) = recorded(paths, id, key)? {
            return Ok(Begun::Done(outcome));
        }
    }
}

/// A request this run makes. Unless [`Claim::finish`] or [`Claim::abandon`] settles it, the record stays without an
/// outcome: a run cancelled or stopped while it made the change cannot tell whether the change happened.
pub struct Claim {
    path: PathBuf,
    record: Record,
}

impl Claim {
    /// Records the outcome of the change. Should that fail, the record stays without an outcome, since the change
    /// happened.
    pub fn finish(mut self, outcome: &impl Serialize) -> Result<(), CliError> {
        self.record.outcome = Some(serde_json::to_value(outcome).expect("an outcome serializes"));
        write(&self.path, &self.record, true)
            .map(drop)
            .map_err(|error| format!("{}: {error}", self.path.display()).into())
    }

    /// Removes the record, for a change known to have failed without an effect, so that the next run with the ID
    /// tries again.
    pub fn abandon(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(crate) fn request_file(paths: &Paths, id: &RequestId) -> PathBuf {
    let name: String = (Sha256::digest(id.as_str().as_bytes()).iter())
        .map(|byte| format!("{byte:02x}"))
        .collect();
    paths.requests().join(format!("{name}.json"))
}

/// Writes `value` to `path` whole: to a temporary file in the same directory, then moved into place. Without
/// `replace` it returns false, writing nothing, when `path` exists.
fn write(path: &Path, value: &impl Serialize, replace: bool) -> io::Result<bool> {
    let dir = path.parent().expect("a record has a directory");
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let mut temp = tempfile::Builder::new().prefix(".").tempfile_in(dir)?;
    temp.write_all(&serde_json::to_vec(value).expect("a record serializes"))?;
    temp.as_file().sync_all()?;
    if replace {
        temp.persist(path).map_err(|error| error.error)?;
    } else {
        match temp.persist_noclobber(path) {
            Ok(_) => {}
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => return Ok(false),
            Err(error) => return Err(error.error),
        }
    }
    File::open(dir)?.sync_all()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    fn id(text: &str) -> RequestId {
        RequestId::parse(text).unwrap()
    }

    fn restart(folder: &str) -> Key {
        Key::restart(Path::new(folder))
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_finished_request_returns_its_first_outcome_and_runs_no_second_time() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/home/ada/Docs");
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &key).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.finish(&json!({ "response": "restarted" })).unwrap();
        for _ in 0..2 {
            let Begun::Done(outcome) = begin(&paths, &id("r1"), &key).unwrap() else {
                panic!("a second run makes no change");
            };
            assert_eq!(outcome, json!({ "response": "restarted" }));
        }
        assert_eq!(
            recorded(&paths, &id("r1"), &key).unwrap(),
            Some(json!({ "response": "restarted" }))
        );
        assert_eq!(recorded(&paths, &id("r2"), &key).unwrap(), None);
        assert_eq!(mode(&paths.requests()), 0o700);
        assert_eq!(mode(&request_file(&paths, &id("r1"))), 0o600);
        let names: Vec<_> = (std::fs::read_dir(paths.requests()).unwrap())
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let file = request_file(&paths, &id("r1"));
        assert_eq!(
            names,
            [file.file_name().unwrap()],
            "no temporary file is left"
        );
    }

    #[test]
    fn folders_whose_names_differ_only_in_bytes_that_are_not_utf_8_have_distinct_keys() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let folder = |name: &[u8]| Path::new(OsStr::from_bytes(name)).to_path_buf();
        let (first, second) = (folder(b"/a/\xff"), folder(b"/a/\xfe"));
        assert_eq!(first.display().to_string(), second.display().to_string());
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &Key::restart(&first)).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.finish(&json!("first")).unwrap();
        let Err(error) = begin(&paths, &id("r1"), &Key::restart(&second)) else {
            panic!("the second folder reused the record of the first");
        };
        assert_eq!(error.code, Code::StaleDecision);
        assert_ne!(Key::retry(&first, false), Key::retry(&second, false));
        assert_ne!(
            Key::restore(&first, None, None),
            Key::restore(&second, None, None)
        );
    }

    #[test]
    fn the_same_id_with_other_arguments_is_a_stale_request_that_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &restart("/a")).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.finish(&json!("first")).unwrap();
        let before = std::fs::read(request_file(&paths, &id("r1"))).unwrap();
        for other in [restart("/b"), Key::retry(Path::new("/a"), false)] {
            let Err(error) = begin(&paths, &id("r1"), &other) else {
                panic!("{other:?} reused the ID");
            };
            assert_eq!(error.code, Code::StaleDecision);
            assert_eq!(error.code.exit(), 7);
            assert!(
                error
                    .message
                    .starts_with("stale request: the request ID r1 was used for folder restart"),
                "{}",
                error.message
            );
            assert!(error.next.is_empty());
        }
        assert_eq!(
            std::fs::read(request_file(&paths, &id("r1"))).unwrap(),
            before
        );
    }

    #[test]
    fn request_ids_that_differ_in_case_are_different_requests() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/a");
        let Begun::Claimed(claim) = begin(&paths, &id("Case-A"), &key).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.finish(&json!("first")).unwrap();
        let Begun::Claimed(claim) = begin(&paths, &id("case-a"), &key).unwrap() else {
            panic!("another ID replayed the first request");
        };
        claim.finish(&json!("second")).unwrap();
        assert_eq!(
            recorded(&paths, &id("Case-A"), &key).unwrap(),
            Some(json!("first"))
        );
        assert_eq!(
            recorded(&paths, &id("case-a"), &key).unwrap(),
            Some(json!("second"))
        );
        let record: Value =
            serde_json::from_slice(&std::fs::read(request_file(&paths, &id("Case-A"))).unwrap())
                .unwrap();
        assert_eq!(record["id"], json!("Case-A"));
    }

    #[test]
    fn a_run_cancelled_or_stopped_before_its_outcome_leaves_the_request_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/a");
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &key).unwrap() else {
            panic!("the first run claims the request");
        };
        drop(claim);
        let Err(error) = begin(&paths, &id("r1"), &key) else {
            panic!("a second run went ahead while the first one's outcome is unknown");
        };
        assert_eq!(error.code, Code::Interrupted);
        assert!(
            error.message.contains("new request ID"),
            "{}",
            error.message
        );
        assert_eq!(
            error.next,
            [koidrive_ipc::Next::of(&["koi", "status", "--json"])]
        );
    }

    #[test]
    fn a_change_that_failed_records_nothing_so_the_id_runs_again() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/a");
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &key).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.abandon();
        assert!(!request_file(&paths, &id("r1")).exists());
        assert!(matches!(
            begin(&paths, &id("r1"), &key).unwrap(),
            Begun::Claimed(_)
        ));
    }

    #[test]
    fn of_runs_of_one_request_at_once_only_one_claims_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/a");
        let runs: Vec<_> = (0..8)
            .map(|_| {
                let (paths, key) = (paths.clone(), key.clone());
                std::thread::spawn(move || match begin(&paths, &id("r1"), &key) {
                    Ok(Begun::Claimed(claim)) => {
                        std::mem::forget(claim);
                        true
                    }
                    Err(error) => {
                        assert_eq!(error.code, Code::Interrupted, "{}", error.message);
                        false
                    }
                    Ok(Begun::Done(outcome)) => {
                        panic!("no run finished, yet {outcome} is recorded")
                    }
                })
            })
            .collect();
        let claimed = runs
            .into_iter()
            .filter_map(|run| run.join().unwrap().then_some(()))
            .count();
        assert_eq!(claimed, 1);
    }
}
