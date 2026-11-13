// SPDX-License-Identifier: Apache-2.0

//! Carrying out the engine's disk effects in the folder the task locked (design §2.5, §2.8, §5.2 and §5.3), and
//! putting observations of that folder into L without undoing an effect.
//!
//! [`Disk::start`] runs before the observer. It binds a [`Space`] to a duplicate of the locked descriptor, reads the
//! root marker's counter *m* before anything writes it, and compares it with the store's effect counter *e*. It
//! recovers the effect in flight and acknowledges it, requires *m* = *e* afterwards, and deletes staged downloads no
//! effect owns. Then each effect takes three calls:
//!
//! 1. [`Disk::next`] plans the next effect and commits it with the ID generator's state. A [`Pending`] exists only
//!    after that commit, so the store holds every effect before the disk sees it.
//! 2. [`Pending::stage`] downloads a written file into staging, which no scan reports. It borrows neither the store
//!    nor the engine, so the task keeps handling other inputs meanwhile.
//! 3. [`Disk::apply`] is the effect section. With no await in between, it writes the effect's counter to the
//!    marker, applies the effect with the quarantine protocol and acknowledges the outcome in one store transaction.
//!
//! Observations reach L through [`Disk::observed`]. It refuses a batch scanned from another folder than the locked
//! one, and drops the changes a batch reports at or below the paths of an effect section that ran while the batch
//! was scanned or waited. Such a change can predate the effect, and applying it would undo the acknowledged L.
//! After each section, and for every path of a dropped change, the observer resets its snapshot there to L's view and
//! scans the path again ([`Sections::take_resets`]). The next batch then reports how the disk differs from L there,
//! removals and moves included, and carries the content of every file L holds there, read from the disk again.
//!
//! A [`Stop`] ends the task. The engine in memory may then be ahead of the store; the next start loads it again and
//! recovers the effect in flight. A root that is no longer at its linked path is never treated as a missing parent.
//! `core/fs` refuses every rename, `mkdir` and marker write once the path names another folder, and that refusal
//! stops the task with the flight kept. The check and the rename that follows it are not one atomic step, so another
//! process that moves the still locked root in between can still receive that one rename (design §5.2 accepts the
//! same window for parents). Nothing is ever written into a folder that replaced the root, since every write goes
//! through the locked descriptor.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use koidrive_chunk::ChunkerConfig;
use koidrive_engine::effects::{
    Base as EngineBase, Displaced as EngineDisplaced, Effect as EngineEffect, EffectRecord,
    Location as EngineLocation, location_of,
};
use koidrive_engine::ignore::is_rules_file;
use koidrive_engine::tree::{Content, Kind};
use koidrive_fs::{
    Action, Base, Contents, Dir, Displaced, Effect, Entry, EntryKind, FileId, Location, Marker,
    Outcome, Reason, Reclaimed, RootProblem, Space, Step,
};
use koidrive_net::{CallError, ChunkError, Session, StopReason, Transfer};
use koidrive_proto::v1::{ChunkRef, Commit, op};
use koidrive_store::{Link, Store, Tx};
use prost::Message;
use rustix::fs::{Mode, OFlags};

use crate::engine::{Engine, file_id, file_identity};
use crate::failures::{self, Failure};
use crate::observe::{FsObserver, Observation, Observed, Observer, Reset};
use crate::policy::{self, Disposition};
use crate::status::WaitKind;

/// How many effect sections [`Disk`] remembers for batches that span them. A batch older than all of them is dropped
/// whole, and the observer rescans the whole tree.
const SPANS: usize = 1024;
/// The wait before staging again after a transfer failed for now, when the server named none.
const STAGE_AGAIN: Duration = Duration::from_secs(30);

/// Why the task stops. The effect in flight, if any, stays in the store for the next start to recover.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Stop {
    #[error("{} no longer names the folder this task locked (it was moved, replaced or removed)", koidrive_fs::quoted(.0))]
    Withdrawn(PathBuf),
    #[error("the folder fails its root check: {0}")]
    Root(RootProblem),
    #[error("the folder's file system has no rename that refuses to replace its target")]
    NoReplace,
    #[error(
        "the root marker counts {marker} effects, more than the {upper} the store accounts for: the store is older \
         than the folder"
    )]
    StoreBehind { marker: u64, upper: u64 },
    #[error(
        "the root marker counts {marker} effects, fewer than the {cleared} the store cleared: the folder was restored \
         from an older state"
    )]
    DiskBehind { marker: u64, cleared: u64 },
    #[error("an observation was scanned from another folder than the one this task locked")]
    OtherRoot,
    #[error("effect {effect_id} cannot be carried out: {what}")]
    Record { effect_id: String, what: String },
    #[error("the folder's sync state: {0}")]
    Store(#[from] koidrive_store::Error),
    #[error("the folder: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Session(StopReason),
    /// The store and the server disagree in a way only recovery (design §6.4) resolves, and this build has none.
    #[error("{0}; this build cannot recover the folder yet")]
    Recovery(String),
    /// The server refused a call for good.
    #[error("{0}")]
    Refused(String),
    /// The server sent something this build cannot apply.
    #[error("{0}")]
    Protocol(String),
}

impl Stop {
    pub(crate) fn after_call(error: &CallError, why: String) -> Stop {
        match error {
            CallError::Server(_) => Stop::Refused(why),
            CallError::Stopped(reason) => Stop::Session(reason.clone()),
            CallError::NotResent(_) | CallError::TooLarge { .. } | CallError::UnexpectedResult => {
                Stop::Protocol(why)
            }
        }
    }
}

/// Why [`Pending::stage`] returned without a staged file. The effect stays in flight.
#[derive(Debug)]
pub(crate) enum Stall {
    /// The transfer failed for now with `why`. Stage again after `wait`.
    Later {
        wait: Duration,
        why: String,
        kind: WaitKind,
    },
    /// Staging cannot go on: the root was withdrawn, a staged file could not be written, or the session ended.
    Halt(Stop),
}

