// SPDX-License-Identifier: Apache-2.0

//! The order of the change stream (spec v0 section 7.3, RFC 0002 section 5): records and this device's commit
//! results are passed on only in contiguous seq order. Free of I/O.

use std::collections::BTreeMap;

use flarebox_proto::v1::{CommitRecord, Resume, ResyncRequired};

use crate::event::{Committed, Event};

/// The last seq passed on, in its history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub history_id: String,
    pub seq: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Stream {
    cursor: Option<Cursor>,
    /// Commit results that arrived while earlier seqs were missing, by seq.
    held: BTreeMap<u64, Committed>,
    /// A `Resume` is outstanding, so a gap needs no other one.
    resuming: bool,
}

/// What one server message produced: events in order, and a `Resume` to send when the stream has a gap.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Fed {
    pub events: Vec<Event>,
    pub resume: Option<Resume>,
}

impl Fed {
    fn record(&mut self, record: CommitRecord) {
        match self.events.last_mut() {
            Some(Event::Changes(records)) => records.push(record),
            _ => self.events.push(Event::Changes(vec![record])),
        }
    }
}

impl Stream {
    /// Starts the stream after `since_seq`, the last seq the driver applied completely.
    pub fn subscribe(&mut self, history_id: String, since_seq: u64) -> Resume {
        self.cursor = Some(Cursor {
            history_id,
            seq: since_seq,
        });
        self.held.clear();
        self.resuming = true;
        self.resume_message().expect("just subscribed")
    }

    /// The `Resume` a new connection sends, if the stream is subscribed.
    pub fn reconnected(&mut self) -> Option<Resume> {
        self.resuming = self.cursor.is_some();
        self.resume_message()
    }

    pub fn changes(&mut self, commits: Vec<CommitRecord>) -> Fed {
        let mut fed = Fed::default();
        for record in commits {
            let Some(cursor) = &mut self.cursor else {
                break;
            };
            if record.seq <= cursor.seq {
                continue;
            }
            if record.seq > cursor.seq + 1 {
                self.gap(&mut fed);
                break;
            }
            cursor.seq = record.seq;
            fed.record(record);
            self.release(&mut fed);
        }
        fed
    }

    pub fn committed(&mut self, committed: Committed) -> Fed {
        let mut fed = Fed::default();
        let seq = committed.result.seq;
        match &mut self.cursor {
            None => fed.events.push(Event::Committed(committed)),
            Some(cursor) if seq <= cursor.seq + 1 => {
                cursor.seq = cursor.seq.max(seq);
                fed.events.push(Event::Committed(committed));
                self.release(&mut fed);
            }
            Some(_) => {
                self.held.insert(seq, committed);
                self.gap(&mut fed);
            }
        }
        fed
    }

    pub fn caught_up(&mut self, head_seq: u64) -> Fed {
        let mut fed = Fed::default();
        self.resuming = false;
        match &self.cursor {
            Some(cursor) if cursor.seq < head_seq => self.gap(&mut fed),
            _ => fed.events.push(Event::CaughtUp { head_seq }),
        }
        fed
    }

    /// The server cannot resume from the cursor: stop until the driver subscribes again. Held results are dropped;
    /// their callers still get them, and the driver bootstraps anyway.
    pub fn resync(&mut self, resync: ResyncRequired) -> Fed {
        self.cursor = None;
        self.held.clear();
        self.resuming = false;
        Fed {
            events: vec![Event::ResyncRequired(resync)],
            resume: None,
        }
    }

    fn release(&mut self, fed: &mut Fed) {
        let Some(cursor) = &mut self.cursor else {
            return;
        };
        while let Some(entry) = self.held.first_entry() {
            if *entry.key() > cursor.seq + 1 {
                break;
            }
            let committed = entry.remove();
            cursor.seq = cursor.seq.max(committed.result.seq);
            fed.events.push(Event::Committed(committed));
        }
    }

    fn gap(&mut self, fed: &mut Fed) {
        if !self.resuming {
            self.resuming = true;
            fed.resume = self.resume_message();
        }
    }

    fn resume_message(&self) -> Option<Resume> {
        let cursor = self.cursor.as_ref()?;
        Some(Resume {
            history_id: cursor.history_id.clone(),
            since_seq: cursor.seq,
        })
    }
}

#[cfg(test)]
mod tests {
    use flarebox_proto::v1::CommitResult;

    use super::*;

    fn record(seq: u64) -> CommitRecord {
        CommitRecord {
            seq,
            commit_id: format!("c{seq}"),
            ..Default::default()
        }
    }

    fn committed(seq: u64) -> Committed {
        Committed {
            commit_id: format!("own{seq}"),
            result: CommitResult {
                seq,
                ..Default::default()
            },
        }
    }

