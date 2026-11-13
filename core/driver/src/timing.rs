// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use koidrive_engine::effects::{Effect, EffectRecord, Location};
use koidrive_engine::tree::{RemoteNode, same};
use koidrive_proto::v1::{
    ChangeRecord, Commit, CommitRecord, CommitResult, change_record, op, op_result,
};
use koidrive_telemetry::{
    Recorder,
    schema::{FileCount, Measure},
};

use crate::status::FolderState;

pub(crate) type Bytes = Option<Arc<AtomicU64>>;

pub(crate) fn add_bytes(bytes: &Bytes, count: u64) {
    if let Some(bytes) = bytes {
        let _ = bytes.try_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
            Some(held.saturating_add(count))
        });
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Time {
    pub mono: Instant,
    pub unix_ms: i64,
}

impl Time {
    pub fn now() -> Self {
        Self {
            mono: Instant::now(),
            unix_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .and_then(|time| time.as_millis().try_into().ok())
                .unwrap_or(0),
        }
    }

    fn day(self) -> u32 {
        u32::try_from(self.unix_ms.max(0) / 86_400_000).unwrap_or(u32::MAX)
    }
}

pub(crate) struct Timing {
    recorder: Recorder,
    state: Option<Box<State>>,
    first: Option<(PathBuf, i64)>,
}

#[derive(Default)]
struct State {
    syncing: Option<(Instant, Instant)>,
    download: Burst,
    upload: Burst,
    origins: BTreeMap<String, Origin>,
    effects: BTreeMap<String, Planned>,
}

