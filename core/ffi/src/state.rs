// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use koidrive_driver::{self as driver, FolderState as DriverFolderState, Status};

use crate::helper::Helper;
use crate::types::{
    Account, Answer, FolderPath, FolderState, HeldItem, ItemDetail, LinkedFolder, MassDeleteLimit,
    NodeKind, Notice, NoticeKind, Problem, ProblemKind, Question, QuestionKind, RecoveryCause,
    RecoveryGroup, RecoveryTrigger, SessionState, SyncProgress, SyncStatus,
};

pub(crate) type Halted = BTreeMap<String, (String, i64)>;

#[derive(Default)]
pub(crate) struct Carried {
    seen: BTreeMap<String, (u64, u64)>,
    done: u64,
}

pub(crate) fn session_state(
    status: &Status,
    questions: &[driver::Question],
    started: bool,
    halted: &mut Halted,
    carried: &mut Carried,
    helper: &Helper,
) -> SessionState {
    let shown = match started {
        true => sync_status(status),
        false => SyncStatus::Stopped,
    };
    let progress = progress(status, carried).filter(|_| shown == SyncStatus::Syncing);
    if progress.is_none() {
        *carried = Carried::default();
    }
    SessionState {
        status: shown,
        progress,
        account: status.account.as_ref().map(account),
        folders: status.folders.iter().map(folder).collect(),
        recent_changes: Vec::new(),
        questions: questions.iter().cloned().filter_map(question).collect(),
        problems: {
            let mut problems = problems(status, halted);
            problems.extend(questions.iter().filter_map(|asked| conflict(asked, helper)));
            problems.sort_by_key(|problem| problem.since_ms);
            problems
        },
        notices: questions.iter().cloned().filter_map(notice).collect(),
    }
}

fn sync_status(status: &Status) -> SyncStatus {
    if status.paused {
        return SyncStatus::Paused;
    }
    if status.account.is_none() || status.folders.is_empty() {
        return SyncStatus::NeedsSetup;
    }
    if !status.running {
        return SyncStatus::Stopped;
    }
    let any = |state| status.folders.iter().any(|folder| folder.state == state);
    if any(DriverFolderState::Halted) {
        SyncStatus::Halted
    } else if any(DriverFolderState::Bootstrapping) || any(DriverFolderState::CatchingUp) {
        SyncStatus::Syncing
    } else if any(DriverFolderState::Connecting) || any(DriverFolderState::Stopped) {
        SyncStatus::Connecting
    } else if any(DriverFolderState::Offline) {
        SyncStatus::Offline
    } else {
        SyncStatus::Idle
    }
}

fn progress(status: &Status, carried: &mut Carried) -> Option<SyncProgress> {
    let mut sum = SyncProgress {
        files_to_upload: 0,
        files_to_download: 0,
        bytes_done: 0,
        bytes_total: 0,
        current: None,
    };
    let mut seen = BTreeMap::new();
    for folder in &status.folders {
        let before = carried.seen.remove(&folder.space_id);
        let Some(progress) = &folder.progress else {
            let Some((done, total)) = before else {
                continue;
            };
            match folder.state {
                DriverFolderState::Offline | DriverFolderState::Connecting => {
                    seen.insert(folder.space_id.clone(), (done, total));
                    sum.bytes_done += done;
                    sum.bytes_total += total;
                }
                DriverFolderState::Bootstrapping
                | DriverFolderState::CatchingUp
                | DriverFolderState::UpToDate => carried.done += total,
                DriverFolderState::Stopped | DriverFolderState::Halted => carried.done += done,
            }
            continue;
        };
        if let Some((done, _)) = before
            && progress.bytes_done < done
        {
            carried.done += done;
        }
        seen.insert(
            folder.space_id.clone(),
            (progress.bytes_done, progress.bytes_total),
        );
        sum.files_to_upload += progress.files_to_upload;
        sum.files_to_download += progress.files_to_download;
        sum.bytes_done += progress.bytes_done;
        sum.bytes_total += progress.bytes_total;
        if sum.current.is_none() {
            sum.current = (progress.current.clone()).map(|path| FolderPath {
                space_id: folder.space_id.clone(),
                path,
            });
        }
    }
    carried.done += carried.seen.values().map(|(done, _)| done).sum::<u64>();
    carried.seen = seen;
    if carried.seen.is_empty() {
        return None;
    }
    sum.bytes_done += carried.done;
    sum.bytes_total += carried.done;
    Some(sum)
}

