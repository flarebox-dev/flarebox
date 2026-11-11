// SPDX-License-Identifier: Apache-2.0

//! The in-memory sums of what a process recorded, per UTC day, until [`Store::absorb`](crate::Store::absorb) moves
//! them to disk.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::schema::{Counter, DailyReport, Event, Histogram, Measure};

/// A cheap handle that code passes around to record. One made with [`Recorder::off`] records nothing and costs one
/// branch per call. Recording takes a short lock on the sums and does no I/O.
#[derive(Clone, Debug, Default)]
pub struct Recorder(Option<Arc<Mutex<Pending>>>);

/// What a recorder holds and [`Recorder::take`] hands over.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pending {
    pub days: BTreeMap<u32, DailyReport>,
    pub events: Vec<Event>,
}

impl Recorder {
    pub fn off() -> Self {
        Recorder(None)
    }

    pub fn on() -> Self {
        Recorder(Some(Arc::default()))
    }

    pub fn is_on(&self) -> bool {
        self.0.is_some()
    }

    pub fn count(&self, counter: Counter, by: u64) {
        self.count_on(crate::today(), counter, by);
    }

    pub fn measure(&self, measure: Measure, value: u64) {
        self.measure_on(crate::today(), measure, value);
    }

    /// Keeps an event, unless [`Event::PER_KIND_PER_DAY`] of its kind are already held for its day.
    pub fn event(&self, event: Event) {
        self.with(|pending| {
            let same = (pending.events.iter())
                .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                .count();
            if same < Event::PER_KIND_PER_DAY {
                pending.events.push(event);
            }
        });
    }

    /// Everything recorded so far, leaving the recorder empty.
    pub fn take(&self) -> Pending {
        let mut taken = Pending::default();
        self.with(|pending| taken = std::mem::take(pending));
        taken
    }

    /// Hands back what a failed write took, so it is not lost.
    pub fn restore(&self, taken: Pending) {
        self.with(|pending| {
            for (day, report) in taken.days {
                pending
                    .days
                    .entry(day)
                    .or_insert_with(|| DailyReport {
                        day,
                        ..DailyReport::default()
                    })
                    .merge(report);
            }
            let mut events = taken.events;
            for event in std::mem::take(&mut pending.events) {
                let same = (events.iter())
                    .filter(|held| held.kind() == event.kind() && held.day() == event.day())
                    .count();
                if same < Event::PER_KIND_PER_DAY {
                    events.push(event);
                }
            }
            pending.events = events;
        });
    }

    pub(crate) fn count_on(&self, day: u32, counter: Counter, by: u64) {
        let (metric, label) = counter.key();
        self.with(|pending| {
            let report = pending.days.entry(day).or_insert_with(|| DailyReport {
                day,
                ..DailyReport::default()
            });
            let sum = report
                .counters
                .entry(metric.to_owned())
                .or_default()
                .entry(label.to_owned())
                .or_default();
            *sum = sum.saturating_add(by);
        });
    }

    pub(crate) fn measure_on(&self, day: u32, measure: Measure, value: u64) {
        let (metric, label, shift) = measure.key();
        self.with(|pending| {
            let report = pending.days.entry(day).or_insert_with(|| DailyReport {
                day,
                ..DailyReport::default()
            });
            let labels = report.histograms.entry(metric.to_owned()).or_default();
            labels
                .entry(label.to_owned())
                .or_insert_with(|| Histogram::new(shift))
                .record(value);
        });
    }

    fn with(&self, update: impl FnOnce(&mut Pending)) {
        if let Some(pending) = &self.0
            && let Ok(mut pending) = pending.lock()
        {
            update(&mut pending);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Command, SourceFile};

    fn panic_on(day: u32) -> Event {
        Event::Panic {
            day,
            file: SourceFile::from_location("cli/src/main.rs"),
            line: 1,
        }
    }

    #[test]
    fn an_off_recorder_holds_nothing() {
        let recorder = Recorder::off();
        recorder.count(Counter::Command(Command::Status), 1);
        recorder.event(panic_on(1));
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn counts_sum_per_day_and_label() {
        let recorder = Recorder::on();
        recorder.count_on(10, Counter::Command(Command::Status), 1);
        recorder.count_on(10, Counter::Command(Command::Status), 2);
        recorder.count_on(11, Counter::Command(Command::Link), 1);
        let taken = recorder.take();
        assert_eq!(taken.days[&10].counters["command"]["status"], 3);
        assert_eq!(taken.days[&11].counters["command"]["link"], 1);
        assert_eq!(recorder.take(), Pending::default());
    }

    #[test]
    fn a_looping_event_is_kept_at_most_20_times_a_day() {
        let recorder = Recorder::on();
        for _ in 0..50 {
            recorder.event(panic_on(5));
        }
        recorder.event(panic_on(6));
        let taken = recorder.take();
        assert_eq!(
            taken.events.iter().filter(|event| event.day() == 5).count(),
            Event::PER_KIND_PER_DAY
        );
        assert_eq!(taken.events.len(), Event::PER_KIND_PER_DAY + 1);
    }

    #[test]
    fn restored_sums_add_to_what_came_after() {
        let recorder = Recorder::on();
        recorder.count_on(3, Counter::Command(Command::Status), 1);
        let taken = recorder.take();
        recorder.count_on(3, Counter::Command(Command::Status), 4);
        recorder.restore(taken);
        assert_eq!(recorder.take().days[&3].counters["command"]["status"], 5);
    }

    #[test]
    fn restoring_keeps_the_event_cap() {
        let recorder = Recorder::on();
        for _ in 0..Event::PER_KIND_PER_DAY {
            recorder.event(panic_on(5));
        }
        for _ in 0..3 {
            let taken = recorder.take();
            for _ in 0..Event::PER_KIND_PER_DAY {
                recorder.event(panic_on(5));
            }
            recorder.restore(taken);
        }
        assert_eq!(recorder.take().events.len(), Event::PER_KIND_PER_DAY);
    }
}
