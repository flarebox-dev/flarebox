// SPDX-License-Identifier: Apache-2.0

//! The files of telemetry under the profile's state directory, which belongs to one machine and is never copied
//! (`koidrive_driver::Paths`): the choice and the install id in `settings.json`, and what waits to be sent in
//! `queue.json`. Every process of the profile writes the queue under `lock`, so the daemon and a command that runs
//! next to it do not lose each other's sums.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};

use crate::consent::{self, Status};
use crate::recorder::Pending;
use crate::schema::{DailyReport, Event, InstallId};

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

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Queue {
    #[serde(default)]
    days: BTreeMap<u32, DailyReport>,
    #[serde(default)]
    events: Vec<Event>,
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
    /// everything waiting to be sent.
    pub fn set(&self, on: bool) -> io::Result<Settings> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
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

    /// Draws a new install id and deletes everything waiting to be sent. Without an id there is nothing to reset.
    pub fn reset(&self) -> io::Result<Option<InstallId>> {
        let _lock = self.lock(true)?;
        let mut settings = self.settings()?;
        if settings.install_id.is_none() {
            return Ok(None);
        }
        let id = InstallId::generate();
        settings.install_id = Some(id.clone());
        write(&self.dir.join("settings.json"), &settings)?;
        remove(&self.dir.join("queue.json"))?;
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

    pub(crate) fn absorb_at(&self, pending: Pending, today: u32, wait: bool) -> io::Result<()> {
        if pending.days.values().all(DailyReport::is_empty) && pending.events.is_empty() {
            return Ok(());
        }
        let _lock = self.lock(wait)?;
        let mut queue: Queue = read(&self.dir.join("queue.json"))?;
        for (day, report) in pending.days {
            queue
                .days
                .entry(day)
                .or_insert_with(|| DailyReport {
                    day,
                    ..DailyReport::default()
                })
                .merge(report);
        }
        for event in pending.events {
            let same = (queue.events.iter())
                .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                .count();
            if same < Event::PER_KIND_PER_DAY {
                queue.events.push(event);
            }
        }
        let oldest = today.saturating_sub(KEEP_DAYS);
        queue
            .days
            .retain(|day, report| *day >= oldest && !report.is_empty());
        queue.events.retain(|event| event.day() >= oldest);
        let over = queue.events.len().saturating_sub(MAX_EVENTS);
        queue.events.drain(..over);
        write(&self.dir.join("queue.json"), &queue)
    }

    /// Takes the reports of days before `today` and every event out of the queue. The caller gives them back with
    /// [`Store::give_back`] if sending fails.
    pub fn take(&self, today: u32) -> io::Result<Batch> {
        let _lock = self.lock(true)?;
        let mut queue: Queue = read(&self.dir.join("queue.json"))?;
        let later = queue.days.split_off(&today);
        let batch = Batch {
            days: std::mem::replace(&mut queue.days, later)
                .into_values()
                .collect(),
            events: std::mem::take(&mut queue.events),
        };
        if !batch.is_empty() {
            write(&self.dir.join("queue.json"), &queue)?;
        }
        Ok(batch)
    }

    /// Puts a batch that was not sent back into the queue, as of `today`.
    pub fn give_back(&self, batch: Batch, today: u32) -> io::Result<()> {
        let pending = Pending {
            days: batch
                .days
                .into_iter()
                .map(|report| (report.day, report))
                .collect(),
            events: batch.events,
        };
        self.absorb_at(pending, today, true)
    }

    /// Everything in the queue, today included, without taking it.
    pub fn peek(&self) -> io::Result<Batch> {
        let queue: Queue = read(&self.dir.join("queue.json"))?;
        Ok(Batch {
            days: queue.days.into_values().collect(),
            events: queue.events,
        })
    }

    fn lock(&self, wait: bool) -> io::Result<File> {
        fs::create_dir_all(&self.dir)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("lock"))?;
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
        store.absorb_at(pending(100, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 2), 100, true).unwrap();
        let queued = store.peek().unwrap();
        assert_eq!(queued.days.len(), 1);
        assert_eq!(queued.days[0].counters["command"]["status"], 3);
        assert_eq!(queued.events.len(), 2);
    }

    #[test]
    fn take_leaves_today_and_give_back_restores_the_rest() {
        let (_dir, store) = store();
        store.absorb_at(pending(99, 1), 100, true).unwrap();
        store.absorb_at(pending(100, 5), 100, true).unwrap();
        let batch = store.take(100).unwrap();
        assert_eq!(
            batch
                .days
                .iter()
                .map(|report| report.day)
                .collect::<Vec<_>>(),
            [99]
        );
        assert_eq!(batch.events.len(), 2);
        let left = store.peek().unwrap();
        assert_eq!(
            left.days
                .iter()
                .map(|report| report.day)
                .collect::<Vec<_>>(),
            [100]
        );
        assert!(left.events.is_empty());

        store.give_back(batch, 100).unwrap();
        assert_eq!(store.peek().unwrap().days.len(), 2);
    }

    #[test]
    fn old_days_and_events_are_dropped() {
        let (_dir, store) = store();
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