/// How an effect ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Done,
    /// The node stays blocked until the effect planned for it changes, except after an interrupted effect or one L
    /// no longer takes, which can be planned again unchanged.
    Failed(String),
    /// Failed, and planning the effect again cannot help until the person changes the disk: the folder it removes
    /// holds entries L does not know. The node stays blocked with no wait to end it.
    Stuck(String),
}

/// What the effect sections share with the observer's thread.
#[derive(Debug, Default)]
pub(crate) struct Sections {
    /// Odd while an effect section runs. Each section adds one when it starts and one when it ends.
    epoch: AtomicU64,
    /// Scopes the observer resets to L's view and scans again before its next batch, in the order requested, and
    /// how many requests there have been.
    resets: Mutex<(Vec<Reset>, u64)>,
}

/// A count of the reset requests of the effect sections. The count a request returns names that request and every
/// one before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Requests(u64);

/// The epoch read before a batch's scans began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp(u64);

impl Sections {
    /// Read before each [`Observer::observe`](crate::observe::Observer::observe), so the stamp covers the batch's
    /// scans and content reads.
    pub fn stamp(&self) -> Stamp {
        Stamp(self.epoch.load(Ordering::SeqCst))
    }

    /// The resets to pass to [`FsObserver::reset`] before the next observation, with the requests they complete.
    pub fn take_resets(&self) -> (Vec<Reset>, Requests) {
        let mut resets = self.resets.lock().unwrap();
        (std::mem::take(&mut resets.0), Requests(resets.1))
    }

    fn request(&self, resets: Vec<Reset>) -> Requests {
        let mut requested = self.resets.lock().unwrap();
        requested.0.extend(resets);
        requested.1 += 1;
        Requests(requested.1)
    }

    /// One pass of the observer's thread. It resets the scopes the effect sections asked for, then takes the stamp,
    /// so the stamp covers the batch's scans and content reads. After a reset it does not wait for a change, since
    /// the task may plan nothing until that scan is reported; otherwise it waits up to `idle`.
    pub fn observe(
        &self,
        observer: &mut FsObserver,
        idle: Duration,
    ) -> Result<Option<Batch>, koidrive_fs::Error> {
        let (resets, requests) = self.take_resets();
        let wait = if resets.is_empty() {
            idle
        } else {
            Duration::ZERO
        };
        observer.reset(resets);
        let stamp = self.stamp();
        let observation = observer.observe(wait)?;
        Ok(observation.map(|observation| Batch {
            stamp,
            answered: observer.answered().then_some(requests),
            observation,
        }))
    }
}

/// One observation with the stamp taken before it was scanned.
#[derive(Debug)]
pub(crate) struct Batch {
    pub stamp: Stamp,
    /// The reset requests this batch answers: the observer was given the resets of each one, and a scan made after
    /// that settled every scope of them, in this batch or in one sent before it. `None` while a reset the observer
    /// was given still waits for such a scan. A stamp cannot say this: it tells when the scans of a batch began, not
    /// which scopes they read.
    pub answered: Option<Requests>,
    pub observation: Observation,
}

/// The places a crash test stops an effect at, besides the steps of `core/fs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cut {
    /// Inside the transaction that records the planned effect, before it commits.
    BeforePlan,
    /// After that transaction committed.
    AfterPlan,
    Step(Step),
    /// Inside the transaction that acknowledges the effect, before it commits.
    BeforeAck,
    /// After that transaction committed.
    AfterAck,
}

type Hook = Box<dyn FnMut(Cut) + Send>;

/// The locked folder, as effects and observations reach it.
pub(crate) struct Disk {
    space: Arc<Space>,
    path: PathBuf,
    space_id: String,
    sections: Arc<Sections>,
    /// The paths of the latest effect sections, each with the epoch its section ended at, oldest first.
    spans: VecDeque<(u64, Vec<PathBuf>)>,
    /// The end epoch of the latest section that `spans` no longer holds.
    forgotten: u64,
    /// The reset request of the latest effect section that can have changed which rules apply where: it wrote, moved
    /// or removed a rules file, or it moved a folder, whose rules files the trees hold by path. Set until a batch
    /// answers that request, and so has passed the rules of a scan of the section's paths.
    rules_after: Option<Requests>,
    hook: Hook,
}

/// A planned effect the store holds.
pub(crate) struct Pending {
    space: Arc<Space>,
    record: EffectRecord,
    effect: Effect,
    contents: Expected,
    /// The version whose chunk list a write downloads when its content omits the list, from the store's R.
    version: Option<String>,
}

/// An effect ready to apply: a write's file is staged and flushed, or the reason it cannot be fetched.
pub(crate) struct Staged {
    record: EffectRecord,
    effect: Effect,
    contents: Expected,
    unfetchable: Option<String>,
}

/// Where chunk lists and chunks come from: the session and [`Transfer`] in the folder task.
pub(crate) trait Fetch {
    /// Every page of the chunk list of `version_id`, in order.
    async fn version_chunks(&self, version_id: &str) -> Result<Vec<ChunkRef>, ChunkError>;

    /// Hands each distinct chunk of `chunks` to `write` once, checked against its hash and size, as
    /// [`Transfer::download`] does.
    async fn download(
        &self,
        chunks: &[ChunkRef],
        write: &mut (dyn FnMut(&ChunkRef, &[u8]) -> io::Result<()> + Send),
    ) -> Result<(), ChunkError>;
}

impl Fetch for (&Session, &Transfer) {
    async fn version_chunks(&self, version_id: &str) -> Result<Vec<ChunkRef>, ChunkError> {
        let mut chunks = Vec::new();
        let mut page = 0;
        loop {
            let found = self.0.get_version_chunks(version_id, page).await?;
            chunks.extend(found.chunks);
            page += 1;
            if page >= found.page_count {
                return Ok(chunks);
            }
        }
    }

    async fn download(
        &self,
        chunks: &[ChunkRef],
        write: &mut (dyn FnMut(&ChunkRef, &[u8]) -> io::Result<()> + Send),
    ) -> Result<(), ChunkError> {
        self.1
            .download(self.0, chunks, |chunk, bytes| {
                crate::volume::downloaded(bytes.len());
                std::future::ready(write(chunk, &bytes))
            })
            .await
    }
}

