// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use koidrive_engine::tree::NodeId;
use koidrive_proto::v1::{Commit, op};
use tokio::sync::watch;

use crate::lifecycle::Lifecycle;
use crate::status::Progress;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Direction {
    Upload,
    Download,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Item {
    pub direction: Direction,
    pub node: NodeId,
    pub digest: Vec<u8>,
}

struct Entry {
    size: u64,
    done: u64,
    credited: HashSet<Vec<u8>>,
    path: Option<String>,
}

struct Sink {
    status: Arc<Lifecycle>,
    progressed: Arc<watch::Sender<()>>,
}

#[derive(Default)]
pub(crate) struct Tracker {
    entries: BTreeMap<Item, Entry>,
    uploads: u64,
    downloads: u64,
    done: u64,
    total: u64,
    finished: u64,
    current: Option<Item>,
    sink: Option<Sink>,
}

impl Tracker {
    pub fn publish_to(&mut self, status: Arc<Lifecycle>, progressed: Arc<watch::Sender<()>>) {
        self.sink = Some(Sink { status, progressed });
        self.publish();
    }

    pub fn progress(&self) -> Option<Progress> {
        if self.entries.is_empty() {
            return None;
        }
        Some(Progress {
            files_to_upload: self.uploads,
            files_to_download: self.downloads,
            bytes_done: self.finished + self.done,
            bytes_total: self.finished + self.total,
            current: (self.current.as_ref())
                .and_then(|item| self.entries.get(item))
                .and_then(|entry| entry.path.clone()),
        })
    }

    pub fn track(&mut self, wanted: BTreeMap<Item, u64>, synced: impl Fn(&Item) -> bool) -> bool {
        let shown = !self.entries.is_empty();
        let gone: Vec<Item> = (self.entries.keys())
            .filter(|item| !wanted.contains_key(*item))
            .cloned()
            .collect();
        for item in gone {
            let entry = self.remove(&item);
            self.finished += match synced(&item) {
                true => entry.size,
                false => entry.done,
            };
        }
        for (item, size) in wanted {
            if self.entries.contains_key(&item) {
                continue;
            }
            match item.direction {
                Direction::Upload => self.uploads += 1,
                Direction::Download => self.downloads += 1,
            }
            self.total += size;
            let entry = Entry {
                size,
                done: 0,
                credited: HashSet::new(),
                path: None,
            };
            self.entries.insert(item, entry);
        }
        if self.entries.is_empty() {
            self.finished = 0;
        }
        self.publish();
        shown != !self.entries.is_empty()
    }

    pub fn clear(&mut self) -> bool {
        self.track(BTreeMap::new(), |_| false)
    }

    fn remove(&mut self, item: &Item) -> Entry {
        let entry = self.entries.remove(item).expect("the entry is tracked");
        match item.direction {
            Direction::Upload => self.uploads -= 1,
            Direction::Download => self.downloads -= 1,
        }
        self.done -= entry.done;
        self.total -= entry.size;
        if self.current.as_ref() == Some(item) {
            self.current = None;
        }
        entry
    }

    fn credit(&mut self, item: &Item, chunk: &[u8], bytes: u64, path: Option<&String>) {
        let Some(entry) = self.entries.get_mut(item) else {
            return;
        };
        if !entry.credited.insert(chunk.to_vec()) {
            return;
        }
        if let Some(path) = path {
            entry.path = Some(path.clone());
        }
        let done = entry.done.saturating_add(bytes).min(entry.size);
        self.done += done - entry.done;
        entry.done = done;
        self.current = Some(item.clone());
        self.publish();
    }

    fn publish(&self) {
        let Some(sink) = &self.sink else { return };
        let progress = self.progress();
        let mut status = sink.status.lock().unwrap();
        if status.progress != progress {
            status.progress = progress;
            drop(status);
            sink.progressed.send_replace(());
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct Credit {
    tracker: Option<Arc<Mutex<Tracker>>>,
    paths: HashMap<NodeId, String>,
    chunks: HashMap<Vec<u8>, Vec<(Item, u64)>>,
}

impl Credit {
    pub fn upload(
        tracker: &Arc<Mutex<Tracker>>,
        commit: &Commit,
        path_of: impl Fn(&str) -> Option<PathBuf>,
    ) -> Credit {
        let mut credit = Credit::download(tracker, []);
        for op in &commit.ops {
            let (node, content) = match &op.op {
                Some(op::Op::CreateFile(create)) => (&create.node_id, &create.content),
                Some(op::Op::UpdateContent(update)) => (&update.node_id, &update.content),
                _ => continue,
            };
            let Some(content) = content else { continue };
            if let Some(path) = path_of(node) {
                credit
                    .paths
                    .insert(node.clone(), path.to_string_lossy().into_owned());
            }
            let item = Item {
                direction: Direction::Upload,
                node: node.clone(),
                digest: content.manifest_digest.clone(),
            };
            for chunk in &content.chunks {
                let holders = credit.chunks.entry(chunk.hash.clone()).or_default();
                match holders.iter_mut().find(|(held, _)| *held == item) {
                    Some((_, bytes)) => *bytes += u64::from(chunk.size),
                    None => holders.push((item.clone(), u64::from(chunk.size))),
                }
            }
        }
        credit
    }

    pub fn download<'a>(
        tracker: &Arc<Mutex<Tracker>>,
        paths: impl IntoIterator<Item = (&'a str, PathBuf)>,
    ) -> Credit {
        Credit {
            tracker: Some(tracker.clone()),
            paths: (paths.into_iter())
                .map(|(node, path)| (node.to_owned(), path.to_string_lossy().into_owned()))
                .collect(),
            chunks: HashMap::new(),
        }
    }

    pub fn chunk(&self, hash: &[u8]) {
        let Some(tracker) = &self.tracker else { return };
        let mut tracker = tracker.lock().unwrap();
        for (item, bytes) in self.chunks.get(hash).into_iter().flatten() {
            tracker.credit(item, hash, *bytes, self.paths.get(&item.node));
        }
    }

    pub fn item(&self, item: &Item, hash: &[u8], bytes: u64) {
        let Some(tracker) = &self.tracker else { return };
        let path = self.paths.get(&item.node);
        tracker.lock().unwrap().credit(item, hash, bytes, path);
    }
}
