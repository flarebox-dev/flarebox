// SPDX-License-Identifier: Apache-2.0

//! The files of telemetry under the profile's state directory, which belongs to one machine and is never copied
//! (`koidrive_driver::Paths`): the choice and the install id in `settings.json`, and what waits to be sent in
//! `queue.json`. Every process of the profile writes the queue under `lock`, so the daemon and a command that runs
//! next to it do not lose each other's sums. A batch being sent stays in the queue, marked as out, until its send
//! ends, so a send that is cancelled or killed before an answer loses nothing.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use crate::consent::{self, Status};
use crate::recorder::Pending;
use crate::schema::{DailyReport, Event, EventUuid, InstallId};

/// Days a report or an event waits to be sent before it is dropped.
pub const KEEP_DAYS: u32 = 14;
/// Events the queue holds at most; the oldest go first.
pub const MAX_EVENTS: usize = 500;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// The person's choice; none until asked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<InstallId>,
}

/// Reports and events taken from the queue to send.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Batch {
    #[serde(default)]
    pub days: Vec<DailyReport>,
    #[serde(default)]
    pub events: Vec<Event>,
}

impl Batch {
    pub fn is_empty(&self) -> bool {
        self.days.is_empty() && self.events.is_empty()
    }
}

/// A batch [`Store::take`] handed out. It stays in the queue until [`Store::sent`] removes it, and holds `send.lock`
/// meanwhile, so one send runs at a time. Given back, or dropped as when its send is cancelled or its process ends,
/// the next take hands out the same batch with the same uuids, so PostHog keeps one copy if an earlier try arrived.
#[derive(Debug)]
pub struct Sending {
    pub id: InstallId,
    pub batch: Batch,
    /// One per day of `batch`, then one per event.
    pub uuids: Vec<EventUuid>,
    _lock: File,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Queue {
    #[serde(default)]
    days: BTreeMap<u32, DailyReport>,
    #[serde(default)]
    events: Vec<Event>,
    /// The batch a send took and has not settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    out: Option<Out>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Out {
    id: InstallId,
    batch: Batch,
    uuids: Vec<EventUuid>,
}

impl Out {
    fn new(id: InstallId, batch: Batch) -> io::Result<Self> {
        let uuids = (0..batch.days.len() + batch.events.len())
            .map(|_| EventUuid::generate().map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        Ok(Out { id, batch, uuids })
    }

    /// Without what is older than [`KEEP_DAYS`]. None when nothing is left, or when its uuids don't match its batch.
    fn trimmed(self, today: u32) -> Option<Self> {
        if self.uuids.len() != self.batch.days.len() + self.batch.events.len() {
            return None;
        }
        let oldest = today.saturating_sub(KEEP_DAYS);
        let mut uuids = self.uuids.into_iter();
        let mut out = Out {
            id: self.id,
            batch: Batch::default(),
            uuids: Vec::new(),
        };
        for (report, uuid) in self.batch.days.into_iter().zip(uuids.by_ref()) {
            if report.day >= oldest {
                out.batch.days.push(report);
                out.uuids.push(uuid);
            }
        }
        for (event, uuid) in self.batch.events.into_iter().zip(uuids) {
            if event.day() >= oldest {
                out.batch.events.push(event);
                out.uuids.push(uuid);
            }
        }
        (!out.batch.is_empty()).then_some(out)
    }
}

impl Queue {
    fn put(&mut self, days: impl IntoIterator<Item = DailyReport>, events: Vec<Event>) {
        for report in days {
            let day = report.day;
            self.days
                .entry(day)
                .or_insert_with(|| DailyReport {
                    day,
                    ..DailyReport::default()
                })
                .merge(report);
        }
        for event in events {
            let same = (self.events.iter().chain(self.out_events()))
                .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                .count();
            if same < Event::PER_KIND_PER_DAY {
                self.events.push(event);
            }
        }
    }

    /// The events of the batch that is out, which count toward the caps until it is sent.
    fn out_events(&self) -> impl Iterator<Item = &Event> {
        self.out.iter().flat_map(|out| &out.batch.events)
    }