impl Disk {
    /// Startup for the folder `link` names, through `lock`, the descriptor the task locked. In order:
    ///
    /// 1. The root check of design §6.2 through that descriptor: the linked path still names it, it is on the linked
    ///    device, and its marker names the space. Nothing is written when it fails.
    /// 2. The counters: with *e* the store's effect counter and *upper* one more while an effect is in flight, a
    ///    marker above *upper* is [`Stop::StoreBehind`] and one below *e* is [`Stop::DiskBehind`]. Both leave the
    ///    marker, staging and quarantine as they are, for recovery (§6.4).
    /// 3. The no-replace probe.
    /// 4. The effect in flight is recovered without staging anything again, and acknowledged. An interrupted one is
    ///    planned again; one that met another state on disk stays blocked, and so does one that recovery found done
    ///    where L shows another entry ([`koidrive_engine::effects::Effects::confirms`]). The marker must then equal
    ///    *e*.
    /// 5. Staged files without an effect are deleted.
    ///
    /// The observer then resets the paths of a recovered effect to L, so it reads a file the effect placed before
    /// the crash once, whatever the snapshot it starts from says about that file.
    pub fn start(
        lock: &File,
        link: &Link,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Disk, Stop> {
        Self::start_with_hook(lock, link, engine, store, Box::new(|_| {}))
    }

    fn start_with_hook(
        lock: &File,
        link: &Link,
        engine: &mut Engine,
        store: &mut Store,
        hook: Hook,
    ) -> Result<Disk, Stop> {
        let space = Space::bind(OwnedFd::from(lock.try_clone()?), &link.root)?;
        let mut disk = Disk {
            space: Arc::new(space),
            path: link.root.clone(),
            space_id: link.space_id.clone(),
            sections: Arc::default(),
            spans: VecDeque::new(),
            forgotten: 0,
            rules_after: None,
            hook,
        };
        if !disk.space.held()? {
            return Err(Stop::Withdrawn(disk.path));
        }
        let found = disk.space.id().dev;
        if found != link.root_dev {
            return Err(Stop::Root(RootProblem::OtherDevice {
                expected: link.root_dev,
                found,
            }));
        }
        let marker = disk.marker()?;
        let cleared = engine.effects.cleared();
        let upper = cleared + u64::from(engine.effects.in_flight().is_some());
        if marker > upper {
            return Err(Stop::StoreBehind { marker, upper });
        }
        if marker < cleared {
            return Err(Stop::DiskBehind { marker, cleared });
        }
        if !disk.space.probe_no_replace()? {
            return Err(Stop::NoReplace);
        }
        if let Some(record) = engine.effects.in_flight().cloned() {
            let (effect, mut contents) = translate(&record)?;
            let marker = disk.counter(record.counter);
            let hook = &mut disk.hook;
            let outcome = disk.space.recover(
                &marker,
                &effect,
                &mut contents,
                SystemTime::now(),
                &mut |step| hook(Cut::Step(step)),
            )?;
            let outcome = confirmed(outcome, engine);
            acknowledge(&record, &outcome, true, &link.space_id, engine, store, hook)?;
            let (marker, cleared) = (disk.marker()?, engine.effects.cleared());
            if marker != cleared {
                return Err(Stop::DiskBehind { marker, cleared });
            }
            let scopes = scopes(&effect.action, &outcome);
            disk.sections.request(engine.resets(scopes));
        }
        disk.space.clear_staging(None)?;
        Ok(disk)
    }

    /// What the observer's thread shares with the effect sections. The observer starts only once [`Disk::start`]
    /// returned, since this is where it gets its stamps.
    pub fn sections(&self) -> Arc<Sections> {
        self.sections.clone()
    }

    /// Reclaims the local trash at `now` with a cap of `cap` bytes ([`Space::reclaim_trash`]). A trashed file is
    /// compared with the content its effect recorded for it.
    pub fn reclaim_trash(&self, now: SystemTime, cap: u64) -> io::Result<Reclaimed> {
        self.space.reclaim_trash(now, cap, &mut |file, content| {
            let Ok(content) = Content::decode(content) else {
                return Ok(false);
            };
            holds(file, Some(&content), false)
        })
    }

    /// Plans the next effect and records it, with the ID generator, in one store transaction. Returns `None` when
    /// nothing is planned, including while an effect is in flight. The nodes planning unblocks are stored unblocked
    /// also then. A record that cannot become a `core/fs` effect (a name that is not a single file name) is never
    /// committed, and stops the task.
    pub fn next(
        &mut self,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Option<Pending>, Stop> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        let space = &self.space;
        let hook = &mut self.hook;
        let pending = store.transaction(|tx| -> Result<_, Stop> {
            let blocked = engine.effects.blocked().len();
            let Some(record) = engine
                .effects
                .plan(
                    &engine.trees,
                    engine.commits.in_flight(),
                    &mut engine.ids,
                    now_ms,
                )
                .cloned()
            else {
                if engine.effects.blocked().len() != blocked {
                    engine.checkpoint(tx)?;
                }
                return Ok(None);
            };
            let (effect, contents) = translate(&record)?;
            let version = version_of(tx, &record)?;
            engine.checkpoint(tx)?;
            hook(Cut::BeforePlan);
            Ok(Some(Pending {
                space: space.clone(),
                record,
                effect,
                contents,
                version,
            }))
        })?;
        if pending.is_some() {
            hook(Cut::AfterPlan);
        }
        Ok(pending)
    }

    /// The effect section, which has no await. It checks that `staged` is the effect in flight and that the root is
    /// held, applies the effect with a marker holding the record's counter, acknowledges the outcome in one
    /// transaction, and asks the observer to reset the paths the effect acted on to L as acknowledged. A write whose
    /// content could not be fetched only writes the marker and drops its staged file, and fails. So does an effect
    /// that L no longer takes ([`EffectRecord::applies`]), because a batch changed L after the plan. Nothing else under the
    /// root is touched for it.
    pub fn apply(
        &mut self,
        staged: Staged,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<Settled, Stop> {
        let Staged {
            record,
            effect,
            mut contents,
            unfetchable,
        } = staged;
        if engine.effects.in_flight() != Some(&record) {
            return Err(Stop::Record {
                effect_id: record.effect_id,
                what: "it is not the effect in flight".into(),
            });
        }
        if !self.space.held()? {
            return Err(Stop::Withdrawn(self.path.clone()));
        }
        let marker = self.counter(record.counter);
        self.sections.epoch.fetch_add(1, Ordering::SeqCst);
        let hook = &mut self.hook;
        let mut step = |step| hook(Cut::Step(step));
        let now = SystemTime::now();
        let outcome = match &unfetchable {
            None if record.applies(&engine.trees, engine.commits.in_flight()) => {
                self.space
                    .apply(&marker, &effect, &mut contents, now, &mut step)?
            }
            _ => confirmed(
                self.space
                    .recover(&marker, &effect, &mut contents, now, &mut step)?,
                engine,
            ),
        };
        let settled = acknowledge(
            &record,
            &outcome,
            false,
            &self.space_id,
            engine,
            store,
            hook,
        )?;
        let end = self.sections.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let scopes = scopes(&effect.action, &outcome);
        let moved_folder = matches!(effect.action, Action::Move { .. })
            && (engine.trees.local().get(&record.node))
                .is_some_and(|node| node.shape.kind == Kind::Dir);
        let rules_file = |scope: &PathBuf| {
            (scope.file_name()).is_some_and(|name| is_rules_file(&name.to_string_lossy()))
        };
        let requested = (self.sections).request(engine.resets(scopes.iter().cloned()));
        if !scopes.is_empty() && (moved_folder || scopes.iter().any(rules_file)) {
            self.rules_after = Some(requested);
        }
        self.spans.push_back((end, scopes));
        while self.spans.len() > SPANS {
            if let Some((end, _)) = self.spans.pop_front() {
                self.forgotten = end;
            }
        }
        Ok(match (settled, unfetchable) {
            (Settled::Failed(_), Some(why)) => Settled::Failed(why),
            (settled, _) => settled,
        })
    }

    /// Puts an observation batch into L in one transaction, after giving the trees the rules files it lists. Refuses
    /// a batch scanned from another folder than the locked one, a root that is no longer held, and a batch whose root
    /// check failed. The rules of a batch are taken even when its changes are dropped: the scans report a rules file
    /// only when it changed. Drops the changes at or below
    /// the paths of every effect section that ended after the batch's stamp, and a whole batch older than the
    /// sections remembered. A change at or below a path that a dropped change names is dropped with it: the removals a
    /// scan reports below a folder it saw move belong to that move. The observer then resets every path a dropped change names, both ends of a rename, or
    /// the whole tree for a dropped batch, to L as it is after the batch, so a later batch reports what was lost.
    pub fn observed(
        &mut self,
        batch: Batch,
        engine: &mut Engine,
        store: &mut Store,
    ) -> Result<(), Stop> {
        let Batch {
            stamp,
            answered,
            observation,
        } = batch;
        if observation.scanned != Some(self.space.id()) {
            return Err(Stop::OtherRoot);
        }
        if !self.space.held()? {
            return Err(Stop::Withdrawn(self.path.clone()));
        }
        observation.root.map_err(Stop::Root)?;
        engine.rules(observation.rules, observation.all_rules);
        if (self.rules_after).is_some_and(|requested| answered >= Some(requested)) {
            self.rules_after = None;
        }
        if stamp.0 < self.forgotten {
            tracing::debug!(
                space_id = self.space_id.as_str(),
                changes = observation.changes.len(),
                "dropped an observation older than the effects the folder remembers, and reset the whole folder"
            );
            engine.read(&observation.read, &observation.unreadable);
            self.sections.request(engine.resets([PathBuf::new()]));
            return Ok(());
        }
        let mut scopes: BTreeSet<PathBuf> = (self.spans.iter())
            .filter(|(end, _)| *end > stamp.0)
            .flat_map(|(_, scopes)| scopes.iter().cloned())
            .collect();
        let mut stale = vec![false; observation.changes.len()];
        let mut grew = true;
        while grew {
            grew = false;
            for (change, stale) in observation.changes.iter().zip(&mut stale) {
                if !*stale
                    && paths(change).any(|path| path.ancestors().any(|at| scopes.contains(at)))
                {
                    *stale = true;
                    grew = true;
                    scopes.extend(paths(change).map(Path::to_path_buf));
                }
            }
        }
        let dropped: BTreeSet<PathBuf> = (observation.changes.iter().zip(&stale))
            .filter(|(_, stale)| **stale)
            .flat_map(|(change, _)| paths(change).map(Path::to_path_buf))
            .collect();
        let changes: Vec<Observed> = (observation.changes.into_iter().zip(stale))
            .filter(|(_, stale)| !stale)
            .map(|(change, _)| change)
            .collect();
        if !changes.is_empty() {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as u64);
            store.transaction(|tx| -> Result<(), Stop> {
                engine.observe(&changes, now_ms);
                Ok(engine.checkpoint(tx)?)
            })?;
        }
        engine.read(&observation.read, &observation.unreadable);
        if !dropped.is_empty() {
            tracing::debug!(
                space_id = self.space_id.as_str(),
                ?dropped,
                "dropped the changes an effect made after the observation began, and reset their paths"
            );
            self.sections.request(engine.resets(dropped));
        }
        Ok(())
    }

    /// True from an effect that can have changed which rules apply where until a batch has passed the rules of a scan
    /// of that effect's paths: an effect that wrote, moved or removed a rules file, or that moved a folder. The caller
    /// plans nothing meanwhile (design §5.10). Otherwise what a rules file that was just downloaded excludes would be
    /// downloaded first, and a remote change would be written into a file that the rules file of a moved folder
    /// excludes, because the trees hold each rules file by the path of its folder. The wait ends with the batch that
    /// answers the reset the effect requested ([`Batch::answered`]), never with a batch that only began later: the
    /// observer may have taken its resets before the effect ended, or may have scanned other paths. The observer
    /// answers a reset with a batch also when the scan found nothing, so the wait ends without a change on disk.
    pub fn awaits_rules(&self) -> bool {
        self.rules_after.is_some()
    }

    /// Asks the observer to reset `paths` to L and scan them again.
    pub fn rescan(&self, engine: &Engine, paths: Vec<PathBuf>) {
        self.sections.request(engine.resets(paths));
    }

    /// The chunks `commit` adds, with where the bytes of each are: for every content the commit creates or
    /// updates, the file of its node in L and the offset of each chunk in it.
    pub fn upload(&self, commit: &Commit, engine: &Engine) -> Upload {
        let mut upload = Upload {
            space: self.space.clone(),
            chunks: Vec::new(),
            sources: HashMap::new(),
        };
        for op in &commit.ops {
            let (node, content) = match &op.op {
                Some(op::Op::CreateFile(create)) => (&create.node_id, &create.content),
                Some(op::Op::UpdateContent(update)) => (&update.node_id, &update.content),
                _ => continue,
            };
            let Some(content) = content else { continue };
            let path = engine.path_of(node);
            let at = location_of(&engine.trees, node).and_then(|at| location(&at).ok());
            let identity = (engine.trees.local().get(node)).map(|held| file_id(held.identity));
            let mut offset = 0;
            for chunk in &content.chunks {
                upload.sources.entry(chunk.hash.clone()).or_insert(Source {
                    path: path.clone(),
                    at: at.clone(),
                    identity,
                    offset,
                });
                upload.chunks.push(chunk.clone());
                offset += u64::from(chunk.size);
            }
        }
        upload
    }

    fn counter(&self, effects: u64) -> Marker {
        Marker {
            space_id: self.space_id.clone(),
            effects,
        }
    }

    /// The marker's counter, read through the locked descriptor, for a marker that names this space.
    fn marker(&self) -> Result<u64, Stop> {
        let marker = self
            .space
            .marker()
            .map_err(|error| Stop::Root(RootProblem::BadMarker(error)))?
            .ok_or(Stop::Root(RootProblem::NoMarker))?;
        if marker.space_id != self.space_id {
            return Err(Stop::Root(RootProblem::OtherSpace(marker.space_id)));
        }
        Ok(marker.effects)
    }
}

/// The chunks a commit adds to the server and where the bytes of each are in the folder.
pub(crate) struct Upload {
    space: Arc<Space>,
    /// Every chunk of every content the commit creates or updates, in the order of its ops.
    pub chunks: Vec<ChunkRef>,
    sources: HashMap<Vec<u8>, Source>,
}

struct Source {
    /// `None` when L no longer holds the node under the root.
    path: Option<PathBuf>,
    at: Option<Location>,
    identity: Option<FileId>,
    offset: u64,
}

impl Upload {
    /// The bytes of `chunk`, read from the file L holds for its node, through the locked descriptor and without
    /// following a link. Another file at that path is an error. The caller checks the bytes against the chunk's hash.
    pub fn read(&self, chunk: &ChunkRef) -> io::Result<Bytes> {
        let gone = || {
            io::Error::new(
                io::ErrorKind::NotFound,
                "the file is no longer in the folder",
            )
        };
        let source = self.sources.get(&chunk.hash).ok_or_else(gone)?;
        let at = source.at.as_ref().ok_or_else(gone)?;
        let (folder, name) = self.space.parent(at).map_err(|_| gone())?;
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let file = File::from(rustix::fs::openat(folder.fd(), name, flags, Mode::empty())?);
        let meta = file.metadata()?;
        let same =
            |id: FileId| meta.is_file() && meta.dev() == id.dev && u128::from(meta.ino()) == id.ino;
        if !source.identity.is_some_and(same) {
            return Err(gone());
        }
        let mut bytes = vec![0; chunk.size as usize];
        file.read_exact_at(&mut bytes, source.offset)?;
        Ok(Bytes::from(bytes))
    }

