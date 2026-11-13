// SPDX-License-Identifier: Apache-2.0

//! How the work of a first sync grows with its files: the driver's loop, which plans one effect, does it, and ends
//! each step with [`Engine::checkpoint`] in a store transaction, run until every remote file is on an empty disk.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use koidrive_engine::tree::FileIdentity;
use koidrive_proto::v1::{Content, NodeKind, NodeState, NodeStatus, VersionInfo};
use koidrive_store::{Error, Store};
use rustix::time::{ClockId, clock_gettime};

use super::{Engine, remote_node};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

struct Counting;

thread_local! {
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes as u64));
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size.saturating_sub(layout.size()));
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn allocated() -> u64 {
    ALLOCATED.with(Cell::get)
}

fn cpu() -> Duration {
    Duration::try_from(clock_gettime(ClockId::ThreadCPUTime)).unwrap()
}

fn remote(id: String, parent: &str, name: String, kind: NodeKind) -> NodeState {
    let current = (kind == NodeKind::File).then(|| VersionInfo {
        content: Some(Content {
            size: 3,
            manifest_digest: vec![7; 32],
            ..Default::default()
        }),
        ..Default::default()
    });
    NodeState {
        node_id: id,
        parent_id: parent.into(),
        name,
        kind: kind as i32,
        status: NodeStatus::Live as i32,
        content_rev: 1,
        meta_rev: 1,
        current,
        ..Default::default()
    }
}

/// The work of the loop.
struct Work {
    cpu: Duration,
    allocated: u64,
}

/// Downloads `files` remote files, `per_dir` to a folder, onto an empty disk, in one transaction of a store on disk.
fn first_sync(files: usize, per_dir: usize) -> Work {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("store.sqlite")).unwrap();
    store
        .transaction(|tx| -> Result<Work, Error> {
            let mut engine = Engine::load(tx, ROOT)?;
            engine.trees.rules_scanned();
            let dirs = files.div_ceil(per_dir);
            let folders = (0..dirs).map(|d| {
                remote(
                    format!("d{d:06}"),
                    ROOT,
                    format!("folder {d}"),
                    NodeKind::Dir,
                )
            });
            let parent = |f: usize| {
                let group = f / per_dir;
                format!("d{group:06}")
            };
            let nodes = (0..files).map(|f| {
                remote(
                    format!("f{f:06}"),
                    &parent(f),
                    format!("file {f}"),
                    NodeKind::File,
                )
            });
            for state in folders.chain(nodes) {
                tx.put_remote_node(&state)?;
                let (id, node) = remote_node(&state).unwrap();
                engine.trees.set_remote(id, node);
            }
            engine.checkpoint(tx)?;
            let (started, before) = (cpu(), allocated());
            let mut planned = 0;
            loop {
                let Engine {
                    trees,
                    effects,
                    ids,
                    ..
                } = &mut engine;
                let Some(record) = effects.plan(trees, None, ids, NOW).cloned() else {
                    break;
                };
                engine.checkpoint(tx)?;
                planned += 1;
                let identity = FileIdentity {
                    dev: 1,
                    ino: planned as u128,
                    birth_ns: Some(0),
                };
                (engine
                    .effects
                    .done(&mut engine.trees, None, &record.effect_id, Some(identity)))
                .unwrap();
                engine.checkpoint(tx)?;
            }
            let work = Work {
                cpu: cpu() - started,
                allocated: allocated() - before,
            };
            assert!(engine.trees.dirty().is_empty(), "{:?}", engine.trees.work());
            assert_eq!(planned, files + dirs);
            assert_eq!(engine.trees.local().len(), files + dirs);
            let loaded = Engine::load(tx, ROOT)?;
            assert_eq!(loaded.trees.state(), engine.trees.state());
            assert_eq!(loaded.effects.state(), engine.effects.state());
            Ok(work)
        })
        .unwrap()
}

#[test]
fn a_first_sync_with_its_checkpoints_takes_work_in_proportion_to_its_files() {
    for per_dir in [100, 4000] {
        let runs = |files| -> Vec<Work> { (0..3).map(|_| first_sync(files, per_dir)).collect() };
        let (small, large) = (runs(500), runs(2000));
        let bytes = large[0].allocated as f64 / small[0].allocated as f64;
        let least = |runs: &[Work]| runs.iter().map(|work| work.cpu).min().unwrap();
        let time = least(&large).as_secs_f64() / least(&small).as_secs_f64();
        eprintln!(
            "{per_dir} per folder: 500 files allocated {} bytes in {:?}, 2000 files {} bytes in {:?}",
            small[0].allocated,
            least(&small),
            large[0].allocated,
            least(&large),
        );
        assert!(
            bytes < 8.0,
            "{per_dir} per folder: 4 times the files allocated {bytes:.1} times the bytes"
        );
        assert!(
            time < 8.0,
            "{per_dir} per folder: 4 times the files used {time:.1} times the thread CPU time"
        );
    }
}

#[test]
fn the_first_sync_clock_excludes_time_off_cpu() {
    let wall = Instant::now();
    let started = cpu();
    std::thread::sleep(Duration::from_millis(20));
    let used = cpu() - started;
    let elapsed = wall.elapsed();
    assert!(used < elapsed / 2, "thread CPU={used:?}, wall={elapsed:?}");
}