    fn seqs(fed: &Fed) -> Vec<String> {
        fed.events
            .iter()
            .flat_map(|event| match event {
                Event::Changes(records) => records.iter().map(|r| format!("r{}", r.seq)).collect(),
                Event::Committed(c) => vec![format!("own{}", c.result.seq)],
                Event::CaughtUp { head_seq } => vec![format!("caught_up{head_seq}")],
                other => vec![format!("{other:?}")],
            })
            .collect()
    }

    fn subscribed(since: u64) -> Stream {
        let mut stream = Stream::default();
        stream.subscribe("h".into(), since);
        stream
    }

    #[test]
    fn subscribe_resumes_from_the_given_seq() {
        let mut stream = Stream::default();
        assert_eq!(
            stream.subscribe("h".into(), 7),
            Resume {
                history_id: "h".into(),
                since_seq: 7
            }
        );
    }

    #[test]
    fn contiguous_records_pass_and_replayed_ones_are_skipped() {
        let mut stream = subscribed(2);
        let fed = stream.changes(vec![record(1), record(2), record(3), record(4)]);
        assert_eq!(seqs(&fed), ["r3", "r4"]);
        assert_eq!(fed.resume, None);
        assert_eq!(seqs(&stream.changes(vec![record(4), record(5)])), ["r5"]);
    }

    #[test]
    fn a_gap_stops_delivery_and_asks_for_one_resume_from_the_cursor() {
        let mut stream = subscribed(2);
        stream.caught_up(2);
        let fed = stream.changes(vec![record(3), record(5), record(6)]);
        assert_eq!(seqs(&fed), ["r3"]);
        assert_eq!(
            fed.resume,
            Some(Resume {
                history_id: "h".into(),
                since_seq: 3
            })
        );
        let again = stream.changes(vec![record(7)]);
        assert_eq!(
            (seqs(&again), again.resume),
            (vec![], None),
            "one Resume is outstanding"
        );
        assert_eq!(
            seqs(&stream.changes(vec![record(4), record(5)])),
            ["r4", "r5"]
        );
    }

    #[test]
    fn an_own_result_takes_its_place_between_records() {
        let mut stream = subscribed(4);
        stream.caught_up(4);
        assert_eq!(seqs(&stream.changes(vec![record(5)])), ["r5"]);
        assert_eq!(seqs(&stream.committed(committed(6))), ["own6"]);
        assert_eq!(seqs(&stream.changes(vec![record(7)])), ["r7"]);
    }

    #[test]
    fn a_result_ahead_of_missing_seqs_is_held_until_they_arrive() {
        let mut stream = subscribed(4);
        stream.caught_up(4);
        let fed = stream.committed(committed(7));
        assert!(fed.events.is_empty());
        assert_eq!(
            fed.resume,
            Some(Resume {
                history_id: "h".into(),
                since_seq: 4
            })
        );
        let replay = stream.changes(vec![record(5), record(6), record(7), record(8)]);
        assert_eq!(
            seqs(&replay),
            ["r5", "r6", "own7", "r8"],
            "the replayed record 7 is the same commit"
        );
        assert_eq!(seqs(&stream.caught_up(8)), ["caught_up8"]);
    }

    #[test]
    fn a_result_whose_record_was_replayed_is_still_reported() {
        let mut stream = subscribed(4);
        stream.changes(vec![record(5), record(6)]);
        assert_eq!(seqs(&stream.committed(committed(5))), ["own5"]);
        assert_eq!(
            seqs(&stream.changes(vec![record(7)])),
            ["r7"],
            "the cursor did not move back"
        );
    }

    #[test]
    fn caught_up_below_the_head_resumes_instead_of_reporting() {
        let mut stream = subscribed(4);
        let fed = stream.caught_up(6);
        assert!(fed.events.is_empty());
        assert_eq!(
            fed.resume,
            Some(Resume {
                history_id: "h".into(),
                since_seq: 4
            })
        );
    }

    #[test]
    fn resync_stops_the_stream_until_the_next_subscribe() {
        let mut stream = subscribed(4);
        stream.committed(committed(9));
        let resync = ResyncRequired {
            reason: 1,
            ..Default::default()
        };
        assert_eq!(
            stream.resync(resync).events,
            vec![Event::ResyncRequired(resync)]
        );
        assert!(
            stream.changes(vec![record(5)]).events.is_empty(),
            "records are dropped while unsubscribed"
        );
        assert_eq!(stream.reconnected(), None);
        assert_eq!(
            seqs(&stream.committed(committed(10))),
            ["own10"],
            "results still reach the driver"
        );
    }

    #[test]
    fn a_reconnect_resumes_from_the_last_seq_passed_on() {
        let mut stream = subscribed(1);
        stream.changes(vec![record(2), record(3)]);
        assert_eq!(
            stream.reconnected(),
            Some(Resume {
                history_id: "h".into(),
                since_seq: 3
            })
        );
    }
}