    /// The files the chunks are read from, relative to the root.
    pub fn paths(&self) -> Vec<PathBuf> {
        let paths: BTreeSet<&PathBuf> = (self.sources.values())
            .filter_map(|source| source.path.as_ref())
            .collect();
        paths.into_iter().cloned().collect()
    }
}

impl Pending {
    /// Downloads a written file into staging, writing each chunk at every offset the content lists it, cuts the file
    /// to the content's size, sets its modification time to the content's and its executable bit, and flushes it.
    /// Every chunk is checked against its hash and size, and the chunk list against the manifest digest before
    /// anything is fetched. Other effects stage nothing.
    ///
    /// A content that cannot be fetched does not stall. That is a chunk the server refused as gone or whose bytes
    /// never matched, a malformed manifest, or a content that omits its chunk list when the store's R has no version
    /// with that content. The effect then fails when it is applied, with that reason.
    pub async fn stage(&self, fetch: &impl Fetch) -> Result<Staged, Stall> {
        let EngineEffect::WriteFile { content, .. } = &self.record.effect else {
            return Ok(self.staged(None));
        };
        let chunks = if content.chunks.is_empty() && content.chunk_count > 0 {
            let Some(version) = &self.version else {
                return Ok(self.staged(Some(
                    "its content omits the chunk list, and the remote tree has no version with that content".into(),
                )));
            };
            match fetch.version_chunks(version).await {
                Ok(chunks) => chunks,
                Err(error) => return self.failed_transfer(error, false),
            }
        } else {
            content.chunks.clone()
        };
        let manifest = match Manifest::parse(content, chunks) {
            Ok(manifest) => manifest,
            Err(why) => return Ok(self.staged(Some(why))),
        };
        let id = &self.record.effect_id;
        let file = self.space.stage(id).map_err(halt)?;
        let mut corrupt = false;
        let mut received = 0;
        let fetched = fetch
            .download(&manifest.distinct, &mut |chunk, bytes| {
                if !koidrive_net::matches(chunk, bytes) {
                    corrupt = true;
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the bytes do not match the chunk's hash and size",
                    ));
                }
                for offset in manifest.offsets.get(&chunk.hash).into_iter().flatten() {
                    file.write_all_at(bytes, *offset)?;
                }
                received += 1;
                Ok(())
            })
            .await;
        if let Err(error) = fetched {
            return self.failed_transfer(error, corrupt);
        }
        if received != manifest.distinct.len() {
            return Ok(self.staged(Some(format!(
                "the transfer handed over {received} of its {} distinct chunks",
                manifest.distinct.len()
            ))));
        }
        file.set_len(content.size).map_err(halt)?;
        if file.metadata().map_err(halt)?.size() != content.size {
            return Ok(self.staged(Some(
                "the staged file does not have the content's size".into(),
            )));
        }
        let mtime = rustix::fs::Timespec {
            tv_sec: content.mtime_ns.div_euclid(1_000_000_000),
            tv_nsec: content.mtime_ns.rem_euclid(1_000_000_000) as _,
        };
        rustix::fs::futimens(
            &file,
            &rustix::fs::Timestamps {
                last_access: mtime,
                last_modification: mtime,
            },
        )
        .map_err(|error| halt(error.into()))?;
        drop(file);
        self.space
            .finish_staged(id, content.executable)
            .map_err(halt)?;
        Ok(self.staged(None))
    }

    fn staged(&self, unfetchable: Option<String>) -> Staged {
        Staged {
            record: self.record.clone(),
            effect: self.effect.clone(),
            contents: self.contents.clone(),
            unfetchable,
        }
    }

    /// What a failed transfer means for the effect: a chunk the server no longer has, or whose bytes never matched,
    /// fails it; a refusal the server asks to retry stalls it; anything else halts the task with the effect in flight.
    fn failed_transfer(&self, error: ChunkError, corrupt: bool) -> Result<Staged, Stall> {
        match corrupt {
            true => failures::failed(Failure::Download(failures::Transfer::Corrupt)),
            false => failures::transfer_failed(&error, Failure::Download),
        }
        let disposition = match &error {
            _ if corrupt => None,
            ChunkError::Corrupt { .. } | ChunkError::NoTarget { .. } => None,
            ChunkError::Refused {
                status: 404 | 410, ..
            } => None,
            ChunkError::Refused {
                error: Some(refusal),
                ..
            } => Some(policy::after_refusal(refusal)),
            ChunkError::Call(call)
                if call.code() == Some(koidrive_proto::v1::error::Code::NotFound) =>
            {
                None
            }
            ChunkError::Call(call) => Some(policy::after_call_error(call)),
            ChunkError::Refused { .. } | ChunkError::Unavailable { .. } => {
                Some(Disposition::Backoff)
            }
            ChunkError::SourceChanged { .. } | ChunkError::Io { .. } => Some(Disposition::Halt),
        };
        let why = match &error {
            ChunkError::Refused {
                error: Some(refusal),
                ..
            }
            | ChunkError::Call(CallError::Server(refusal)) => policy::describe(refusal),
            other => other.to_string(),
        };
        let kind = policy::wait_kind(match &error {
            ChunkError::Refused {
                error: Some(refusal),
                ..
            }
            | ChunkError::Call(CallError::Server(refusal)) => Some(refusal),
            _ => None,
        });
        let later = |wait| Stall::Later {
            wait,
            why: format!("downloading a file's content failed: {why}"),
            kind,
        };
        match disposition {
            None => Ok(self.staged(Some(format!("its content cannot be downloaded: {error}")))),
            Some(Disposition::RetryAfter(wait)) => Err(later(wait)),
            Some(Disposition::Backoff) => Err(later(STAGE_AGAIN)),
            Some(Disposition::Halt) => {
                let what = format!("its content could not be downloaded: {error}");
                Err(Stall::Halt(match &error {
                    ChunkError::Call(call) => Stop::after_call(call, what),
                    ChunkError::Refused { .. } => Stop::Refused(what),
                    _ => Stop::Record {
                        effect_id: self.record.effect_id.clone(),
                        what,
                    },
                }))
            }
        }
    }
}