fn account(account: &driver::Account) -> Account {
    Account {
        server_url: account.server.trim_end_matches('/').to_owned(),
        user_id: account.user_id.clone(),
        user_name: account.user_name.clone().unwrap_or_default(),
        device_id: account.device_id.clone(),
        device_name: account.device_name.clone(),
    }
}

pub(crate) fn folder(folder: &driver::FolderStatus) -> LinkedFolder {
    LinkedFolder {
        space_id: folder.space_id.clone(),
        space_name: folder.space_name.clone(),
        path: folder.path.clone(),
        state: match folder.state {
            DriverFolderState::Stopped
            | DriverFolderState::Connecting
            | DriverFolderState::Offline => FolderState::Waiting,
            DriverFolderState::Bootstrapping | DriverFolderState::CatchingUp => {
                FolderState::Syncing
            }
            DriverFolderState::UpToDate => FolderState::UpToDate,
            DriverFolderState::Halted => FolderState::Halted,
        },
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

fn problems(status: &Status, halted: &mut Halted) -> Vec<Problem> {
    let now = now_ms();
    let current: Halted = status
        .folders
        .iter()
        .filter(|folder| folder.state == DriverFolderState::Halted)
        .map(|folder| {
            let reason = folder.problem.clone().unwrap_or_default();
            let since = match halted.get(&folder.space_id) {
                Some((seen, since)) if *seen == reason => *since,
                _ => now,
            };
            (folder.space_id.clone(), (reason, since))
        })
        .collect();
    *halted = current;
    let mut problems: Vec<Problem> = halted
        .iter()
        .map(|(space_id, (reason, since))| Problem {
            id: format!("halted-{space_id}"),
            space_id: Some(space_id.clone()),
            path: None,
            kind: ProblemKind::FolderHalted {
                reason: reason.clone(),
            },
            since_ms: *since,
        })
        .collect();
    problems.sort_by_key(|problem| problem.since_ms);
    problems
}

fn mass_delete_limit(limit: driver::Limit) -> MassDeleteLimit {
    match limit {
        driver::Limit::Deletions => MassDeleteLimit::Deletions {
            limit: driver::MAX_DELETIONS,
        },
        driver::Limit::Share => MassDeleteLimit::ShareOfSpace { percent: 50 },
    }
}

fn conflict(asked: &driver::Question, helper: &Helper) -> Option<Problem> {
    let driver::QuestionKind::Conflict { original, .. } = &asked.kind else {
        return None;
    };
    Some(Problem {
        id: asked.id.clone(),
        space_id: Some(asked.space_id.clone()),
        path: asked
            .items
            .iter()
            .find(|item| item.node_id == original.node_id)
            .map(|item| item.path.clone()),
        kind: ProblemKind::Conflict {
            resolve_command: helper.action_command(&["resolve"]),
        },
        since_ms: asked.asked_at_ms,
    })
}

fn notice(asked: driver::Question) -> Option<Notice> {
    let driver::QuestionKind::RemoteMassDelete {
        deletions,
        affected_files,
        space_files,
        exceeded,
    } = asked.kind
    else {
        return None;
    };
    Some(Notice {
        id: asked.id,
        space_id: asked.space_id,
        at_ms: asked.asked_at_ms,
        kind: NoticeKind::RemoteMassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded: exceeded.into_iter().map(mass_delete_limit).collect(),
        },
    })
}

fn question(question: driver::Question) -> Option<Question> {
    let kind = match question.kind {
        driver::QuestionKind::RemoteMassDelete { .. } | driver::QuestionKind::Conflict { .. } => {
            return None;
        }
        driver::QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded: exceeded.into_iter().map(mass_delete_limit).collect(),
        },
        driver::QuestionKind::AmbiguousDisappearance => QuestionKind::AmbiguousDisappearance,
        driver::QuestionKind::Recovery { group, .. } => QuestionKind::Recovery {
            cause: recovery_cause(question.recovery_cause),
            group: match group {
                driver::RecoveryGroup::CreatedAfterRestorePoint => {
                    RecoveryGroup::CreatedAfterRestorePoint
                }
                driver::RecoveryGroup::ChangedOnThisDevice => RecoveryGroup::ChangedOnThisDevice,
                driver::RecoveryGroup::CreatedOnThisDevice => RecoveryGroup::CreatedOnThisDevice,
                driver::RecoveryGroup::DeletedOnThisDevice => RecoveryGroup::DeletedOnThisDevice,
                driver::RecoveryGroup::RequestedBeforeReset => RecoveryGroup::RequestedBeforeReset,
            },
        },
    };
    Some(Question {
        id: question.id,
        space_id: question.space_id,
        asked_at_ms: question.asked_at_ms,
        answers: kind.answers(),
        kind,
        items: question
            .items
            .into_iter()
            .map(|item| HeldItem {
                node_id: item.node_id,
                path: item.path,
                kind: match item.files {
                    1 => NodeKind::File,
                    _ => NodeKind::Folder,
                },
                files: item.files,
                detail: ItemDetail::Plain,
            })
            .collect(),
    })
}