    /// Drops what is older than [`KEEP_DAYS`], in the batch that is out too, and the oldest events over
    /// [`MAX_EVENTS`].
    fn trim(&mut self, today: u32) {
        self.out = self.out.take().and_then(|out| out.trimmed(today));
        let oldest = today.saturating_sub(KEEP_DAYS);
        self.days
            .retain(|day, report| *day >= oldest && !report.is_empty());
        self.events.retain(|event| event.day() >= oldest);
        let over = (self.events.len() + self.out_events().count()).saturating_sub(MAX_EVENTS);
        self.events.drain(..over.min(self.events.len()));
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The telemetry files of the profile whose state directory is `state_dir`.
    pub fn new(state_dir: &Path) -> Self {
        Store {
            dir: state_dir.join("telemetry"),
        }
    }

    /// A missing file is the default: never asked, no id.
    pub fn settings(&self) -> io::Result<Settings> {
        read(&self.dir.join("settings.json"))
    }

    /// The mode with this profile's stored choice and the process's environment.
    pub fn status(&self, var: impl Fn(&str) -> Option<std::ffi::OsString>) -> io::Result<Status> {
        Ok(consent::resolve(var, self.settings()?.enabled))
    }

    /// Stores the person's choice. Turning it on keeps the install id or draws one. Turning it off deletes the id and
    /// everything waiting to be sent. A `settings.json` that cannot be parsed is replaced, so a broken file never
    /// stops someone from choosing, least of all from turning reports off.
    pub fn set(&self, on: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        let mut settings = match self.settings() {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Settings::default(),
            settings => settings?,
        };
        settings.enabled = Some(on);
        if on {
            settings.install_id.get_or_insert_with(InstallId::generate);
        } else {
            settings.install_id = None;
            remove(&self.dir.join("queue.json"))?;
        }
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(settings)
    }

    /// Deletes everything waiting to be sent and then draws a new install id, so a crash between the two never leaves
    /// old reports under the new id. Without an id there is nothing to reset.
    pub fn reset(&self) -> io::Result<Option<InstallId>> {
        let _lock = self.lock(true)?;
        remove(&self.dir.join("queue.json"))?;
        let mut settings = self.settings()?;
        if settings.install_id.is_none() {
            return Ok(None);
        }
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(Some(id))
    }

    /// The install id, drawn and stored when telemetry is on through `KOIDRIVE_TELEMETRY` and none exists yet.
    pub fn install_id(&self) -> io::Result<InstallId> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        if let Some(id) = &settings.install_id {
            return Ok(id.clone());
        }
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        Ok(id)
    }

    /// Adds what a recorder took to the queue and drops what is older than [`KEEP_DAYS`].
    pub fn absorb(&self, pending: Pending) -> io::Result<()> {
        self.absorb_at(pending, crate::today(), true)
    }

    /// Like [`Store::absorb`], but gives up instead of waiting when another process holds the queue. A panic hook
    /// uses it, since the panicking thread may hold the lock itself.
    pub fn try_absorb(&self, pending: Pending) -> io::Result<()> {
        self.absorb_at(pending, crate::today(), false)
    }

    /// Like [`Store::absorb`], but only while the profile's install id is still `id`. A process that recorded under one
    /// id uses it, so its sums are dropped instead of moving to the new id after a reset, or after off and on.
    pub fn absorb_as(&self, pending: Pending, id: &InstallId) -> io::Result<()> {
        self.absorb_for(pending, crate::today(), true, Some(id))
    }

    pub(crate) fn absorb_at(&self, pending: Pending, today: u32, wait: bool) -> io::Result<()> {
        self.absorb_for(pending, today, wait, None)
    }

    /// Adds `pending` while the profile has an install id, and only when it is `expected` if that is given. Without an
    /// id telemetry was turned off, and what was recorded before must not be kept.
    fn absorb_for(
        &self,
        pending: Pending,
        today: u32,
        wait: bool,
        expected: Option<&InstallId>,
    ) -> io::Result<()> {
        if pending.days.values().all(DailyReport::is_empty) && pending.events.is_empty() {
            return Ok(());
        }
        let _lock = self.lock(wait)?;
        match (self.settings()?.install_id, expected) {
            (None, _) => return Ok(()),
            (Some(id), Some(expected)) if &id != expected => return Ok(()),
            _ => {}
        }
        let mut queue = self.queue()?;
        queue.put(pending.days.into_values(), pending.events);
        queue.trim(today);
        write(&self.dir.join("queue.json"), &queue)
    }