fn halt(error: io::Error) -> Stall {
    Stall::Halt(Stop::Io(error))
}

/// Records the outcome of the effect in flight in one transaction: done with the identity of the entry it created,
/// or failed. The mass-delete guard counts a removal done for another device's deletion, and a notice it opens or
/// updates is stored in the same transaction. With `retry`, an interrupted effect is unblocked in the same
/// transaction, so it is planned again. An outcome that L no longer takes ([`EffectRecord::applies`]) is never
/// written to L, whatever the disk holds. The effect fails and is unblocked, and the planner leaves it out until L
/// changes.
fn acknowledge(
    record: &EffectRecord,
    outcome: &Outcome,
    retry: bool,
    space_id: &str,
    engine: &mut Engine,
    store: &mut Store,
    hook: &mut Hook,
) -> Result<Settled, Stop> {
    let refused = |error| Stop::Record {
        effect_id: record.effect_id.clone(),
        what: format!("the engine refused its outcome: {error:?}"),
    };
    let settled = store.transaction(|tx| -> Result<_, Stop> {
        let settled = match outcome {
            _ if !record.applies(&engine.trees, engine.commits.in_flight()) => {
                engine.effects.failed(&record.effect_id).map_err(refused)?;
                engine.effects.unblock(&record.node);
                Settled::Failed("L changed since it was planned".into())
            }
            Outcome::Done { identity } => {
                let removal = (engine.effects)
                    .done(
                        &mut engine.trees,
                        engine.commits.in_flight(),
                        &record.effect_id,
                        identity.map(file_identity),
                    )
                    .map_err(refused)?;
                if let Some(removal) = removal {
                    let now_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |since| since.as_millis() as u64);
                    let Engine {
                        trees,
                        commits,
                        ids,
                        cursor,
                        ..
                    } = engine;
                    let last_seq = cursor.as_ref().map_or(0, |cursor| cursor.last_seq);
                    commits.count_remote(removal, trees, last_seq, ids, now_ms);
                }
                Settled::Done
            }
            Outcome::Failed { reason, .. } => {
                match reason {
                    Reason::Overtaken => {
                        (engine.effects).overtaken(&mut engine.trees, &record.effect_id)
                    }
                    _ => engine.effects.failed(&record.effect_id),
                }
                .map_err(refused)?;
                if retry && *reason == Reason::Interrupted {
                    engine.effects.unblock(&record.node);
                }
                let failed = |why: &str| Settled::Failed(why.into());
                match reason {
                    Reason::Moved => failed("a folder on its path is missing or was replaced"),
                    Reason::Missing => failed("the entry is not there"),
                    Reason::Changed => failed("the entry changed on this device"),
                    Reason::Taken => failed("another entry holds its name"),
                    Reason::Interrupted => failed("it was interrupted"),
                    Reason::Overtaken => failed("the file it wrote changed on this device"),
                    Reason::Holds(names) => {
                        let quoted: Vec<_> = (names.iter())
                            .map(|name| koidrive_fs::quoted(name).to_string())
                            .collect();
                        Settled::Stuck(format!(
                            "the folder holds entries this device does not sync: {}",
                            quoted.join(", ")
                        ))
                    }
                }
            }
        };
        engine.ask(tx, space_id)?;
        engine.checkpoint(tx)?;
        hook(Cut::BeforeAck);
        Ok(settled)
    })?;
    hook(Cut::AfterAck);
    Ok(settled)
}

