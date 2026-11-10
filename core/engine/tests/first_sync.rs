// SPDX-License-Identifier: Apache-2.0

//! How the work of a first sync grows with the number of files (GUN-320, item 4 of the BOX-116
//! audit): the loop of the driver, which settles, takes the changed nodes for its checkpoint,
//! plans one effect and acknowledges it, run until every remote file is on an empty disk.

use std::time::{Duration, Instant};

use flarebox_engine::classify::Trees;
use flarebox_engine::effects::Effects;
use flarebox_engine::ids::IdGen;
use flarebox_engine::tree::{Content, FileIdentity, Kind, RemoteNode, Shape};

const ROOT: &str = "root";
const NOW: u64 = 1_794_268_740_000;

fn remote(parent: &str, name: String, kind: Kind) -> RemoteNode {
    let content = (kind == Kind::File).then(|| Content {
        size: 3,
        manifest_digest: vec![7; 32],
        ..Default::default()
    });
    RemoteNode {
        shape: Shape {
            parent: parent.into(),
            name,
            kind,
            content,
        },
        trashed: false,
        content_rev: 1,
        meta_rev: 1,
        trash_group: String::new(),
    }
}

/// Downloads `files` remote files, `per_dir` to a folder, onto an empty disk. Returns the time
/// the loop took, the effects it planned and the nodes the checkpoints were given to write.
fn first_sync(files: usize, per_dir: usize) -> (Duration, usize, usize) {
    let mut trees = Trees::new(ROOT);
    let dirs = files.div_ceil(per_dir);
    for d in 0..dirs {
        trees.set_remote(
            format!("d{d:06}"),
            remote(ROOT, format!("folder {d}"), Kind::Dir),
        );
    }
    for f in 0..files {
        let parent = format!("d{:06}", f / per_dir);
        trees.set_remote(
            format!("f{f:06}"),
            remote(&parent, format!("file {f}"), Kind::File),
        );
    }
    trees.settle();
    trees.take_changed();
    let (mut effects, mut ids) = (Effects::default(), IdGen::new(1));
    let (mut planned, mut rows) = (0, 0);
    let started = Instant::now();
    loop {
        trees.settle();
        rows += trees.take_changed().len();
        let Some(record) = effects.plan(&trees, &mut ids, NOW).cloned() else {
            break;
        };
        planned += 1;
        let identity = FileIdentity {
            dev: 1,
            ino: planned as u64,
            birth_ns: Some(0),
        };
        effects
            .done(&mut trees, &record.effect_id, Some(identity))
            .unwrap();
    }
    let elapsed = started.elapsed();
    assert!(trees.dirty().is_empty(), "{:?}", trees.work());
    assert_eq!(trees.local().len(), files + dirs);
    (elapsed, planned, rows)
}

/// Each effect changes one node, so the checkpoints write one row per node, and the time per file
/// stays about the same as the count grows. Work that walks every dirty node or every node of L
/// at each step takes four times as long per file at four times the files.
#[test]
fn a_first_sync_takes_time_in_proportion_to_its_files() {
    for per_dir in [100, 4000] {
        let mut times = Vec::new();
        for files in [1000, 2000, 4000] {
            let (elapsed, planned, rows) = first_sync(files, per_dir);
            let nodes = files + files.div_ceil(per_dir);
            assert_eq!(
                (planned, rows),
                (nodes, nodes),
                "{files} files, {per_dir} per folder"
            );
            eprintln!("files={files:5} per_dir={per_dir:4} time={elapsed:?}");
            times.push(elapsed);
        }
        let growth = times[2].as_secs_f64() / times[0].as_secs_f64();
        assert!(
            growth < 8.0,
            "{per_dir} per folder: 4 times the files took {growth:.1} times as long ({times:?})"
        );
    }
}