    /// Hands out the batch an earlier send left out, unchanged but for what expired. Without one, it marks the reports
    /// of days before `today` and every event as out and hands them over, each with a new uuid. None while the profile
    /// has no id, or while another send has a batch out. The caller settles it with [`Store::sent`] or
    /// [`Store::give_back`].
    pub fn take(&self, today: u32) -> io::Result<Option<Sending>> {
        let sending = match self.send_lock() {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        };
        let _lock = self.lock(true)?;
        let Some(id) = self.settings()?.install_id else {
            return Ok(None);
        };
        let mut queue = self.queue()?;
        let held = (queue.days.len(), queue.events.len(), queue.out.is_some());
        queue.trim(today);
        let trimmed = held != (queue.days.len(), queue.events.len(), queue.out.is_some());
        let left = queue.out.take().filter(|out| out.id == id);
        let out = match left {
            Some(out) => out,
            None => {
                let later = queue.days.split_off(&today);
                let batch = Batch {
                    days: std::mem::replace(&mut queue.days, later)
                        .into_values()
                        .collect(),
                    events: std::mem::take(&mut queue.events),
                };
                Out::new(id.clone(), batch)?
            }
        };
        if !out.batch.is_empty() {
            queue.out = Some(out.clone());
        }
        if trimmed || held.2 || queue.out.is_some() {
            write(&self.dir.join("queue.json"), &queue)?;
        }
        Ok(Some(Sending {
            id,
            batch: out.batch,
            uuids: out.uuids,
            _lock: sending,
        }))
    }

    /// Removes a batch the endpoint accepted, or refused for good.
    pub fn sent(&self, sending: Sending) -> io::Result<()> {
        self.settle(sending, true)
    }

    /// Leaves a batch that was not sent in the queue, for the next take to send again as it is. It is dropped when
    /// the profile's install id is no longer the batch's: telemetry was turned off or reset while the batch was out,
    /// so it must not be sent again, under the old id or a new one.
    pub fn give_back(&self, sending: Sending) -> io::Result<()> {
        self.settle(sending, false)
    }

    /// Touches the queue's batch only when it is the one `sending` took, so a [`Sending`] of another profile
    /// changes nothing.
    fn settle(&self, sending: Sending, sent: bool) -> io::Result<()> {
        let _lock = self.lock(true)?;
        let id = self.settings()?.install_id;
        let mut queue = self.queue()?;
        let ours = (queue.out.as_ref()).is_some_and(|out| {
            out.id == sending.id && out.uuids.iter().all(|uuid| sending.uuids.contains(uuid))
        });
        if !ours || (!sent && id.as_ref() == Some(&sending.id)) {
            return Ok(());
        }
        queue.out = None;
        write(&self.dir.join("queue.json"), &queue)
    }

    /// Everything in the queue, today and a batch that is out included, without taking it.
    pub fn peek(&self) -> io::Result<Batch> {
        let mut queue = self.queue()?;
        if let Some(out) = queue.out.take() {
            queue.put(out.batch.days, out.batch.events);
        }
        Ok(Batch {
            days: queue.days.into_values().collect(),
            events: queue.events,
        })
    }

    /// The queue. One that cannot be parsed, such as one a newer build wrote or one a crash left empty, is dropped,
    /// so it cannot stop every later write. Any other read error is returned, so the file is not overwritten.
    fn queue(&self) -> io::Result<Queue> {
        match read(&self.dir.join("queue.json")) {
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                tracing::debug!(%error, "dropping a telemetry queue that cannot be parsed");
                Ok(Queue::default())
            }
            read => read,
        }
    }

    fn lock(&self, wait: bool) -> io::Result<File> {
        self.flock("lock", wait)
    }

    /// Held by a send from its take until it settles, never while the queue's lock is waited for elsewhere.
    fn send_lock(&self) -> io::Result<File> {
        self.flock("send.lock", false)
    }

    /// Creates the directory, and any missing parent, readable by the owner alone.
    fn flock(&self, name: &str, wait: bool) -> io::Result<File> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join(name))?;
        let operation = if wait {
            FlockOperation::LockExclusive
        } else {
            FlockOperation::NonBlockingLockExclusive
        };
        flock(&file, operation)?;
        Ok(file)
    }
}

fn read<T: Default + for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error),
    }
}

/// Writes through a temporary file and a rename, so a crash leaves the old file or the new one.
fn write<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(value)?)?;
    fs::rename(&temporary, path)
}