/// `outcome` as recovery reports it for the effect in flight, unless L refutes a done
/// ([`koidrive_engine::effects::Effects::confirms`]). Then the effect fails as it would have on the entry recovery
/// found: a removal finds its entry missing, and a write or a new folder finds its name taken.
fn confirmed(outcome: Outcome, engine: &Engine) -> Outcome {
    let Outcome::Done { identity } = &outcome else {
        return outcome;
    };
    if (engine.effects).confirms(&engine.trees, identity.map(file_identity).as_ref()) {
        return outcome;
    }
    let reason = match engine.effects.in_flight().map(|record| &record.effect) {
        Some(EngineEffect::RemoveDir { .. } | EngineEffect::RemoveFile { .. }) => Reason::Missing,
        _ => Reason::Taken,
    };
    Outcome::Failed {
        reason,
        recovered: None,
    }
}

/// The version of a write whose content omits its chunk list: the version R holds for the node, if it has that
/// content.
fn version_of(tx: &Tx<'_>, record: &EffectRecord) -> Result<Option<String>, Stop> {
    let EngineEffect::WriteFile { content, .. } = &record.effect else {
        return Ok(None);
    };
    if !content.chunks.is_empty() || content.chunk_count == 0 {
        return Ok(None);
    }
    Ok(tx
        .remote_nodes()?
        .into_iter()
        .find(|node| node.node_id == record.node)
        .and_then(|node| node.current)
        .filter(|version| {
            version
                .content
                .as_ref()
                .is_some_and(|found| found.manifest_digest == content.manifest_digest)
        })
        .map(|version| version.version_id))
}

