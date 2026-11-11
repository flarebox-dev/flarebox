// SPDX-License-Identifier: Apache-2.0

//! The order of the change stream (spec v0 section 7.3, RFC 0002 section 5): records and this device's commit
//! results are passed on only in contiguous seq order. Free of I/O.

use std::collections::BTreeMap;

use koidrive_proto::v1::{CaughtUp, CommitRecord, Resume, ResyncRequired};

use crate::event::{Committed, Event};

/// The last seq passed on, in its history, with the commit token there (RFC 0013). The token always moves with the
/// seq, so a `Resume` never names a token at another seq.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cursor {
    pub history_id: String,
    pub seq: u64,
    pub commit_id: String,
    pub committed_at_ms: i64,
}

impl Cursor {
    /// Moves to `seq` with its commit token. A token with no time is unknown, as in the result of a server from before
    /// RFC 0013, so the cursor then holds none and may take the head's from a `CaughtUp`.
    fn advance(&mut self, seq: u64, commit_id: &str, committed_at_ms: i64) {
        self.seq = seq;
        self.commit_id = match committed_at_ms {
            0 => String::new(),
            _ => commit_id.to_owned(),
        };
        self.committed_at_ms = committed_at_ms;
    }
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
    /// Starts the stream after `since_seq`, the last seq the driver applied completely, with its commit token.
    pub fn subscribe(&mut self, since: Resume) -> Resume {
        self.cursor = Some(Cursor {
            history_id: since.history_id,
            seq: since.since_seq,
            commit_id: since.since_commit_id,
            committed_at_ms: since.since_committed_at_ms,
        });
        self.held.clear();
        self.resuming = true;
        self.resume_message().expect("just subscribed")
    }