#[derive(Default)]
struct Burst {
    start: Option<Instant>,
    files: BTreeSet<(String, String)>,
    bytes: Arc<AtomicU64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Origin {
    seq: u64,
    revision: Option<(u64, u64, bool)>,
    committed_ms: i64,
}

struct Planned {
    node: String,
    wrote_file: bool,
    final_target: bool,
    origin: Option<Origin>,
    version: String,
}

impl Timing {
    pub fn initialize(store: &Path, linked_ms: i64) {
        let path = store.with_file_name("first-sync");
        let _ = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(&path)?;
            write!(file, "{linked_ms}")?;
            file.sync_all()?;
            File::open(path.parent().unwrap())?.sync_all()
        })();
    }

    pub fn open(store: &Path, linked_ms: i64) -> Self {
        let path = store.with_file_name("first-sync");
        let eligible = fs::read_to_string(&path)
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            == Some(linked_ms);
        Self {
            recorder: Recorder::off(),
            state: None,
            first: eligible.then_some((path, linked_ms)),
        }
    }

    pub fn enable(&mut self, recorder: Recorder) {
        self.state = recorder.is_on().then(Box::default);
        self.recorder = recorder;
    }

    pub fn is_on(&self) -> bool {
        self.recorder.is_on()
    }

    pub fn has_first(&self) -> bool {
        self.first.is_some()
    }

    pub fn first_pending(&self) -> bool {
        self.first.is_some() && self.recorder.is_on()
    }

    pub fn first_sync(&mut self, files: u64, at: Time) {
        let Some((path, linked_ms)) = self.first.take() else {
            return;
        };
        if fs::remove_file(&path).is_err() {
            return;
        }
        if File::open(path.parent().unwrap())
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            return;
        }
        if linked_ms > 0 && at.unix_ms >= linked_ms {
            self.recorder.measure_on(
                at.day(),
                Measure::FirstSync(FileCount::bucket(files)),
                (at.unix_ms - linked_ms) as u64,
            );
        }
    }

    pub fn status(&mut self, status: FolderState, at: Time) {
        let Some(state) = &mut self.state else { return };
        let syncing = matches!(status, FolderState::Bootstrapping | FolderState::CatchingUp);
        if syncing {
            let (start, sample) = state.syncing.get_or_insert((at.mono, at.mono));
            if at.mono.saturating_duration_since(*sample) >= Duration::from_secs(60) {
                self.recorder.measure_on(
                    at.day(),
                    Measure::ActiveSyncingMilliseconds,
                    millis(at.mono.saturating_duration_since(*start)),
                );
                *sample = at.mono;
            }
        } else if let Some((start, _)) = state.syncing.take() {
            self.recorder.measure_on(
                at.day(),
                Measure::SyncingMilliseconds,
                millis(at.mono.saturating_duration_since(start)),
            );
        }
    }

    pub fn drained(&mut self, at: Time) {
        let Some(state) = &mut self.state else { return };
        state.download.finish(
            &self.recorder,
            at,
            Measure::DownloadFilesPerSecond,
            Measure::DownloadBytesPerSecond,
        );
        state.upload.finish(
            &self.recorder,
            at,
            Measure::UploadFilesPerSecond,
            Measure::UploadBytesPerSecond,
        );
        state.effects.clear();
    }

    pub fn records(
        &mut self,
        records: &[CommitRecord],
        before: u64,
        through: u64,
        device: &str,
        at: Time,
    ) {
        if self.state.is_none() {
            return;
        }
        let mut last = before;
        for record in records {
            if record.seq <= last {
                continue;
            }
            if record.seq > through || record.seq != last + 1 {
                break;
            }
            self.changes(
                &record.changes,
                record.seq,
                if record.device_id == device {
                    0
                } else {
                    record.committed_at_ms
                },
                at,
            );
            last = record.seq;
        }
    }

    pub fn changes(&mut self, changes: &[ChangeRecord], seq: u64, committed_ms: i64, at: Time) {
        let Some(state) = &mut self.state else { return };
        for change in changes {
            let (node, revision) = match &change.change {
                Some(change_record::Change::Node(node)) => (
                    &node.node_id,
                    Some((
                        node.content_rev,
                        node.meta_rev,
                        node.status == koidrive_proto::v1::NodeStatus::Trashed as i32,
                    )),
                ),
                Some(change_record::Change::Purged(node)) => (&node.node_id, None),
                _ => continue,
            };
            state.origins.remove(node);
            if committed_ms > 0 && committed_ms <= at.unix_ms {
                state.origins.insert(
                    node.clone(),
                    Origin {
                        seq,
                        revision,
                        committed_ms,
                    },
                );
            }
        }
    }

    pub fn clear_origins(&mut self) {
        if let Some(state) = &mut self.state {
            state.origins.clear();
        }
    }

    pub fn forget(&mut self, changes: &[ChangeRecord]) {
        let Some(state) = &mut self.state else { return };
        for change in changes {
            match &change.change {
                Some(change_record::Change::Node(node)) => {
                    state.origins.remove(&node.node_id);
                }
                Some(change_record::Change::Purged(node)) => {
                    state.origins.remove(&node.node_id);
                }
                _ => {}
            }
        }
    }

    pub fn planned(
        &mut self,
        record: &EffectRecord,
        remote: Option<&RemoteNode>,
        at: Time,
    ) -> Bytes {
        let Some(state) = &mut self.state else {
            return None;
        };
        let revision = remote.map(|node| (node.content_rev, node.meta_rev, node.trashed));
        let origin = state
            .origins
            .get(&record.node)
            .filter(|origin| origin.revision == revision)
            .cloned();
        state.effects.insert(
            record.effect_id.clone(),
            Planned {
                node: record.node.clone(),
                wrote_file: matches!(record.effect, Effect::WriteFile { .. }),
                final_target: !matches!(
                    record.effect,
                    Effect::Move {
                        to: Location::Parked(_),
                        ..
                    }
                ) && same(
                    record.after.as_ref(),
                    remote.filter(|node| !node.trashed).map(|node| &node.shape),
                ),
                origin,
                version: remote
                    .map_or_else(|| record.effect_id.clone(), |node| node.version_id.clone()),
            },
        );
        if matches!(record.effect, Effect::WriteFile { .. }) {
            state.download.start.get_or_insert(at.mono);
            Some(state.download.bytes.clone())
        } else {
            None
        }
    }

    pub fn applied(&mut self, effect_id: &str, success: bool, at: Time) {
        let Some(state) = &mut self.state else { return };
        let Some(planned) = state.effects.remove(effect_id) else {
            return;
        };
        if !success {
            return;
        }
        if let Some(origin) = planned.origin
            && state.origins.get(&planned.node) == Some(&origin)
            && at.unix_ms >= origin.committed_ms
        {
            self.recorder.measure_on(
                at.day(),
                Measure::RemoteApplyMilliseconds,
                (at.unix_ms - origin.committed_ms) as u64,
            );
            if planned.final_target {
                state.origins.remove(&planned.node);
            }
        }
        if planned.wrote_file {
            state.download.files.insert((planned.node, planned.version));
        }
    }

    pub fn uploading(&mut self, commit: &Commit, at: Time) -> Bytes {
        let state = self.state.as_mut()?;
        if commit.ops.iter().any(|op| {
            matches!(
                op.op,
                Some(op::Op::CreateFile(_) | op::Op::UpdateContent(_))
            )
        }) {
            state.upload.start.get_or_insert(at.mono);
        }
        Some(state.upload.bytes.clone())
    }

    pub fn uploaded(&mut self, commit: &Commit, result: &CommitResult) {
        let Some(state) = &mut self.state else { return };
        for (index, (op, result)) in commit.ops.iter().zip(&result.results).enumerate() {
            if matches!(
                op.op,
                Some(op::Op::CreateFile(_) | op::Op::UpdateContent(_))
            ) && matches!(
                op_result::Code::try_from(result.code),
                Ok(op_result::Code::Applied
                    | op_result::Code::AppliedRenamed
                    | op_result::Code::ConflictCopy)
            ) {
                state
                    .upload
                    .files
                    .insert((commit.commit_id.clone(), index.to_string()));
            }
        }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        if self.is_on() {
            self.status(FolderState::Stopped, Time::now());
        }
    }
}