fn remove(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Recorder;
    use crate::schema::{Command, Counter, SourceFile};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        (dir, store)
    }

    fn panic_on(day: u32) -> Event {
        Event::Panic {
            day,
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 7,
        }
    }

    fn pending(day: u32, status: u64) -> Pending {
        let recorder = Recorder::on();
        recorder.count_on(day, Counter::Command(Command::Status), status);
        recorder.event(panic_on(day));
        recorder.take()
    }

    #[test]
    fn a_new_profile_was_never_asked() {
        let (_dir, store) = store();
        assert_eq!(store.settings().unwrap(), Settings::default());
    }

    #[test]
    fn turning_it_on_draws_an_id_once_and_off_deletes_the_id_and_the_queue() {
        let (_dir, store) = store();
        let on = store.set(true).unwrap();
        let id = on.install_id.clone().unwrap();
        assert_eq!(store.set(true).unwrap().install_id, Some(id.clone()));
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(!store.peek().unwrap().is_empty());

        let off = store.set(false).unwrap();
        assert_eq!(
            off,
            Settings {
                enabled: Some(false),
                install_id: None
            }
        );
        assert!(store.peek().unwrap().is_empty());
        assert_ne!(store.set(true).unwrap().install_id, Some(id));
    }

    #[test]
    fn a_broken_settings_file_does_not_stop_a_choice() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        fs::write(store.dir.join("settings.json"), b"{").unwrap();
        assert!(store.settings().is_err());
        assert_eq!(store.set(false).unwrap().enabled, Some(false));
        assert!(store.peek().unwrap().is_empty());
        assert_eq!(store.settings().unwrap().install_id, None);
    }

    #[test]
    fn reset_draws_a_new_id_and_empties_the_queue() {
        let (_dir, store) = store();
        assert_eq!(store.reset().unwrap(), None);
        let id = store.set(true).unwrap().install_id.unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        let new = store.reset().unwrap().unwrap();
        assert_ne!(new, id);
        assert_eq!(store.settings().unwrap().install_id, Some(new));
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn sums_from_several_processes_add_up() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 2), 100, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(queued.days.len(), 1);
        assert_eq!(queued.days[0].counters["command"]["status"], 3);
        assert_eq!(queued.events.len(), 2);
    }

    fn days(queue: &Queue) -> Vec<u32> {
        queue.days.keys().copied().collect()
    }

    #[test]
    fn take_leaves_today_and_give_back_restores_the_rest() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 5), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert_eq!(
            (sending.batch.days.iter())
                .map(|report| report.day)
                .collect::<Vec<_>>(),
            [99]
        );
        assert_eq!(sending.batch.events.len(), 2);
        let left = store.queue().unwrap();
        assert_eq!(days(&left), [100]);
        assert!(left.events.is_empty());
        assert_eq!(left.out.unwrap().batch, sending.batch);
        assert_eq!(store.peek().unwrap().days.len(), 2);

        let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
        assert_eq!(uuids.len(), 3);
        store.give_back(sending).unwrap();
        store.absorb_at(pending(99, 4), 100, true).unwrap();
        let again = store.take(101).unwrap().unwrap();
        assert_eq!((again.batch, again.uuids), (batch, uuids));
    }

    #[test]
    fn take_drops_what_expired() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        assert!(
            store
                .take(99 + KEEP_DAYS + 1)
                .unwrap()
                .unwrap()
                .batch
                .is_empty()
        );
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        assert!(
            store
                .take(99 + KEEP_DAYS + 1)
                .unwrap()
                .unwrap()
                .batch
                .is_empty()
        );
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn a_take_with_nothing_due_still_drops_what_expired() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let late = 99 + KEEP_DAYS + 1;
        drop(store.take(late).unwrap());
        assert!(store.queue().unwrap().days.is_empty());
        assert!(store.queue().unwrap().events.is_empty());
    }

    #[test]
    fn an_expired_batch_out_makes_no_room_for_itself() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        let late = 99 + KEEP_DAYS + 1;
        store.absorb_at(pending(late, 1), late, true).unwrap();
        let queue = store.queue().unwrap();
        assert!(queue.out.is_none());
        assert_eq!(queue.events.len(), 1);
    }

    #[test]
    fn events_out_count_toward_the_cap() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        let panics = |n| Pending {
            days: BTreeMap::new(),
            events: vec![panic_on(99); n],
        };
        store
            .absorb_at(panics(Event::PER_KIND_PER_DAY), 100, true)
            .unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.give_back(sending).unwrap();
        store.absorb_at(panics(5), 100, true).unwrap();
        assert!(store.queue().unwrap().events.is_empty());
    }

    #[test]
    fn a_batch_partly_expired_while_out_still_settles() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(98, 1), 100, true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert_eq!(sending.batch.days.len(), 2);
        store
            .absorb_at(pending(100, 1), 98 + KEEP_DAYS + 1, true)
            .unwrap();
        assert_eq!(store.queue().unwrap().out.unwrap().batch.days.len(), 1);
        store.sent(sending).unwrap();
        assert!(store.queue().unwrap().out.is_none());
    }

    #[test]
    fn a_sending_of_another_profile_settles_nothing() {
        let (_dir, store) = store();
        let (_other_dir, other) = self::store();
        for store in [&store, &other] {
            store.set(true).unwrap();
            store.absorb_at(pending(99, 1), 100, true).unwrap();
        }
        let theirs = other.take(100).unwrap().unwrap();
        let _ours = store.take(100).unwrap().unwrap();
        store.sent(theirs).unwrap();
        assert!(store.queue().unwrap().out.is_some());
    }

    #[test]
    fn a_sent_batch_leaves_the_queue() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.sent(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn one_send_at_a_time_and_a_dropped_one_loses_nothing() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        assert!(store.take(100).unwrap().is_none());
        let (batch, uuids) = (sending.batch.clone(), sending.uuids.clone());
        drop(sending);

        store.absorb_at(pending(99, 2), 100, true).unwrap();
        let again = store.take(100).unwrap().unwrap();
        assert_eq!((&again.batch, &again.uuids), (&batch, &uuids));
        store.sent(again).unwrap();
        let next = store.take(100).unwrap().unwrap();
        assert_eq!(next.batch.days[0].counters["command"]["status"], 2);
        assert_ne!(next.uuids, uuids);
    }

    #[test]
    fn a_batch_out_while_telemetry_is_reset_or_turned_off_is_not_given_back() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.reset().unwrap();
        store.give_back(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());

        store.absorb_at(pending(99, 1), 100, true).unwrap();
        let sending = store.take(100).unwrap().unwrap();
        store.set(false).unwrap();
        store.give_back(sending).unwrap();
        assert!(store.peek().unwrap().is_empty());
        assert!(store.take(100).unwrap().is_none());
        assert_eq!(store.settings().unwrap().install_id, None);

        let id = store.set(true).unwrap().install_id.unwrap();
        let new = store.reset().unwrap().unwrap();
        store
            .absorb_for(pending(99, 1), 100, true, Some(&id))
            .unwrap();
        assert!(store.peek().unwrap().is_empty());
        store
            .absorb_for(pending(99, 1), 100, true, Some(&new))
            .unwrap();
        assert!(!store.peek().unwrap().is_empty());
    }

    #[test]
    fn nothing_is_kept_without_an_install_id() {
        let (_dir, store) = store();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(store.peek().unwrap().is_empty());
        store.set(true).unwrap();
        store.set(false).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert!(store.peek().unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_queue_is_dropped_instead_of_stopping_later_writes() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        fs::write(store.dir.join("queue.json"), b"").unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        assert_eq!(store.peek().unwrap().days.len(), 1);
    }

    #[test]
    fn a_queue_that_cannot_be_read_is_left_alone() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        fs::create_dir(store.dir.join("queue.json")).unwrap();
        assert!(store.absorb_at(pending(100, 1), 100, true).is_err());
        assert!(store.dir.join("queue.json").is_dir());
    }

    #[test]
    fn the_directory_is_the_owner_s_alone() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, store) = store();
        store.set(true).unwrap();
        let mode = fs::metadata(&store.dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn old_days_and_events_are_dropped() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        store.absorb_at(pending(80, 1), 80, true).unwrap();
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(
            queued
                .days
                .iter()
                .map(|report| report.day)
                .collect::<Vec<_>>(),
            [100]
        );
        assert!(queued.events.iter().all(|event| event.day() == 100));
    }

    #[test]
    fn the_queue_holds_at_most_20_events_of_a_kind_a_day() {
        let (_dir, store) = store();
        store.set(true).unwrap();
        for _ in 0..30 {
            store
                .absorb_at(
                    Pending {
                        days: BTreeMap::new(),
                        events: vec![panic_on(100)],
                    },
                    100,
                    true,
                )
                .unwrap();
        }
        assert_eq!(store.peek().unwrap().events.len(), Event::PER_KIND_PER_DAY);
    }

    #[test]
    fn a_busy_queue_is_skipped_instead_of_waited_for() {
        let (_dir, store) = store();
        let _held = store.lock(true).unwrap();
        let error = store.absorb_at(pending(100, 1), 100, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}
