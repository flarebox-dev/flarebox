// SPDX-License-Identifier: Apache-2.0

//! What `--json` prints: one versioned envelope per command, and the CLI's own types for the data in it. The driver's,
//! engine's and server's types never reach the output directly, so they can change without changing the schema.
//! Schema 1 may gain optional fields; anything else needs a new schema version.

use koidrive_driver::{
    Account, FolderState, FolderStatus, Limit, Question, QuestionKind, RecoveryGroup,
    RestoreRejection, RestoreRequest, RestoreState, RestoreStatus, RestoreWait, Status, Wait,
    WaitKind,
};
use koidrive_proto::v1::{DeviceInfo, SpaceInfo, SpaceUsage, device_info::Kind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{CliError, Code};

/// The only schema `--json` speaks.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The command did what it was asked.
    Ok,
    /// The work is recorded and happens later; it has not finished.
    Accepted,
    Error,
}

/// A command to run next, as the arguments to pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Next {
    pub argv: Vec<String>,
}

impl Next {
    pub fn of(argv: &[&str]) -> Self {
        Next {
            argv: argv.iter().map(|arg| (*arg).to_owned()).collect(),
        }
    }
}

/// What a finished command produced: the text it prints without `--json`, and the data, decisions and next commands
/// it returns with it.
#[derive(Debug)]
pub struct Done {
    pub outcome: Outcome,
    /// The exit status, which only `status --check` sets for a successful command.
    pub exit: u8,
    pub data: Value,
    /// What it prints on stdout. A command whose result is one value, such as a new ID, prints only that value, so that
    /// `id=$(koi space create Notes)` works.
    pub text: String,
    /// What it prints on stderr after the text without `--json`: what the value is and what to do with it.
    pub note: String,
    pub decisions: Vec<Decision>,
    pub next: Vec<Next>,
}

impl Done {
    pub fn ok(data: impl Serialize, text: impl Into<String>) -> Self {
        Done {
            outcome: Outcome::Ok,
            exit: 0,
            data: serde_json::to_value(data).expect("the CLI's own types serialize"),
            text: text.into(),
            note: String::new(),
            decisions: Vec::new(),
            next: Vec::new(),
        }
    }

    /// A result that is one value: `value` alone on stdout, and `note` on stderr.
    pub fn value(data: impl Serialize, value: &str, note: impl Into<String>) -> Self {
        Done {
            note: note.into(),
            ..Done::ok(data, format!("{value}\n"))
        }
    }

    pub fn accepted(data: impl Serialize, text: impl Into<String>) -> Self {
        Done {
            outcome: Outcome::Accepted,
            ..Done::ok(data, text)
        }
    }

    /// A command whose output was already printed while it ran, such as a server deploy.
    pub fn printed() -> Self {
        Done::ok(Value::Null, "")
    }

