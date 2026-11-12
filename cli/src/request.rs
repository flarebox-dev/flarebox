// SPDX-License-Identifier: Apache-2.0

//! Records that let an agent run a change again without making it twice. `--request-id` names one change: the first
//! run records the command, its arguments and its outcome under the ID, and a later run with the same ID and arguments
//! returns that outcome and changes nothing, also after a restart. `koi resolve` records each answer it gives the same
//! way, keyed by the decision, because the folder's store drops a question once sync takes its answer.
//!
//! Each record is one file in the profile's state directory, 0600 in a 0700 directory. A file is written under a
//! temporary name and then moved into place, so a reader never sees half of one. A request's record is created before
//! the change, which fails when another run created it first, and gets the outcome after the change. A run that stops
//! in between leaves a record without an outcome, and the next run with that ID refuses instead of guessing whether
//! the change happened. A change that fails records nothing, so the next run with its ID tries again.

use std::fs::{DirBuilder, File};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use koidrive_driver::{Answer, Paths};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CliError, Code};

/// The most bytes a request ID has.
const MAX_ID: usize = 128;

/// An ID an agent picks for one change: 1 to 128 ASCII letters, digits, `-` and `_`, so that it names a file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RequestId(String);

impl RequestId {
    /// The value parser of `--request-id`.
    pub fn parse(text: &str) -> Result<Self, String> {
        let shaped = (1..=MAX_ID).contains(&text.len())
            && (text.chars()).all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        match shaped {
            true => Ok(RequestId(text.to_owned())),
            false => Err(format!(
                "a request ID has 1 to {MAX_ID} ASCII letters, digits, - and _"
            )),
        }
    }
}

impl TryFrom<String> for RequestId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        RequestId::parse(&text)
    }
}

impl From<RequestId> for String {
    fn from(id: RequestId) -> Self {
        id.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a request ID was used for: the command path, such as `folder restart`, and its arguments, normalized. The same
/// ID with another key is a stale request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    pub command: String,
    pub arguments: Value,
}

impl Key {
    pub fn new(command: &str, arguments: Value) -> Self {
        Key {
            command: command.to_owned(),
            arguments,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
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
                "the request {id} has no recorded outcome: an earlier run of it is still going, or it stopped before \
                 it finished. Read the state to see whether the change happened; to make it, run the command with \
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
        key: key.clone(),
        outcome: None,
    };
    loop {
        if write(&path, &record, false).map_err(|error| format!("{}: {error}", path.display()))? {
            return Ok(Begun::Claimed(Claim {
                path,
                record,
                settled: false,
            }));
        }
        if let Some(outcome) = recorded(paths, id, key)? {
            return Ok(Begun::Done(outcome));
        }
    }
}

/// A request this run makes. Dropping it without [`Claim::finish`] removes the record, for a change that failed.
pub struct Claim {
    path: PathBuf,
    record: Record,
    settled: bool,
}

impl Claim {
    /// Records the outcome of the change. Should that fail, the record stays without an outcome, since the change
    /// happened.
    pub fn finish(mut self, outcome: &impl Serialize) -> Result<(), CliError> {
        self.settled = true;
        self.record.outcome = Some(serde_json::to_value(outcome).expect("an outcome serializes"));
        write(&self.path, &self.record, true)
            .map(drop)
            .map_err(|error| format!("{}: {error}", self.path.display()).into())
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.settled {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Answered {
    answer: Answer,
}

/// Records that the decision `decision_id` was answered with `answer`. Decision IDs come from the store; one that
/// cannot name a file is not recorded.
pub fn answered(paths: &Paths, decision_id: &str, answer: Answer) -> io::Result<()> {
    let Ok(id) = RequestId::parse(decision_id) else {
        return Ok(());
    };
    write(&answer_file(paths, &id), &Answered { answer }, true).map(drop)
}

/// The answer recorded for the decision `decision_id`.
pub fn answer_of(paths: &Paths, decision_id: &str) -> Option<Answer> {
    let id = RequestId::parse(decision_id).ok()?;
    let bytes = std::fs::read(answer_file(paths, &id)).ok()?;
    Some(serde_json::from_slice::<Answered>(&bytes).ok()?.answer)
}

fn request_file(paths: &Paths, id: &RequestId) -> PathBuf {
    paths.requests().join(format!("{id}.json"))
}

fn answer_file(paths: &Paths, id: &RequestId) -> PathBuf {
    paths.answers().join(format!("{id}.json"))
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
    use std::os::unix::fs::PermissionsExt;

    use serde_json::json;

    use super::*;

    fn id(text: &str) -> RequestId {
        RequestId::parse(text).unwrap()
    }

    fn restart(folder: &str) -> Key {
        Key::new("folder restart", json!({ "folder": folder }))
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_request_id_is_a_file_name() {
        for good in [
            "r1",
            "019b2c4e-7d1f-7000-8000-000000000001",
            "deploy_2",
            &"a".repeat(128),
        ] {
            assert!(RequestId::parse(good).is_ok(), "{good}");
        }
        for bad in ["", "..", "a/b", "a.json", "r 1", "ré", &"a".repeat(129)] {
            assert!(RequestId::parse(bad).is_err(), "{bad:?}");
        }
        assert!(serde_json::from_value::<RequestId>(json!("../x")).is_err());
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
        assert_eq!(mode(&paths.requests().join("r1.json")), 0o600);
        let names: Vec<_> = (std::fs::read_dir(paths.requests()).unwrap())
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["r1.json"], "no temporary file is left");
    }

    #[test]
    fn the_same_id_with_other_arguments_is_a_stale_request_that_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &restart("/a")).unwrap() else {
            panic!("the first run claims the request");
        };
        claim.finish(&json!("first")).unwrap();
        let before = std::fs::read(paths.requests().join("r1.json")).unwrap();
        for other in [
            restart("/b"),
            Key::new("folder retry", json!({ "folder": "/a", "how": "resend" })),
        ] {
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
            std::fs::read(paths.requests().join("r1.json")).unwrap(),
            before
        );
    }

    #[test]
    fn a_run_that_stopped_before_its_outcome_leaves_the_request_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let key = restart("/a");
        let Begun::Claimed(claim) = begin(&paths, &id("r1"), &key).unwrap() else {
            panic!("the first run claims the request");
        };
        std::mem::forget(claim);
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
            [crate::report::Next::of(&["koi", "status", "--json"])]
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
        drop(claim);
        assert!(!paths.requests().join("r1.json").exists());
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

    #[test]
    fn an_answer_is_kept_by_its_decision() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        assert_eq!(answer_of(&paths, "q1"), None);
        answered(&paths, "q1", Answer::Delete).unwrap();
        assert_eq!(answer_of(&paths, "q1"), Some(Answer::Delete));
        assert_eq!(answer_of(&paths, "q2"), None);
        assert_eq!(mode(&paths.answers().join("q1.json")), 0o600);
        answered(&paths, "../q1", Answer::Delete).unwrap();
        assert_eq!(answer_of(&paths, "../q1"), None);
    }
}