fn recovery_cause(cause: Option<driver::RecoveryCause>) -> RecoveryCause {
    let Some(cause) = cause else {
        return RecoveryCause {
            trigger: RecoveryTrigger::Unknown,
            restore_point_ms: None,
        };
    };
    let trigger = match cause.trigger {
        driver::RecoveryTrigger::SpaceRewound => RecoveryTrigger::SpaceRewound,
        driver::RecoveryTrigger::StoreRolledBack => RecoveryTrigger::StoreRolledBack,
        driver::RecoveryTrigger::StoreOlderThanDisk => RecoveryTrigger::StoreOlderThanDisk,
        driver::RecoveryTrigger::SpaceImported => RecoveryTrigger::SpaceImported,
        driver::RecoveryTrigger::NoSyncState => RecoveryTrigger::NoSyncState,
        driver::RecoveryTrigger::Unknown => RecoveryTrigger::Unknown,
    };
    RecoveryCause {
        trigger,
        restore_point_ms: match trigger {
            RecoveryTrigger::Unknown => None,
            _ => cause.restore_point_ms,
        },
    }
}

pub(crate) fn word(answer: Answer) -> &'static str {
    match answer {
        Answer::Delete => driver::Answer::Delete,
        Answer::Restore => driver::Answer::Restore,
        Answer::Keep => driver::Answer::Keep,
        Answer::Discard => driver::Answer::Discard,
        Answer::Publish => driver::Answer::Publish,
        Answer::Repeat => driver::Answer::Repeat,
        Answer::Cancel => driver::Answer::Cancel,
    }
    .as_str()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;

    fn helper() -> Helper {
        Helper {
            path: "/Applications/Koidrive.app/Contents/Helpers/koi".into(),
            env: Vec::new(),
        }
    }

    fn recovery_payload(cause: Option<serde_json::Value>) -> driver::Question {
        let mut payload = serde_json::json!({
            "id": "recovery", "space_id": "s1", "asked_at_ms": 8,
            "kind": { "type": "recovery", "group": "created_after_restore_point" },
            "items": []
        });
        if let Some(cause) = cause {
            payload["recovery_cause"] = cause;
        }
        let response: koidrive_ipc::Response = serde_json::from_value(serde_json::json!({
            "response": "questions", "questions": [payload]
        }))
        .unwrap();
        let koidrive_ipc::Response::Questions { mut questions } = response else {
            unreachable!();
        };
        questions.remove(0)
    }

    #[test]
    fn recovery_explanations_use_only_the_cause_the_daemon_sent() {
        for (wire, expected) in [
            ("space_rewound", RecoveryTrigger::SpaceRewound),
            ("store_rolled_back", RecoveryTrigger::StoreRolledBack),
            ("store_older_than_disk", RecoveryTrigger::StoreOlderThanDisk),
            ("space_imported", RecoveryTrigger::SpaceImported),
            ("no_sync_state", RecoveryTrigger::NoSyncState),
        ] {
            let asked = recovery_payload(Some(serde_json::json!({
                "trigger": wire, "restore_point_ms": 1_794_442_424_000_i64
            })));
            let shown = question(asked).unwrap();
            assert_eq!(
                shown.kind,
                QuestionKind::Recovery {
                    cause: RecoveryCause {
                        trigger: expected,
                        restore_point_ms: Some(1_794_442_424_000),
                    },
                    group: RecoveryGroup::CreatedAfterRestorePoint,
                }
            );
            assert_eq!(shown.answers, [Answer::Keep, Answer::Discard]);
        }
        for cause in [
            None,
            Some(serde_json::json!({})),
            Some(serde_json::json!({
                "trigger": "a_future_cause", "restore_point_ms": 42
            })),
            Some(serde_json::json!({ "trigger": "unknown", "restore_point_ms": 42 })),
        ] {
            let shown = question(recovery_payload(cause)).unwrap();
            assert_eq!(
                shown.kind,
                QuestionKind::Recovery {
                    cause: RecoveryCause {
                        trigger: RecoveryTrigger::Unknown,
                        restore_point_ms: None,
                    },
                    group: RecoveryGroup::CreatedAfterRestorePoint,
                }
            );
            assert_eq!(shown.answers, [Answer::Keep, Answer::Discard]);
        }
    }

    #[test]
    fn conflicts_are_visible_with_a_shell_safe_bundled_resolution_command() {
        let asked: driver::Question = serde_json::from_value(serde_json::json!({
            "id": "q-conflict", "space_id": "s1", "asked_at_ms": 7,
            "kind": {
                "type": "conflict",
                "original": { "node_id": "original", "revisions": { "content_rev": 1, "meta_rev": 1 } },
                "copy": { "node_id": "copy", "revisions": { "content_rev": 1, "meta_rev": 1 } }
            },
            "items": [
                { "node_id": "copy", "path": "A conflict.txt", "files": 1 },
                { "node_id": "original", "path": "notes.txt", "files": 1 }
            ]
        }))
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Ada's $(false) Koidrive koi");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' \"$#\" \"$1\" \"$HOME\" \"$XDG_CONFIG_HOME\" \"${XDG_STATE_HOME-unset}\" \"${XDG_RUNTIME_DIR-unset}\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let helper = Helper {
            path,
            env: vec![
                ("HOME", Some("/Users/Ada's home".into())),
                (
                    "XDG_CONFIG_HOME",
                    Some("/private/other $(false) profile".into()),
                ),
                ("XDG_STATE_HOME", None),
                ("XDG_RUNTIME_DIR", None),
            ],
        };
        let status = status_of(&[DriverFolderState::Halted]);
        let mut halted = Halted::new();
        let state = session_state(
            &status,
            &[asked],
            true,
            &mut halted,
            &mut Carried::default(),
            &helper,
        );
        assert!(state.questions.is_empty());
        assert_eq!(state.problems.len(), 2);
        let problem = state
            .problems
            .iter()
            .find(|p| p.id == "q-conflict")
            .unwrap();
        assert_eq!(problem.space_id.as_deref(), Some("s1"));
        assert_eq!(problem.path.as_deref(), Some("notes.txt"));
        assert_eq!(problem.since_ms, 7);
        let ProblemKind::Conflict { resolve_command } = &problem.kind else {
            panic!("a conflict must remain visible as a problem");
        };
        let parsed = std::process::Command::new("/bin/sh")
            .args(["-c", resolve_command])
            .env("HOME", "/wrong/home")
            .env("XDG_CONFIG_HOME", "/wrong/config")
            .env("XDG_STATE_HOME", "/wrong/state")
            .env("XDG_RUNTIME_DIR", "/wrong/runtime")
            .output()
            .unwrap();
        assert!(parsed.status.success());
        assert_eq!(
            String::from_utf8(parsed.stdout).unwrap(),
            "1\nresolve\n/Users/Ada's home\n/private/other $(false) profile\nunset\nunset\n"
        );
        let resolved = session_state(
            &status,
            &[],
            true,
            &mut halted,
            &mut Carried::default(),
            &helper,
        );
        assert_eq!(resolved.problems.len(), 1);
        assert!(matches!(
            resolved.problems[0].kind,
            ProblemKind::FolderHalted { .. }
        ));
    }

    fn folder_in(state: DriverFolderState, problem: Option<&str>) -> driver::FolderStatus {
        driver::FolderStatus {
            recovery: None,
            space_id: "0190a5b0-0000-7000-8000-000000000001".into(),
            space_name: "Documents".into(),
            path: "/Users/ada/Documents".into(),
            wait: None,
            state,
            history_id: None,
            last_seq: None,
            head_seq: None,
            remote_nodes: 0,
            local_entries: None,
            questions: 0,
            problem: problem.map(str::to_owned),
            progress: None,
        }
    }

    fn status_of(states: &[DriverFolderState]) -> Status {
        Status {
            account: Some(driver::Account {
                server: "https://files.example.com/".into(),
                user_id: "u1".into(),
                user_name: None,
                device_id: "d1".into(),
                device_name: "Mac".into(),
            }),
            running: true,
            paused: false,
            version: None,
            update: driver::Update::Unknown,
            folders: states.iter().map(|state| folder_in(*state, None)).collect(),
        }
    }

    #[test]
    fn the_session_status_follows_its_folders_and_the_daemon() {
        use DriverFolderState::*;
        let status = |status: &Status, started| {
            session_state(
                status,
                &[],
                started,
                &mut Default::default(),
                &mut Carried::default(),
                &helper(),
            )
            .status
        };
        let up = status_of(&[UpToDate]);
        assert_eq!(status(&up, false), SyncStatus::Stopped);
        let paused = Status {
            paused: true,
            ..up.clone()
        };
        assert_eq!(status(&paused, true), SyncStatus::Paused);
        assert_eq!(status(&status_of(&[]), true), SyncStatus::NeedsSetup);
        let signed_out = Status {
            account: None,
            running: false,
            ..status_of(&[Halted])
        };
        assert_eq!(status(&signed_out, true), SyncStatus::NeedsSetup);
        let no_daemon = Status {
            running: false,
            ..up.clone()
        };
        assert_eq!(status(&no_daemon, true), SyncStatus::Stopped);
        for (states, expected) in [
            (vec![UpToDate], SyncStatus::Idle),
            (vec![UpToDate, UpToDate], SyncStatus::Idle),
            (vec![Halted], SyncStatus::Halted),
            (vec![UpToDate, Halted], SyncStatus::Halted),
            (vec![CatchingUp, Halted], SyncStatus::Halted),
            (vec![Offline, Halted], SyncStatus::Halted),
            (vec![UpToDate, CatchingUp], SyncStatus::Syncing),
            (vec![Bootstrapping, Connecting], SyncStatus::Syncing),
            (vec![UpToDate, Connecting], SyncStatus::Connecting),
            (vec![Offline, UpToDate], SyncStatus::Offline),
        ] {
            assert_eq!(status(&status_of(&states), true), expected, "{states:?}");
        }
        assert_eq!(
            folder(&folder_in(Offline, None)).state,
            FolderState::Waiting
        );
        assert_eq!(
            folder(&folder_in(CatchingUp, None)).state,
            FolderState::Syncing
        );
        assert_eq!(
            account(up.account.as_ref().unwrap()).server_url,
            "https://files.example.com"
        );
    }

    #[test]
    fn a_halted_folder_is_a_problem_dated_when_its_reason_first_appeared() {
        let mut halted = Halted::new();
        let mut status = status_of(&[]);
        status.folders = vec![folder_in(
            DriverFolderState::Halted,
            Some("the marker is missing"),
        )];
        let first = problems(&status, &mut halted);
        assert_eq!(first.len(), 1);
        assert_eq!(
            first[0].kind,
            ProblemKind::FolderHalted {
                reason: "the marker is missing".into()
            }
        );
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(problems(&status, &mut halted), first);

        status.folders[0].problem = Some("the folder is missing".into());
        let changed = problems(&status, &mut halted);
        assert!(changed[0].since_ms > first[0].since_ms);

        status.folders[0].state = DriverFolderState::UpToDate;
        assert!(problems(&status, &mut halted).is_empty());
        assert!(halted.is_empty());
    }

    #[test]
    fn questions_keep_their_answers_and_items_and_a_remote_mass_delete_is_a_notice() {
        let asked = |id: &str, kind| driver::Question {
            id: id.into(),
            lineage: id.into(),
            recovery_cause: None,
            space_id: "s1".into(),
            asked_at_ms: 7,
            kind,
            items: vec![
                driver::HeldItem {
                    node_id: "n1".into(),
                    path: "Archive".into(),
                    files: 1_499,
                    device_id: None,
                },
                driver::HeldItem {
                    node_id: "n2".into(),
                    path: "notes.txt".into(),
                    files: 1,
                    device_id: None,
                },
            ],
        };
        let limits = || vec![driver::Limit::Deletions, driver::Limit::Share];
        let questions = [
            asked(
                "q1",
                driver::QuestionKind::MassDelete {
                    deletions: 1_200,
                    affected_files: 1_500,
                    space_files: 2_000,
                    exceeded: limits(),
                },
            ),
            asked(
                "q2",
                driver::QuestionKind::Recovery {
                    group: driver::RecoveryGroup::RequestedBeforeReset,
                    sent_in: None,
                    cause: None,
                },
            ),
            asked(
                "n1",
                driver::QuestionKind::RemoteMassDelete {
                    deletions: 2,
                    affected_files: 30,
                    space_files: 40,
                    exceeded: limits(),
                },
            ),
        ];
        let state = session_state(
            &status_of(&[]),
            &questions,
            true,
            &mut Halted::new(),
            &mut Carried::default(),
            &helper(),
        );
        let limits = vec![
            MassDeleteLimit::Deletions { limit: 1_000 },
            MassDeleteLimit::ShareOfSpace { percent: 50 },
        ];
        let [mass_delete, recovery] = state.questions.as_slice() else {
            panic!("two questions expected: {:?}", state.questions);
        };
        assert_eq!(
            mass_delete.kind,
            QuestionKind::MassDelete {
                deletions: 1_200,
                affected_files: 1_500,
                space_files: 2_000,
                exceeded: limits.clone(),
            }
        );
        assert_eq!(mass_delete.answers, [Answer::Delete, Answer::Restore]);
        assert_eq!(mass_delete.items[0].kind, NodeKind::Folder);
        assert_eq!(mass_delete.items[1].kind, NodeKind::File);
        assert_eq!(recovery.answers, [Answer::Repeat, Answer::Cancel]);
        assert_eq!(
            state.notices,
            [Notice {
                id: "n1".into(),
                space_id: "s1".into(),
                at_ms: 7,
                kind: NoticeKind::RemoteMassDelete {
                    deletions: 2,
                    affected_files: 30,
                    space_files: 40,
                    exceeded: limits,
                },
            }]
        );
    }

    #[test]
    fn each_answer_is_the_word_koi_resolve_takes() {
        for (answer, expected) in [
            (Answer::Delete, "delete"),
            (Answer::Restore, "restore"),
            (Answer::Keep, "keep"),
            (Answer::Discard, "discard"),
            (Answer::Publish, "publish"),
            (Answer::Repeat, "repeat"),
            (Answer::Cancel, "cancel"),
        ] {
            assert_eq!(word(answer), expected);
        }
    }

    #[test]
    fn a_folder_keeps_its_bytes_in_the_sum_through_every_change_until_no_folder_syncs() {
        use DriverFolderState::*;
        type Folder = (&'static str, DriverFolderState, Option<(u64, u64)>);
        type Step = (Vec<Folder>, Option<(u64, u64)>);
        let status = |folders: &[Folder]| {
            let mut status = status_of(&[]);
            status.folders = (folders.iter())
                .map(|&(space, state, progress)| driver::FolderStatus {
                    space_id: space.into(),
                    progress: progress.map(|(done, total)| driver::Progress {
                        files_to_upload: 1,
                        files_to_download: 0,
                        bytes_done: done,
                        bytes_total: total,
                        current: None,
                    }),
                    ..folder_in(state, None)
                })
                .collect();
            status
        };
        let syncing = |done, total| Some((done, total));
        let cases: [(&str, Vec<Step>); 10] = [
            (
                "a folder finishes while the other syncs",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(90, 100)),
                            ("b", CatchingUp, syncing(1, 100)),
                        ],
                        Some((91, 200)),
                    ),
                    (
                        vec![("a", UpToDate, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((101, 200)),
                    ),
                    (
                        vec![("a", UpToDate, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((101, 200)),
                    ),
                    (
                        vec![("a", UpToDate, None), ("b", CatchingUp, syncing(60, 100))],
                        Some((160, 200)),
                    ),
                    (vec![("a", UpToDate, None), ("b", UpToDate, None)], None),
                ],
            ),
            (
                "new work grows the total and keeps the bytes done",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(50, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((51, 110)),
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(50, 150)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((51, 160)),
                    ),
                ],
            ),
            (
                "a folder ends its files while it still catches up",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(90, 100)),
                            ("b", CatchingUp, syncing(1, 100)),
                        ],
                        Some((91, 200)),
                    ),
                    (
                        vec![("a", CatchingUp, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((101, 200)),
                    ),
                ],
            ),
            (
                "an offline folder comes back with the same count",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(40, 100)),
                            ("b", CatchingUp, syncing(1, 100)),
                        ],
                        Some((41, 200)),
                    ),
                    (
                        vec![("a", Offline, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((41, 200)),
                    ),
                    (
                        vec![("a", Connecting, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((41, 200)),
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(40, 100)),
                            ("b", CatchingUp, syncing(2, 100)),
                        ],
                        Some((42, 200)),
                    ),
                ],
            ),
            (
                "an offline folder finishes before it shows progress again",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(50, 100)),
                            ("b", CatchingUp, syncing(1, 100)),
                        ],
                        Some((51, 200)),
                    ),
                    (
                        vec![("a", Offline, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((51, 200)),
                    ),
                    (
                        vec![("a", UpToDate, None), ("b", CatchingUp, syncing(1, 100))],
                        Some((101, 200)),
                    ),
                ],
            ),
            (
                "an offline folder comes back with counts started again",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(50, 50)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((51, 60)),
                    ),
                    (
                        vec![("a", Offline, None), ("b", CatchingUp, syncing(1, 10))],
                        Some((51, 60)),
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(0, 5)),
                            ("b", CatchingUp, syncing(2, 10)),
                        ],
                        Some((52, 65)),
                    ),
                ],
            ),
            (
                "a folder's counts start again while it shows progress",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(50, 50)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((51, 60)),
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(0, 5)),
                            ("b", CatchingUp, syncing(2, 10)),
                        ],
                        Some((52, 65)),
                    ),
                ],
            ),
            (
                "a stopped folder keeps the bytes it moved",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(30, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((31, 110)),
                    ),
                    (
                        vec![("a", Stopped, None), ("b", CatchingUp, syncing(1, 10))],
                        Some((31, 40)),
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(5, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((36, 140)),
                    ),
                ],
            ),
            (
                "an unlinked folder keeps the bytes it moved",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(30, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((31, 110)),
                    ),
                    (vec![("b", CatchingUp, syncing(1, 10))], Some((31, 40))),
                ],
            ),
            (
                "the sum starts again once the session stops syncing",
                vec![
                    (
                        vec![
                            ("a", CatchingUp, syncing(30, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((31, 110)),
                    ),
                    (
                        vec![("a", Halted, None), ("b", CatchingUp, syncing(1, 10))],
                        None,
                    ),
                    (
                        vec![
                            ("a", CatchingUp, syncing(10, 100)),
                            ("b", CatchingUp, syncing(1, 10)),
                        ],
                        Some((11, 110)),
                    ),
                ],
            ),
        ];
        for (case, steps) in cases {
            let mut carried = Carried::default();
            let mut done = 0;
            for (step, (folders, expected)) in steps.into_iter().enumerate() {
                let state = session_state(
                    &status(&folders),
                    &[],
                    true,
                    &mut Default::default(),
                    &mut carried,
                    &helper(),
                );
                let shown =
                    (state.progress).map(|progress| (progress.bytes_done, progress.bytes_total));
                assert_eq!(shown, expected, "{case}, step {step}");
                if let Some((now, _)) = shown {
                    assert!(now >= done, "{case}, step {step}: done fell from {done}");
                }
                done = shown.map_or(0, |(now, _)| now);
            }
        }
    }
}