/// The paths an effect acted on, relative to the root: its target, the source of a move or of a replacement that
/// also moves the node, and a recovered file. Paths inside the private folder are left out, since no scan reports
/// them.
fn scopes(action: &Action, outcome: &Outcome) -> Vec<PathBuf> {
    let locations = match action {
        Action::CreateDir { at } | Action::RemoveDir { at, .. } => vec![at],
        Action::WriteFile { at, replaces } => std::iter::once(at)
            .chain(replaces.iter().map(|d| &d.at))
            .collect(),
        Action::Move { from, to, .. } => vec![from, to],
        Action::RemoveFile { displaced } => vec![&displaced.at],
    };
    let mut scopes: Vec<PathBuf> = locations
        .into_iter()
        .filter_map(|location| match location {
            Location::At {
                base: Base::Root,
                dirs,
                name,
            } => Some(
                dirs.iter()
                    .map(|dir| dir.name.as_os_str())
                    .chain([name.as_os_str()])
                    .collect(),
            ),
            Location::At { .. } | Location::Parked(_) => None,
        })
        .collect();
    if let Outcome::Failed {
        recovered: Some(path),
        ..
    } = outcome
    {
        scopes.push(path.clone());
    }
    scopes.dedup();
    scopes
}

/// The paths a change names.
fn paths(change: &Observed) -> impl Iterator<Item = &Path> {
    let (first, second) = match change {
        Observed::Created { path, .. }
        | Observed::Modified { path, .. }
        | Observed::Removed { path, .. } => (path, None),
        Observed::Renamed { from, to, .. } => (from, Some(to)),
    };
    std::iter::once(first.as_path()).chain(second.map(PathBuf::as_path))
}

/// The record as a `core/fs` effect, with the contents it expects. One match covers every effect, so a new kind
/// fails to build here. Names keep their exact bytes; a name that is not a single file name is refused. Expected
/// entries come from the identities and contents the record holds, never from the disk.
fn translate(record: &EffectRecord) -> Result<(Effect, Expected), Stop> {
    let bad = |what: String| Stop::Record {
        effect_id: record.effect_id.clone(),
        what,
    };
    name(&record.effect_id).map_err(bad)?;
    let (action, contents) = match &record.effect {
        EngineEffect::CreateDir { at } => (
            Action::CreateDir {
                at: location(at).map_err(bad)?,
            },
            Expected::default(),
        ),
        EngineEffect::WriteFile {
            at,
            content,
            replaces,
        } => (
            Action::WriteFile {
                at: location(at).map_err(bad)?,
                replaces: replaces.as_ref().map(displaced).transpose().map_err(bad)?,
            },
            Expected {
                displaced: replaces.as_ref().map(|d| d.content.clone()),
                written: Some(content.clone()),
            },
        ),
        EngineEffect::Move { from, identity, to } => (
            Action::Move {
                from: location(from).map_err(bad)?,
                id: file_id(*identity),
                to: location(to).map_err(bad)?,
            },
            Expected::default(),
        ),
        EngineEffect::RemoveFile { displaced: removed } => (
            Action::RemoveFile {
                displaced: displaced(removed).map_err(bad)?,
            },
            Expected {
                displaced: Some(removed.content.clone()),
                written: None,
            },
        ),
        EngineEffect::RemoveDir { at, identity } => (
            Action::RemoveDir {
                at: location(at).map_err(bad)?,
                id: file_id(*identity),
            },
            Expected::default(),
        ),
    };
    let effect = Effect {
        id: record.effect_id.clone(),
        action,
    };
    Ok((effect, contents))
}