    pub fn then(mut self, argv: &[&str]) -> Self {
        self.next.push(Next::of(argv));
        self
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ErrorBody {
    pub code: Code,
    pub message: String,
    pub retryable: bool,
}

#[derive(Debug, Serialize)]
pub struct Envelope {
    pub schema_version: u32,
    /// The subcommand path, such as `folder link`; empty for `koi` itself.
    pub command: String,
    pub outcome: Outcome,
    pub data: Value,
    pub error: Option<ErrorBody>,
    pub decisions: Vec<Decision>,
    pub next_commands: Vec<Next>,
}

impl Envelope {
    pub fn of(command: &str, done: Done) -> Self {
        Envelope {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            outcome: done.outcome,
            data: done.data,
            error: None,
            decisions: done.decisions,
            next_commands: done.next,
        }
    }

    /// The envelope of a failed command, keeping the data it read before it failed.
    pub fn failed(command: &str, error: CliError, partial: Option<Done>) -> Self {
        let (data, decisions) = partial.map_or((Value::Null, Vec::new()), |done| {
            (done.data, done.decisions)
        });
        Envelope {
            schema_version: SCHEMA_VERSION,
            command: command.to_owned(),
            outcome: Outcome::Error,
            data,
            error: Some(ErrorBody {
                code: error.code,
                retryable: error.code.retryable(),
                message: error.message,
            }),
            decisions,
            next_commands: error.next,
        }
    }
}

/// A decision sync waits on, with every item it holds and the command for each answer.
#[derive(Debug, PartialEq, Serialize)]
pub struct Decision {
    pub id: String,
    pub space_id: String,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<&'static str>,
    pub asked_at_ms: i64,
    /// False for a notice that only takes `dismiss`: it holds nothing and blocks nothing.
    pub requires_choice: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Counts>,
    pub items: Vec<Item>,
    pub answers: Vec<AnswerChoice>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Counts {
    pub deletions: u64,
    pub affected_files: u64,
    pub space_files: u64,
    pub exceeded: Vec<&'static str>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct Item {
    pub node_id: String,
    pub path: String,
    pub files: u64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct AnswerChoice {
    pub token: String,
    pub consequence: String,
    pub argv: Vec<String>,
}

impl Decision {
    pub fn of(question: &Question) -> Self {
        let counts = |deletions: &u64, affected: &u64, space: &u64, exceeded: &[Limit]| {
            Some(Counts {
                deletions: *deletions,
                affected_files: *affected,
                space_files: *space,
                exceeded: exceeded
                    .iter()
                    .map(|limit| match limit {
                        Limit::Deletions => "deletions",
                        Limit::Share => "share",
                    })
                    .collect(),
            })
        };
        let (kind, group, counts) = match &question.kind {
            QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } => (
                "mass_delete",
                None,
                counts(deletions, affected_files, space_files, exceeded),
            ),
            QuestionKind::RemoteMassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } => (
                "remote_mass_delete",
                None,
                counts(deletions, affected_files, space_files, exceeded),
            ),
            QuestionKind::AmbiguousDisappearance => ("ambiguous_disappearance", None, None),
            QuestionKind::Conflict { .. } => ("conflict", None, None),
            QuestionKind::Recovery { group } => (
                "recovery",
                Some(match group {
                    RecoveryGroup::CreatedAfterRestorePoint => "created_after_restore_point",
                    RecoveryGroup::ChangedOnThisDevice => "changed_on_this_device",
                    RecoveryGroup::CreatedOnThisDevice => "created_on_this_device",
                    RecoveryGroup::DeletedOnThisDevice => "deleted_on_this_device",
                    RecoveryGroup::RequestedBeforeReset => "requested_before_reset",
                }),
                None,
            ),
        };
        Decision {
            id: question.id.clone(),
            space_id: question.space_id.clone(),
            kind,
            group,
            asked_at_ms: question.asked_at_ms,
            requires_choice: requires_choice(question),
            counts,
            items: (question.items.iter())
                .map(|item| Item {
                    node_id: item.node_id.clone(),
                    path: item.path.clone(),
                    files: item.files,
                })
                .collect(),
            answers: (crate::choices(question).into_iter())
                .map(|choice| AnswerChoice {
                    argv: Next::of(&["koi", "resolve", &question.id, &choice.word]).argv,
                    token: choice.word,
                    consequence: choice.effect,
                })
                .collect(),
        }
    }
}

/// Whether a decision needs a choice. A notice that only takes `dismiss` holds nothing back.
pub fn requires_choice(question: &Question) -> bool {
    !matches!(question.kind, QuestionKind::RemoteMassDelete { .. })
}

#[derive(Debug, PartialEq, Serialize)]
pub struct AccountData {
    pub server: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    pub device_id: String,
    pub device_name: String,
}

impl AccountData {
    pub fn of(account: &Account) -> Self {
        AccountData {
            server: account.server.clone(),
            user_id: account.user_id.clone(),
            user_name: account.user_name.clone(),
            device_id: account.device_id.clone(),
            device_name: account.device_name.clone(),
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct FolderData {
    pub path: String,
    pub space_id: String,
    pub space_name: String,
    pub state: &'static str,
    pub history_id: Option<String>,
    pub last_seq: Option<u64>,
    pub head_seq: Option<u64>,
    pub remote_nodes: u64,
    pub local_entries: Option<u64>,
    pub questions: u64,
    pub problem: Option<String>,
    pub wait: Option<WaitData>,
}

/// A call the folder puts off: `quota` when the server refused it for a used-up limit or full storage, `retry` when
/// it kept failing for another reason. It is sent again at `until_ms`, in Unix milliseconds.
#[derive(Debug, PartialEq, Serialize)]
pub struct WaitData {
    pub kind: &'static str,
    pub until_ms: i64,
}

impl WaitData {
    fn of(wait: Wait) -> Self {
        WaitData {
            kind: match wait.kind {
                WaitKind::Quota => "quota",
                WaitKind::Retry => "retry",
            },
            until_ms: wait.until_ms,
        }
    }
}

impl FolderData {
    pub fn of(folder: &FolderStatus) -> Self {
        FolderData {
            path: folder.path.clone(),
            space_id: folder.space_id.clone(),
            space_name: folder.space_name.clone(),
            state: match folder.state {
                FolderState::Stopped => "stopped",
                FolderState::Connecting => "connecting",
                FolderState::Bootstrapping => "bootstrapping",
                FolderState::CatchingUp => "catching_up",
                FolderState::UpToDate => "up_to_date",
                FolderState::Offline => "offline",
                FolderState::Halted => "halted",
            },
            history_id: folder.history_id.clone(),
            last_seq: folder.last_seq,
            head_seq: folder.head_seq,
            remote_nodes: folder.remote_nodes,
            local_entries: folder.local_entries,
            questions: folder.questions,
            problem: folder.problem.clone(),
            wait: folder.wait.map(WaitData::of),
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct StatusData {
    pub account: Option<AccountData>,
    pub running: bool,
    pub folders: Vec<FolderData>,
    /// Every condition `status --check` weighs, whether or not `--check` was given.
    pub conditions: Vec<Condition>,
}

/// Something `status --check` reports. Connecting and catching up are not conditions: they pass on their own.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "condition", rename_all = "snake_case")]
pub enum Condition {
    NotSetUp,
    Halted {
        folder: String,
        problem: Option<String>,
    },
    Stopped {
        folder: String,
    },
    DecisionRequired {
        decision_id: String,
        space_id: String,
    },
    Offline {
        folder: String,
    },
    /// The folder puts a call off until `until_ms`, because the server refused it or it kept failing.
    Waiting {
        folder: String,
        #[serde(flatten)]
        wait: WaitData,
    },
}

impl Condition {
    /// The exit status of `status --check` for this condition alone.
    pub fn exit(&self) -> u8 {
        match self {
            Condition::NotSetUp => Code::AuthRequired.exit(),
            Condition::Halted { .. } | Condition::Stopped { .. } => Code::Halted.exit(),
            Condition::DecisionRequired { .. } => Code::DecisionRequired.exit(),
            Condition::Offline { .. } | Condition::Waiting { .. } => Code::Unavailable.exit(),
        }
    }
}

/// The conditions of a status and the decisions pending in it. A notice that only takes `dismiss` is not one.
pub fn conditions(status: &Status, questions: &[Question]) -> Vec<Condition> {
    let mut found = Vec::new();
    if status.account.is_none() {
        found.push(Condition::NotSetUp);
    }
    for folder in &status.folders {
        let path = folder.path.clone();
        match folder.state {
            FolderState::Halted => found.push(Condition::Halted {
                folder: path,
                problem: folder.problem.clone(),
            }),
            FolderState::Stopped => found.push(Condition::Stopped { folder: path }),
            FolderState::Offline => found.push(Condition::Offline { folder: path }),
            FolderState::Connecting
            | FolderState::Bootstrapping
            | FolderState::CatchingUp
            | FolderState::UpToDate => {}
        }
        if let Some(wait) = folder.wait {
            found.push(Condition::Waiting {
                folder: folder.path.clone(),
                wait: WaitData::of(wait),
            });
        }
    }
    for question in questions
        .iter()
        .filter(|question| requires_choice(question))
    {
        found.push(Condition::DecisionRequired {
            decision_id: question.id.clone(),
            space_id: question.space_id.clone(),
        });
    }
    found
}

/// The exit status of `status --check`: missing credentials first, then halted or stopped sync, then a decision
/// that needs a choice, then offline or waiting folders. 0 when there is none of them.
pub fn check_exit(conditions: &[Condition]) -> u8 {
    let precedence = [
        Code::AuthRequired,
        Code::Halted,
        Code::DecisionRequired,
        Code::Unavailable,
    ];
    precedence
        .iter()
        .map(|code| code.exit())
        .find(|exit| conditions.iter().any(|condition| condition.exit() == *exit))
        .unwrap_or(0)
}

/// One line of `koi _daemon --json`, printed as it happens.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Lifecycle {
    Started {
        version: &'static str,
        socket: String,
        folders: usize,
    },
    /// A folder's state changed, with the state named as in `status`.
    FolderState {
        folder: String,
        space_id: String,
        state: &'static str,
    },
    Stopping {
        signal: &'static str,
    },
    Stopped,
}

impl Lifecycle {
    /// The event as one line of JSON, with the schema version.
    pub fn line(&self) -> String {
        #[derive(Serialize)]
        struct Line<'a> {
            schema_version: u32,
            #[serde(flatten)]
            event: &'a Lifecycle,
        }
        let line = Line {
            schema_version: SCHEMA_VERSION,
            event: self,
        };
        serde_json::to_string(&line).expect("an event serializes")
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct UnlinkedData {
    pub space_id: String,
    pub path: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct AnsweredData {
    pub decision_id: String,
    pub answer: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct DeviceData {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub kind: &'static str,
    pub created_at_ms: i64,
    pub last_seen_ms: i64,
    pub revoked_at_ms: i64,
    pub credential_expires_at_ms: i64,
    pub issued_by: String,
    pub current: bool,
}

impl DeviceData {
    pub fn of(device: &DeviceInfo) -> Self {
        DeviceData {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            platform: device.platform.clone(),
            kind: match device.kind() {
                Kind::Device => "device",
                Kind::Agent => "agent",
                Kind::Unspecified => "unspecified",
            },
            created_at_ms: device.created_at_ms,
            last_seen_ms: device.last_seen_ms,
            revoked_at_ms: device.revoked_at_ms,
            credential_expires_at_ms: device.credential_expires_at_ms,
            issued_by: device.issued_by.clone(),
            current: device.current,
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct DevicesData {
    pub devices: Vec<DeviceData>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RevokedData {
    pub device_id: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct InvitationData {
    /// The arguments of `koi join` on the new device.
    pub join_argv: Vec<String>,
    pub expires_at_ms: i64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct SpaceData {
    pub space_id: String,
    pub name: String,
    pub created_at_ms: i64,
}

impl SpaceData {
    pub fn of(space: &SpaceInfo) -> Self {
        SpaceData {
            space_id: space.space_id.clone(),
            name: space.name.clone(),
            created_at_ms: space.created_at_ms,
        }
    }
}

#[derive(Debug, PartialEq, Serialize)]
pub struct SpacesData {
    pub spaces: Vec<SpaceData>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct UsageData {
    pub space_id: String,
    pub name: String,
    pub live_files: u64,
    pub live_bytes: u64,
    pub trashed_files: u64,
    pub trash_bytes: u64,
    pub old_versions: u64,
    pub old_version_bytes: u64,
    pub unreferenced_bytes: u64,
    pub total_bytes: u64,
}

impl UsageData {
    pub fn of(space: &SpaceInfo, usage: &SpaceUsage) -> Self {
        UsageData {
            space_id: space.space_id.clone(),
            name: space.name.clone(),
            live_files: usage.live_files,
            live_bytes: usage.live_bytes,
            trashed_files: usage.trashed_files,
            trash_bytes: usage.trash_bytes,
            old_versions: usage.old_versions,
            old_version_bytes: usage.old_version_bytes,
            unreferenced_bytes: usage.unreferenced_bytes,
            total_bytes: crate::total_bytes(usage),
        }
    }
}

/// The spaces that answered, and the ones the server refused with why.
#[derive(Debug, PartialEq, Serialize)]
pub struct UsagesData {
    pub spaces: Vec<UsageData>,
    pub refused: Vec<RefusedSpace>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RefusedSpace {
    pub space_id: String,
    pub name: String,
    pub message: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct TrashEntry {
    pub node_id: String,
    pub path: String,
    pub kind: &'static str,
    pub trash_group: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct TrashData {
    pub folder: String,
    pub entries: Vec<TrashEntry>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct VersionData {
    pub version_id: String,
    pub seq: u64,
    pub size: u64,
    pub device_id: String,
    pub created_at_ms: i64,
    pub current: bool,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct VersionsData {
    pub path: String,
    pub node_id: String,
    pub versions: Vec<VersionData>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RestoreAskedData {
    pub request_id: String,
    pub folder: String,
    pub path: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RequestsData {
    pub folder: String,
    pub requests: Vec<RequestData>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct RequestData {
    pub id: String,
    pub request: RequestedData,
    pub state: RequestState,
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestedData {
    FromTrash {
        node_id: String,
        parent_id: Option<String>,
        name: Option<String>,
    },
    Version {
        node_id: String,
        version_id: String,
    },
}

#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestState {
    Pending { waits_for: Option<&'static str> },
    Restored,
    Renamed { name: String },
    Copied { node_id: String, name: String },
    Rejected { reason: String },
    DoesNotFit,
}

impl RequestData {
    pub fn of(restore: &RestoreStatus) -> Self {
        RequestData {
            id: restore.id.clone(),
            request: match &restore.request {
                RestoreRequest::FromTrash {
                    node_id,
                    parent_id,
                    name,
                } => RequestedData::FromTrash {
                    node_id: node_id.clone(),
                    parent_id: parent_id.clone(),
                    name: name.clone(),
                },
                RestoreRequest::Version {
                    node_id,
                    version_id,
                } => RequestedData::Version {
                    node_id: node_id.clone(),
                    version_id: version_id.clone(),
                },
            },
            state: match &restore.state {
                RestoreState::Pending { waits_for } => RequestState::Pending {
                    waits_for: waits_for.map(|wait| match wait {
                        RestoreWait::NodeProblem => "node_problem",
                    }),
                },
                RestoreState::Restored => RequestState::Restored,
                RestoreState::Renamed { name } => RequestState::Renamed { name: name.clone() },
                RestoreState::Copied { node_id, name } => RequestState::Copied {
                    node_id: node_id.clone(),
                    name: name.clone(),
                },
                RestoreState::Rejected { reason } => RequestState::Rejected {
                    reason: match reason {
                        RestoreRejection::NotFound => "not_found".into(),
                        RestoreRejection::NotFile => "not_file".into(),
                        RestoreRejection::InvalidName => "invalid_name".into(),
                        RestoreRejection::ParentNotDir => "parent_not_dir".into(),
                        RestoreRejection::Cycle => "cycle".into(),
                        RestoreRejection::Root => "root".into(),
                        RestoreRejection::ReadOnly => "read_only".into(),
                        RestoreRejection::Other(reason) => reason.clone(),
                    },
                },
                RestoreState::DoesNotFit => RequestState::DoesNotFit,
            },
        }
    }
}

/// One way to set up this machine.
#[derive(Debug, PartialEq, Serialize)]
pub struct Choice {
    pub argv: Vec<String>,
    pub description: &'static str,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct ChoicesData {
    pub choices: Vec<Choice>,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct HelpData {
    pub help: String,
}

#[derive(Debug, PartialEq, Serialize)]
pub struct VersionInfoData {
    pub version: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_driver::HeldItem;

    fn folder(path: &str, state: FolderState) -> FolderStatus {
        FolderStatus {
            space_id: format!("space-{path}"),
            space_name: path.to_owned(),
            path: format!("/home/ada/{path}"),
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: None,
            wait: None,
        }
    }

    fn status(signed_in: bool, folders: Vec<FolderStatus>) -> Status {
        Status {
            account: signed_in.then(|| Account {
                server: "https://files.example.com".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d1".into(),
                device_name: "desk".into(),
            }),
            running: true,
            folders,
        }
    }

    fn question(id: &str, kind: QuestionKind) -> Question {
        Question {
            id: id.into(),
            lineage: id.into(),
            space_id: "space-Work".into(),
            asked_at_ms: 5,
            kind,
            items: vec![
                HeldItem {
                    node_id: "n1".into(),
                    path: "archive/a".into(),
                    files: 3,
                    device_id: None,
                },
                HeldItem {
                    node_id: "n2".into(),
                    path: "archive/b".into(),
                    files: 1,
                    device_id: None,
                },
            ],
        }
    }

    fn held() -> QuestionKind {
        QuestionKind::MassDelete {
            deletions: 24,
            affected_files: 1_240,
            space_files: 2_000,
            exceeded: vec![Limit::Deletions, Limit::Share],
        }
    }

    fn notice() -> QuestionKind {
        QuestionKind::RemoteMassDelete {
            deletions: 9,
            affected_files: 900,
            space_files: 1_000,
            exceeded: vec![Limit::Share],
        }
    }

    fn exit(status: &Status, questions: &[Question]) -> u8 {
        check_exit(&conditions(status, questions))
    }

    #[test]
    fn a_success_envelope_carries_every_field_and_a_null_error() {
        let done = Done::accepted(
            AnsweredData {
                decision_id: "q1".into(),
                answer: "restore".into(),
            },
            "Resolved.\n",
        )
        .then(&["koi", "status", "--json"]);
        let envelope = serde_json::to_value(Envelope::of("resolve", done)).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({
                "schema_version": 1,
                "command": "resolve",
                "outcome": "accepted",
                "data": { "decision_id": "q1", "answer": "restore" },
                "error": null,
                "decisions": [],
                "next_commands": [{ "argv": ["koi", "status", "--json"] }],
            })
        );
    }

    #[test]
    fn an_error_envelope_carries_the_code_whether_to_retry_and_the_next_command() {
        let error =
            CliError::new(Code::StaleDecision, "q1 changed").then(&["koi", "resolve", "--json"]);
        let envelope = serde_json::to_value(Envelope::failed("resolve", error, None)).unwrap();
        assert_eq!(
            envelope,
            serde_json::json!({
                "schema_version": 1,
                "command": "resolve",
                "outcome": "error",
                "data": null,
                "error": { "code": "stale_decision", "message": "q1 changed", "retryable": false },
                "decisions": [],
                "next_commands": [{ "argv": ["koi", "resolve", "--json"] }],
            })
        );
        let offline = CliError::new(Code::Unavailable, "could not reach the server");
        let envelope = Envelope::failed("space list", offline, None);
        assert!(envelope.error.unwrap().retryable);
    }

    #[test]
    fn a_decision_carries_its_counts_every_item_and_each_answer_as_argv() {
        let decision = serde_json::to_value(Decision::of(&question("q1", held()))).unwrap();
        assert_eq!(
            decision,
            serde_json::json!({
                "id": "q1",
                "space_id": "space-Work",
                "kind": "mass_delete",
                "asked_at_ms": 5,
                "requires_choice": true,
                "counts": {
                    "deletions": 24,
                    "affected_files": 1240,
                    "space_files": 2000,
                    "exceeded": ["deletions", "share"],
                },
                "items": [
                    { "node_id": "n1", "path": "archive/a", "files": 3 },
                    { "node_id": "n2", "path": "archive/b", "files": 1 },
                ],
                "answers": [
                    {
                        "token": "delete",
                        "consequence": "Delete these files on the server and other devices",
                        "argv": ["koi", "resolve", "q1", "delete"],
                    },
                    {
                        "token": "restore",
                        "consequence": "Download these files to this device again",
                        "argv": ["koi", "resolve", "q1", "restore"],
                    },
                ],
            })
        );
        let recovery = Decision::of(&question(
            "q2",
            QuestionKind::Recovery {
                group: RecoveryGroup::RequestedBeforeReset,
            },
        ));
        assert_eq!(
            (recovery.kind, recovery.group, recovery.counts),
            ("recovery", Some("requested_before_reset"), None)
        );
        assert!(!Decision::of(&question("q3", notice())).requires_choice);
    }

    #[test]
    fn a_conflict_answers_with_the_node_to_keep_or_keep_both() {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let file = |node_id: &str| ConflictFile {
            node_id: node_id.into(),
            revisions: Revisions {
                content_rev: 2,
                meta_rev: 1,
            },
        };
        let conflict = QuestionKind::Conflict {
            original: file("n1"),
            copy: file("n2"),
            sent_in: None,
        };
        let decision = Decision::of(&question("q1", conflict));
        assert_eq!(
            (decision.kind, decision.group, decision.counts),
            ("conflict", None, None)
        );
        let answers: Vec<(&str, Vec<&str>)> = (decision.answers.iter())
            .map(|answer| {
                let argv = answer.argv.iter().map(String::as_str).collect();
                (answer.token.as_str(), argv)
            })
            .collect();
        assert_eq!(
            answers,
            [
                ("keep-node:n1", vec!["koi", "resolve", "q1", "keep-node:n1"]),
                ("keep-node:n2", vec!["koi", "resolve", "q1", "keep-node:n2"]),
                ("keep-both", vec!["koi", "resolve", "q1", "keep-both"]),
            ]
        );
        assert_eq!(
            decision.answers[0].consequence,
            "Keep archive/a and move archive/b to the trash"
        );
    }

    #[test]
    fn check_exits_with_the_most_urgent_condition_first() {
        let halted = folder("Work", FolderState::Halted);
        let offline = folder("Photos", FolderState::Offline);
        let choice = question("q1", held());

        assert_eq!(
            exit(
                &status(false, vec![halted.clone(), offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            4
        );
        assert_eq!(
            exit(
                &status(true, vec![halted.clone(), offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            5
        );
        assert_eq!(
            exit(
                &status(true, vec![folder("Work", FolderState::Stopped)]),
                std::slice::from_ref(&choice)
            ),
            5,
            "a stopped folder counts as halted"
        );
        assert_eq!(
            exit(
                &status(true, vec![offline.clone()]),
                std::slice::from_ref(&choice)
            ),
            3
        );
        assert_eq!(exit(&status(true, vec![offline]), &[]), 6);
        assert_eq!(exit(&status(true, Vec::new()), &[]), 0);
    }

    #[test]
    fn a_quota_or_retry_wait_exits_unavailable_from_the_typed_field_alone() {
        let waiting = |kind| FolderStatus {
            problem: Some("no words here decide the exit".into()),
            wait: Some(Wait {
                kind,
                until_ms: 1_700_000_000_000,
            }),
            ..folder("Work", FolderState::CatchingUp)
        };
        assert_eq!(exit(&status(true, vec![waiting(WaitKind::Quota)]), &[]), 6);
        assert_eq!(exit(&status(true, vec![waiting(WaitKind::Retry)]), &[]), 6);
        assert_eq!(
            exit(
                &status(true, vec![waiting(WaitKind::Quota)]),
                &[question("q1", held())]
            ),
            3
        );
        let mut quota_text = folder("Work", FolderState::UpToDate);
        quota_text.problem = Some("QUOTA_EXCEEDED: the server's storage is full".into());
        assert_eq!(exit(&status(true, vec![quota_text]), &[]), 0);

        let found = conditions(&status(true, vec![waiting(WaitKind::Quota)]), &[]);
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            serde_json::json!([{
                "condition": "waiting", "folder": "/home/ada/Work", "kind": "quota", "until_ms": 1_700_000_000_000_i64
            }])
        );
        let data = FolderData::of(&waiting(WaitKind::Retry));
        assert_eq!(
            serde_json::to_value(&data).unwrap()["wait"],
            serde_json::json!({ "kind": "retry", "until_ms": 1_700_000_000_000_i64 })
        );
    }

    #[test]
    fn connecting_catching_up_and_a_dismiss_only_notice_never_fail_the_check() {
        let busy = status(
            true,
            vec![
                folder("A", FolderState::Connecting),
                folder("B", FolderState::Bootstrapping),
                folder("C", FolderState::CatchingUp),
                folder("D", FolderState::UpToDate),
            ],
        );
        let notice = question("q9", notice());
        assert_eq!(exit(&busy, std::slice::from_ref(&notice)), 0);
        assert!(conditions(&busy, &[notice]).is_empty());
    }

    #[test]
    fn the_status_data_reports_every_condition_with_its_folder() {
        let mut halted = folder("Work", FolderState::Halted);
        halted.problem = Some("the folder is gone".into());
        let found = conditions(
            &status(false, vec![halted, folder("Photos", FolderState::Offline)]),
            &[question("q1", held())],
        );
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            serde_json::json!([
                { "condition": "not_set_up" },
                { "condition": "halted", "folder": "/home/ada/Work", "problem": "the folder is gone" },
                { "condition": "offline", "folder": "/home/ada/Photos" },
                { "condition": "decision_required", "decision_id": "q1", "space_id": "space-Work" },
            ])
        );
        assert_eq!(check_exit(&found), 4);
    }
}