impl Burst {
    fn finish(&mut self, recorder: &Recorder, at: Time, files: Measure, bytes: Measure) {
        if let Some(start) = self.start.take() {
            let elapsed = at.mono.saturating_duration_since(start).as_nanos();
            let transferred = self.bytes.swap(0, Ordering::Relaxed);
            if !self.files.is_empty() && elapsed > 0 {
                recorder.measure_on(at.day(), files, rate(self.files.len() as u64, elapsed));
                recorder.measure_on(at.day(), bytes, rate(transferred, elapsed));
            }
            self.files.clear();
        }
    }
}

fn rate(count: u64, nanos: u128) -> u64 {
    u64::try_from(u128::from(count) * 1_000_000_000 / nanos.max(1)).unwrap_or(u64::MAX)
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use koidrive_engine::tree::{Kind, Shape};
    use koidrive_proto::v1::{Content, NodeState, NodeStatus, Op, OpResult};
    use serde_json::{Value, json};

    fn time() -> Time {
        Time {
            mono: Instant::now(),
            unix_ms: 2 * 86_400_000 - 30_000,
        }
    }

    fn later(at: Time, duration: Duration) -> Time {
        Time {
            mono: at.mono + duration,
            unix_ms: at.unix_ms + duration.as_millis() as i64,
        }
    }

    fn tracker(recorder: &Recorder) -> Timing {
        let mut timing = Timing {
            recorder: Recorder::off(),
            state: None,
            first: None,
        };
        timing.enable(recorder.clone());
        timing
    }

    fn reports(recorder: &Recorder) -> Value {
        serde_json::to_value(recorder.take().days).unwrap()
    }

    fn node(revision: u64) -> RemoteNode {
        RemoteNode {
            shape: Shape {
                parent: "root".into(),
                name: "secret-name".into(),
                kind: Kind::File,
                content: Some(Content::default()),
            },
            trashed: false,
            content_rev: revision,
            meta_rev: 1,
            trash_group: String::new(),
            version_id: format!("version-{revision}"),
        }
    }

    fn effect(id: &str) -> EffectRecord {
        let remote = node(1);
        EffectRecord {
            effect_id: id.into(),
            node: "secret-node-id".into(),
            counter: 1,
            effect: Effect::WriteFile {
                at: Location::Parked("secret-node-id".into()),
                content: Content::default(),
                replaces: None,
            },
            after: Some(remote.shape),
        }
    }

    fn record(seq: u64, committed_ms: i64, revision: u64) -> CommitRecord {
        CommitRecord {
            seq,
            committed_at_ms: committed_ms,
            device_id: "other-device".into(),
            changes: vec![ChangeRecord {
                change: Some(change_record::Change::Node(NodeState {
                    node_id: "secret-node-id".into(),
                    content_rev: revision,
                    meta_rev: 1,
                    status: NodeStatus::Live as i32,
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }
    }

    fn commit() -> Commit {
        Commit {
            commit_id: "private-commit".into(),
            ops: vec![Op {
                op: Some(op::Op::CreateFile(koidrive_proto::v1::CreateFile {
                    node_id: "secret-node-id".into(),
                    content: Some(Content::default()),
                    ..Default::default()
                })),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn first_sync_is_correlated_durable_once_and_legacy_links_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let recorder = Recorder::on();
        let at = time();
        let mut legacy = Timing::open(&store, 1);
        legacy.enable(recorder.clone());
        legacy.first_sync(100_000, at);
        assert!(recorder.take().days.is_empty());
        Timing::initialize(&store, at.unix_ms - 5000);
        drop(Timing::open(&store, at.unix_ms - 5000));
        let mut timing = Timing::open(&store, at.unix_ms - 5000);
        timing.enable(recorder.clone());
        timing.first_sync(100_000, at);
        timing.first_sync(400_000, at);
        drop(timing);
        let mut reopened = Timing::open(&store, at.unix_ms - 5000);
        reopened.enable(recorder.clone());
        reopened.first_sync(400_000, later(at, Duration::from_secs(100)));
        let value = reports(&recorder);
        assert_eq!(
            value["1"]["histograms"]["first_sync_ms"]["65536_131071"]["sum"],
            5000
        );
        assert_eq!(
            value["1"]["histograms"]["first_sync_ms"]
                .as_object()
                .unwrap()
                .len(),
            1
        );
        Timing::initialize(&store, at.unix_ms);
        let mut relinked = Timing::open(&store, at.unix_ms);
        relinked.enable(recorder.clone());
        relinked.first_sync(400_000, later(at, Duration::from_secs(1)));
        assert_eq!(
            reports(&recorder)["1"]["histograms"]["first_sync_ms"]["262144_524287"]["count"],
            1
        );
    }

    #[test]
    fn first_sync_io_failure_mismatched_link_and_off_do_not_record() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("store.sqlite");
        let at = time();
        let recorder = Recorder::on();
        Timing::initialize(&store, at.unix_ms);
        assert!(!Timing::open(&store, at.unix_ms + 1).has_first());
        let mut timing = Timing::open(&store, at.unix_ms);
        timing.enable(recorder.clone());
        fs::remove_file(store.with_file_name("first-sync")).unwrap();
        fs::create_dir(store.with_file_name("first-sync")).unwrap();
        timing.first_sync(1, at);
        assert!(recorder.take().days.is_empty());
        fs::remove_dir(store.with_file_name("first-sync")).unwrap();
        Timing::initialize(&store, at.unix_ms);
        let mut off = Timing::open(&store, at.unix_ms);
        off.first_sync(1, at);
        assert!(!Timing::open(&store, at.unix_ms).has_first());
        assert!(off.state.is_none());
    }

    #[test]
    fn syncing_spans_sample_active_age_and_finish_on_real_transitions_across_days() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        timing.status(FolderState::Connecting, at);
        timing.status(FolderState::Bootstrapping, at);
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(10)));
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(60)));
        let value = reports(&recorder);
        assert_eq!(
            value["2"]["histograms"]["syncing_ms"]["active_age"]["sum"],
            60_000
        );
        assert!(value["2"]["histograms"]["syncing_ms"]["completed"].is_null());
        timing.status(FolderState::Offline, later(at, Duration::from_secs(90)));
        timing.status(FolderState::Halted, later(at, Duration::from_secs(100)));
        timing.status(FolderState::Stopped, later(at, Duration::from_secs(110)));
        assert_eq!(
            reports(&recorder)["2"]["histograms"]["syncing_ms"]["completed"]["sum"],
            90_000
        );
        let off = Recorder::off();
        let mut timing = tracker(&off);
        timing.status(FolderState::CatchingUp, at);
        timing.status(FolderState::CatchingUp, later(at, Duration::from_secs(600)));
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(900)));
        assert!(timing.state.is_none());
        assert!(off.take().days.is_empty());
    }

    #[test]
    fn bursts_include_retries_empty_files_dedup_and_subsecond_denominators() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let record = effect("write");
        let bytes = timing.planned(&record, Some(&node(1)), at);
        add_bytes(&bytes, 17);
        timing.applied(
            &record.effect_id,
            false,
            later(at, Duration::from_millis(20)),
        );
        let bytes = timing.planned(
            &record,
            Some(&node(1)),
            later(at, Duration::from_millis(50)),
        );
        add_bytes(&bytes, 17);
        timing.applied(
            &record.effect_id,
            true,
            later(at, Duration::from_millis(70)),
        );
        timing.planned(
            &record,
            Some(&node(1)),
            later(at, Duration::from_millis(80)),
        );
        timing.applied(
            &record.effect_id,
            true,
            later(at, Duration::from_millis(90)),
        );
        let mut upload = commit();
        upload.ops.push(Op {
            op: Some(op::Op::UpdateContent(koidrive_proto::v1::UpdateContent {
                node_id: "deduplicated-file".into(),
                content: Some(Content {
                    size: 4096,
                    ..Default::default()
                }),
            })),
        });
        timing.uploading(&upload, at);
        let accepted = CommitResult {
            results: vec![
                OpResult {
                    code: op_result::Code::Applied as i32,
                    ..Default::default()
                };
                2
            ],
            ..Default::default()
        };
        timing.uploaded(&upload, &accepted);
        timing.uploaded(&upload, &accepted);
        timing.status(FolderState::UpToDate, later(at, Duration::from_millis(100)));
        timing.drained(later(at, Duration::from_millis(100)));
        let value = reports(&recorder);
        let histograms = &value["1"]["histograms"];
        assert_eq!(histograms["download_per_second"]["files"]["sum"], 10);
        assert_eq!(histograms["download_per_second"]["bytes"]["sum"], 340);
        assert_eq!(histograms["upload_per_second"]["files"]["sum"], 20);
        assert_eq!(histograms["upload_per_second"]["bytes"]["sum"], 0);
        assert_eq!(rate(u64::MAX, 1), u64::MAX);
        assert_eq!(rate(1, 1_500_000_000), 0);
    }

    #[test]
    fn unsuccessful_and_zero_duration_bursts_do_not_invent_throughput() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let record = effect("failed");
        let bytes = timing.planned(&record, Some(&node(1)), at);
        add_bytes(&bytes, 99);
        timing.applied(&record.effect_id, false, at);
        let upload = commit();
        add_bytes(&timing.uploading(&upload, at), 99);
        timing.uploaded(
            &upload,
            &CommitResult {
                results: vec![OpResult {
                    code: op_result::Code::Rejected as i32,
                    ..Default::default()
                }],
                ..Default::default()
            },
        );
        timing.status(FolderState::UpToDate, later(at, Duration::from_secs(1)));
        timing.drained(later(at, Duration::from_secs(1)));
        assert!(recorder.take().days.is_empty());
        timing.planned(&record, Some(&node(1)), at);
        timing.applied(&record.effect_id, true, at);
        timing.status(FolderState::UpToDate, at);
        timing.drained(at);
        assert!(recorder.take().days.is_empty());
    }

    #[test]
    fn apply_latency_is_emitted_only_on_success_with_current_known_revision() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        let remote = node(1);
        let record = effect("one");
        timing.records(&[self::record(1, at.unix_ms - 1000, 1)], 0, 1, "me", at);
        timing.planned(&record, Some(&remote), at);
        assert!(recorder.take().days.is_empty());
        timing.applied(&record.effect_id, false, later(at, Duration::from_secs(1)));
        assert!(recorder.take().days.is_empty());
        timing.planned(&record, Some(&remote), at);
        timing.applied(&record.effect_id, true, later(at, Duration::from_secs(2)));
        let value = reports(&recorder);
        assert_eq!(
            value["1"]["histograms"]["remote_apply_ms"]["effect"]["sum"],
            3000
        );
        timing.applied(&record.effect_id, true, later(at, Duration::from_secs(3)));
        assert!(recorder.take().days.is_empty());
        timing.planned(&record, Some(&remote), at);
        timing.records(&[self::record(2, at.unix_ms - 500, 2)], 1, 2, "me", at);
        timing.applied(&record.effect_id, true, at);
        let value = reports(&recorder);
        assert!(!value.to_string().contains("remote_apply_ms"));
    }

    #[test]
    fn old_gap_future_missing_and_snapshot_origins_are_omitted() {
        let recorder = Recorder::on();
        let at = time();
        for (records, before, through, revision) in [
            (vec![record(1, at.unix_ms - 1, 1)], 1, 1, 1),
            (vec![record(2, at.unix_ms - 1, 1)], 0, 0, 1),
            (vec![record(1, 0, 1)], 0, 1, 1),
            (vec![record(1, at.unix_ms + 1, 1)], 0, 1, 1),
            (vec![record(1, at.unix_ms - 1, 1)], 0, 1, 2),
            (vec![], 0, 0, 1),
        ] {
            let mut timing = tracker(&recorder);
            timing.records(&records, before, through, "me", at);
            let effect = effect("omitted");
            timing.planned(&effect, Some(&node(revision)), at);
            timing.applied(&effect.effect_id, true, at);
        }
        assert!(recorder.take().days.is_empty());
        let mut timing = tracker(&recorder);
        timing.records(
            &[
                record(1, at.unix_ms - 20, 1),
                record(2, at.unix_ms - 10, 2),
                record(1, at.unix_ms - 20, 1),
            ],
            0,
            2,
            "me",
            at,
        );
        let effect = effect("newest");
        timing.planned(&effect, Some(&node(2)), at);
        timing.applied(&effect.effect_id, true, at);
        assert_eq!(
            reports(&recorder)["1"]["histograms"]["remote_apply_ms"]["effect"]["sum"],
            10
        );
    }

    #[test]
    #[ignore]
    fn timing_tracker_release_measurement() {
        for run in 0..5 {
            for on in if run % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let recorder = if on { Recorder::on() } else { Recorder::off() };
                let mut timing = tracker(&recorder);
                let effect = effect("measured");
                let remote = node(1);
                let records = [record(1, Time::now().unix_ms - 10, 1)];
                let commit = commit();
                let accepted = CommitResult {
                    results: vec![OpResult {
                        code: op_result::Code::Applied as i32,
                        ..Default::default()
                    }],
                    ..Default::default()
                };
                let started = Instant::now();
                for _ in 0..1000 {
                    if std::hint::black_box(&timing).is_on() {
                        timing.status(FolderState::CatchingUp, Time::now());
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.records(&records, 0, 1, "me", Time::now());
                    }
                    if std::hint::black_box(&timing).is_on() {
                        add_bytes(&timing.planned(&effect, Some(&remote), Time::now()), 1024);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        add_bytes(&timing.uploading(&commit, Time::now()), 2048);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.applied(&effect.effect_id, true, Time::now());
                        timing.uploaded(&commit, &accepted);
                    }
                    if std::hint::black_box(&timing).is_on() {
                        timing.status(FolderState::UpToDate, Time::now());
                        timing.drained(Time::now());
                    }
                }
                let elapsed = started.elapsed();
                let value = reports(&recorder);
                if on {
                    for (metric, label) in [
                        ("download_per_second", "files"),
                        ("download_per_second", "bytes"),
                        ("upload_per_second", "files"),
                        ("upload_per_second", "bytes"),
                        ("remote_apply_ms", "effect"),
                        ("syncing_ms", "completed"),
                    ] {
                        let count: u64 = value
                            .as_object()
                            .unwrap()
                            .values()
                            .map(|day| {
                                day["histograms"][metric][label]["count"]
                                    .as_u64()
                                    .unwrap_or(0)
                            })
                            .sum();
                        assert_eq!(count, 1000, "{metric}.{label}");
                    }
                } else {
                    assert_eq!(value, json!({}));
                }
                eprintln!(
                    "timing tracker run={run} on={on} bursts=1000 elapsed_ns={}",
                    elapsed.as_nanos()
                );
            }
        }
    }

    #[test]
    fn blocked_retries_keep_origins_and_bursts_until_the_direction_drains() {
        let recorder = Recorder::on();
        let mut timing = tracker(&recorder);
        let at = time();
        timing.records(&[record(1, at.unix_ms - 1000, 1)], 0, 1, "me", at);
        let mut parked = effect("park");
        parked.effect = Effect::Move {
            from: Location::Parked("before".into()),
            identity: koidrive_engine::tree::FileIdentity {
                dev: 1,
                ino: 1,
                birth_ns: None,
            },
            to: Location::Parked("secret-node-id".into()),
        };
        timing.planned(&parked, Some(&node(1)), at);
        timing.applied(
            &parked.effect_id,
            true,
            later(at, Duration::from_millis(100)),
        );
        assert_eq!(
            reports(&recorder)["1"]["histograms"]["remote_apply_ms"]["effect"]["sum"],
            1100
        );
        let write = effect("write");
        add_bytes(&timing.planned(&write, Some(&node(1)), at), 13);
        timing.applied(
            &write.effect_id,
            false,
            later(at, Duration::from_millis(200)),
        );
        timing.status(FolderState::UpToDate, later(at, Duration::from_millis(400)));
        assert!(recorder.take().days.is_empty());
        add_bytes(
            &timing.planned(
                &write,
                Some(&node(1)),
                later(at, Duration::from_millis(600)),
            ),
            13,
        );
        timing.applied(&write.effect_id, true, later(at, Duration::from_secs(1)));
        timing.drained(later(at, Duration::from_secs(1)));
        let value = reports(&recorder);
        assert_eq!(
            value["1"]["histograms"]["remote_apply_ms"]["effect"]["sum"],
            2000
        );
        assert_eq!(
            value["1"]["histograms"]["download_per_second"]["files"]["sum"],
            1
        );
        assert_eq!(
            value["1"]["histograms"]["download_per_second"]["bytes"]["sum"],
            26
        );
    }
}