fn location(location: &EngineLocation) -> Result<Location, String> {
    Ok(match location {
        EngineLocation::Parked(node) => Location::Parked(name(node)?),
        EngineLocation::At {
            base,
            dirs,
            name: last,
        } => Location::At {
            base: match base {
                EngineBase::Root => Base::Root,
                EngineBase::Parked { node, identity } => Base::Parked {
                    name: name(node)?,
                    id: file_id(*identity),
                },
            },
            dirs: dirs
                .iter()
                .map(|dir| {
                    Ok(Dir {
                        name: name(&dir.name)?,
                        id: file_id(dir.identity),
                    })
                })
                .collect::<Result<_, String>>()?,
            name: name(last)?,
        },
    })
}

/// The entry a file effect displaces, as the record expects it.
fn displaced(displaced: &EngineDisplaced) -> Result<Displaced, String> {
    Ok(Displaced {
        at: location(&displaced.at)?,
        entry: Entry {
            kind: EntryKind::File,
            id: file_id(displaced.identity),
            size: displaced.content.size,
            mtime_ns: i128::from(displaced.content.mtime_ns),
            executable: displaced.content.executable,
        },
    })
}

/// `value` as a file name with the same bytes, if it is a single one.
fn name(value: &str) -> Result<OsString, String> {
    if matches!(value, "" | "." | "..") || value.contains(['/', '\0']) {
        return Err(format!("{value:?} is not a single file name"));
    }
    Ok(OsString::from(value))
}

/// A written content's chunk list, checked: a known chunker, as many 32-byte raw chunks as the content counts,
/// one size per hash, sizes that add up to the content's size, and the content's manifest digest.
struct Manifest {
    /// Each hash once, in order of first appearance.
    distinct: Vec<ChunkRef>,
    /// Where each hash appears.
    offsets: BTreeMap<Vec<u8>, Vec<u64>>,
}

impl Manifest {
    fn parse(content: &Content, chunks: Vec<ChunkRef>) -> Result<Manifest, String> {
        ChunkerConfig::from_name(&content.chunker).map_err(|error| {
            format!(
                "its chunker {:?} is not supported: {error}",
                content.chunker
            )
        })?;
        if chunks.len() != content.chunk_count as usize {
            return Err(format!(
                "its content counts {} chunks and lists {}",
                content.chunk_count,
                chunks.len()
            ));
        }
        if koidrive_chunk::manifest_digest(&chunks).as_slice() != content.manifest_digest {
            return Err("its chunk list does not match its manifest digest".into());
        }
        let mut sizes: BTreeMap<&[u8], u32> = BTreeMap::new();
        let mut offsets: BTreeMap<Vec<u8>, Vec<u64>> = BTreeMap::new();
        let mut distinct = Vec::new();
        let mut size = 0u64;
        for chunk in &chunks {
            if chunk.hash.len() != 32 || chunk.encoding != 0 || chunk.size == 0 {
                return Err("it lists a chunk that is not a raw chunk with a 32-byte hash".into());
            }
            if *sizes.entry(chunk.hash.as_slice()).or_insert(chunk.size) != chunk.size {
                return Err("it lists one chunk hash with two sizes".into());
            }
            let at = offsets.entry(chunk.hash.clone()).or_default();
            if at.is_empty() {
                distinct.push(chunk.clone());
            }
            at.push(size);
            size = size
                .checked_add(u64::from(chunk.size))
                .ok_or("its chunk sizes overflow")?;
        }
        if size != content.size {
            return Err(format!(
                "its chunks add up to {size} bytes, not to its size of {}",
                content.size
            ));
        }
        Ok(Manifest { distinct, offsets })
    }
}

/// The contents an effect expects: the displaced file's and the written file's, compared by size and manifest
/// digest, chunked with each content's own chunker. A written file must also have the content's executable bit.
#[derive(Clone, Debug, Default)]
struct Expected {
    displaced: Option<Content>,
    written: Option<Content>,
}

impl Contents for Expected {
    fn displaced(&mut self, file: &File) -> io::Result<bool> {
        holds(file, self.displaced.as_ref(), false)
    }

    fn written(&mut self, file: &File) -> io::Result<bool> {
        holds(file, self.written.as_ref(), true)
    }

    fn displaced_content(&self) -> Option<Vec<u8>> {
        let displaced = self.displaced.as_ref()?;
        let content = Content {
            size: displaced.size,
            manifest_digest: displaced.manifest_digest.clone(),
            chunker: displaced.chunker.clone(),
            ..Content::default()
        };
        Some(content.encode_to_vec())
    }
}

/// Whether `file` holds `expected`, read from its start. A file that changes while it is read does not.
fn holds(file: &File, expected: Option<&Content>, executable: bool) -> io::Result<bool> {
    let Some(expected) = expected else {
        return Ok(false);
    };
    let Ok(config) = ChunkerConfig::from_name(&expected.chunker) else {
        return Ok(false);
    };
    let before = file.metadata()?;
    if before.size() != expected.size
        || (executable && (before.mode() & 0o111 != 0) != expected.executable)
    {
        return Ok(false);
    }
    let chunks = koidrive_chunk::chunks(Positioned { file, at: 0 }, config)
        .map(|chunk| chunk.map(|chunk| chunk.chunk_ref()))
        .collect::<io::Result<Vec<ChunkRef>>>()?;
    let after = file.metadata()?;
    let same = |meta: &std::fs::Metadata| {
        (
            meta.size(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
        )
    };
    Ok(same(&before) == same(&after)
        && koidrive_chunk::manifest_digest(&chunks).as_slice() == expected.manifest_digest)
}

/// Reads a file from `at` with positional reads, whatever its descriptor's offset.
struct Positioned<'a> {
    file: &'a File,
    at: u64,
}

impl io::Read for Positioned<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.file.read_at(buf, self.at)?;
        self.at += read as u64;
        Ok(read)
    }
}

#[cfg(test)]
pub(crate) mod tests;