    /// The `Resume` a new connection sends, if the stream is subscribed. Results held from the previous connection
    /// are dropped, so every event after the new `Connected` comes from the new connection: the server sends their
    /// records again from the cursor, their callers got them already, and a server that lost them must not release
    /// them with records of its own.
    pub fn reconnected(&mut self) -> Option<Resume> {
        self.held.clear();
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
            cursor.advance(record.seq, &record.commit_id, record.committed_at_ms);
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
                if seq == cursor.seq + 1 {
                    let at = committed.result.committed_at_ms;
                    cursor.advance(seq, &committed.commit_id, at);
                }
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

    /// The backlog is delivered. A cursor at the head with no token takes the head's.
    pub fn caught_up(&mut self, caught_up: CaughtUp) -> Fed {
        let mut fed = Fed::default();
        self.resuming = false;
        match &mut self.cursor {
            Some(cursor) if cursor.seq < caught_up.head_seq => self.gap(&mut fed),
            Some(cursor) => {
                if cursor.seq == caught_up.head_seq && cursor.commit_id.is_empty() {
                    let (id, at) = (&caught_up.head_commit_id, caught_up.head_committed_at_ms);
                    cursor.advance(caught_up.head_seq, id, at);
                }
                fed.events.push(Event::CaughtUp(caught_up));
            }
            None => fed.events.push(Event::CaughtUp(caught_up)),
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
            if committed.result.seq == cursor.seq + 1 {
                let at = committed.result.committed_at_ms;
                cursor.advance(committed.result.seq, &committed.commit_id, at);
            }
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
            since_commit_id: cursor.commit_id.clone(),
            since_committed_at_ms: cursor.committed_at_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use koidrive_proto::v1::CommitResult;

    use super::*;

    fn record(seq: u64) -> CommitRecord {
        CommitRecord {
            seq,
            commit_id: format!("c{seq}"),
            committed_at_ms: seq as i64 * 10,
            ..Default::default()
        }
    }

    /// A `Resume` after `seq`, with the token of `record(seq)`.
    fn since(seq: u64) -> Resume {
        Resume {
            history_id: "h".into(),
            since_seq: seq,
            since_commit_id: format!("c{seq}"),
            since_committed_at_ms: seq as i64 * 10,
        }
    }

    fn head(seq: u64) -> CaughtUp {
        CaughtUp {
            head_seq: seq,
            head_commit_id: format!("c{seq}"),
            head_committed_at_ms: seq as i64 * 10,
        }
    }

    fn committed(seq: u64) -> Committed {
        Committed {
            commit_id: format!("own{seq}"),
            result: CommitResult {
                seq,
                committed_at_ms: seq as i64 * 10,
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
                Event::CaughtUp(c) => vec![format!("caught_up{}", c.head_seq)],
                other => vec![format!("{other:?}")],
            })
            .collect()
    }

    fn subscribed(seq: u64) -> Stream {
        let mut stream = Stream::default();
        stream.subscribe(since(seq));
        stream
    }

    #[test]
    fn subscribe_resumes_from_the_given_seq() {
        let mut stream = Stream::default();
        assert_eq!(stream.subscribe(since(7)), since(7));
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
        stream.caught_up(head(2));
        let fed = stream.changes(vec![record(3), record(5), record(6)]);
        assert_eq!(seqs(&fed), ["r3"]);
        assert_eq!(fed.resume, Some(since(3)));
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
        stream.caught_up(head(4));
        assert_eq!(seqs(&stream.changes(vec![record(5)])), ["r5"]);
        assert_eq!(seqs(&stream.committed(committed(6))), ["own6"]);
        assert_eq!(seqs(&stream.changes(vec![record(7)])), ["r7"]);
    }

    #[test]
    fn a_result_ahead_of_missing_seqs_is_held_until_they_arrive() {
        let mut stream = subscribed(4);
        stream.caught_up(head(4));
        let fed = stream.committed(committed(7));
        assert!(fed.events.is_empty());
        assert_eq!(fed.resume, Some(since(4)));
        let replay = stream.changes(vec![record(5), record(6), record(7), record(8)]);
        assert_eq!(
            seqs(&replay),
            ["r5", "r6", "own7", "r8"],
            "the replayed record 7 is the same commit"
        );
        assert_eq!(seqs(&stream.caught_up(head(8))), ["caught_up8"]);
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
        let fed = stream.caught_up(head(6));
        assert!(fed.events.is_empty());
        assert_eq!(fed.resume, Some(since(4)));
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
        assert_eq!(stream.reconnected(), Some(since(3)));
    }

    #[test]
    fn a_reconnect_drops_results_held_from_the_previous_connection() {
        let mut stream = subscribed(1);
        stream.caught_up(head(1));
        assert!(stream.committed(committed(3)).events.is_empty());
        stream.reconnected();
        assert_eq!(
            seqs(&stream.changes(vec![record(2)])),
            ["r2"],
            "a restored server's record 2 does not release the old connection's result 3"
        );
        assert_eq!(seqs(&stream.caught_up(head(2))), ["caught_up2"]);
    }

    #[test]
    fn the_token_moves_with_records_and_own_results() {
        let mut stream = subscribed(4);
        stream.caught_up(head(4));
        stream.committed(committed(5));
        assert_eq!(
            stream.reconnected(),
            Some(Resume {
                since_commit_id: "own5".into(),
                ..since(5)
            })
        );
        stream.changes(vec![record(5), record(6)]);
        assert_eq!(stream.reconnected(), Some(since(6)));
    }

    #[test]
    fn caught_up_at_the_cursor_gives_a_missing_token_and_keeps_a_known_one() {
        let mut stream = Stream::default();
        stream.subscribe(Resume {
            history_id: "h".into(),
            since_seq: 4,
            ..Default::default()
        });
        assert_eq!(seqs(&stream.caught_up(head(4))), ["caught_up4"]);
        assert_eq!(stream.reconnected(), Some(since(4)));
        let other = CaughtUp {
            head_commit_id: "other".into(),
            ..head(4)
        };
        stream.caught_up(other);
        assert_eq!(stream.reconnected(), Some(since(4)));
    }
    /// A result with no time, from a server before RFC 0013, leaves the token unknown, and the next
    /// `CaughtUp` at that seq supplies it, also for a result held until the records before it came.
    #[test]
    fn a_result_without_a_time_takes_its_token_from_caught_up() {
        for held in [false, true] {
            let mut stream = subscribed(4);
            stream.caught_up(head(4));
            let seq = if held { 7 } else { 5 };
            let mut own = committed(seq);
            own.result.committed_at_ms = 0;
            stream.committed(own);
            if held {
                stream.changes(vec![record(5), record(6)]);
            }
            stream.caught_up(CaughtUp {
                head_commit_id: format!("own{seq}"),
                ..head(seq)
            });
            assert_eq!(
                stream.reconnected(),
                Some(Resume {
                    since_commit_id: format!("own{seq}"),
                    ..since(seq)
                }),
                "held={held}"
            );
        }
    }
}
