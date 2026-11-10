// SPDX-License-Identifier: Apache-2.0

//! Two devices syncing one space through the real Worker in `server/`, run by
//! `core/driver/scripts/integration-test.sh`, which starts `wrangler dev` and sets `FLAREBOX_TEST_SERVER` (its URL)
//! and `FLAREBOX_TEST_SERVER_LOG` (its output, where the Worker prints the claim code). Each device is a [`Driver`]
//! with a profile and a folder of its own, and both run in the test's process.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flarebox_chunk::ChunkerConfig;
use flarebox_driver::{Driver, FolderState, FolderStatus, Paths};
use flarebox_net::{Api, Auth, Credentials};
use flarebox_proto::v1::{ClaimRequest, CreateSpaceRequest, SpaceInfo};
use tokio::sync::OnceCell;
use url::Url;

/// How long two devices may take to hold the same tree.
const WAIT: Duration = Duration::from_secs(30);

fn server_url() -> Url {
    let url = std::env::var("FLAREBOX_TEST_SERVER").expect(
        "FLAREBOX_TEST_SERVER is not set; run core/driver/scripts/integration-test.sh, which starts the Worker",
    );
    url.parse().expect("FLAREBOX_TEST_SERVER is a URL")
}

fn api() -> Api {
    Api::local_development(server_url()).unwrap()
}

static OWNER: OnceCell<Credentials> = OnceCell::const_new();

/// The deployment's owner, claimed once for every test in this binary. It creates the spaces and starts the
/// pairings, and it syncs nothing.
async fn owner() -> Arc<Auth> {
    let credentials = OWNER.get_or_init(claim).await.clone();
    Arc::new(Auth::new(api(), credentials))
}

/// Claims the deployment with the code the Worker prints to its log.
async fn claim() -> Credentials {
    let log =
        std::env::var("FLAREBOX_TEST_SERVER_LOG").expect("FLAREBOX_TEST_SERVER_LOG is not set");
    let setup = reqwest::get(server_url().join("setup").unwrap())
        .await
        .unwrap();
    assert!(
        setup.status().is_success(),
        "GET /setup answered {}",
        setup.status()
    );
    let started = Instant::now();
    let code = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(code) = text.lines().rev().find_map(claim_code) {
            break code;
        }
        assert!(started.elapsed() < WAIT, "no claim code in {log}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let request = ClaimRequest {
        code,
        user_name: "Owner".into(),
        device_name: "owner".into(),
        platform: "test".into(),
        setup_attempt: String::new(),
    };
    Credentials::from(&api().claim(&request).await.expect("the claim succeeds"))
}

/// The claim code on a Worker log line that mentions one, such as `ABCD-EFGH-JKLM`.
fn claim_code(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claim code") {
        return None;
    }
    line.split_whitespace()
        .map(|word| word.trim_matches(|c: char| !c.is_ascii_alphanumeric()))
        .rfind(|word| {
            word.contains('-')
                && word.len() >= 8
                && word.split('-').all(|group| {
                    !group.is_empty() && group.chars().all(|c| c.is_ascii_alphanumeric())
                })
        })
        .map(str::to_owned)
}

/// Shows the driver's log lines with a failed test.
fn log() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "flarebox_driver=debug,flarebox_net=info".into());
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}

/// A new space of the owner. Each test syncs a space of its own.
async fn new_space(owner: &Arc<Auth>, name: &str) -> SpaceInfo {
    let request = CreateSpaceRequest { name: name.into() };
    owner.api().create_space(owner, &request).await.unwrap()
}

/// One device: a profile directory, a folder, and the driver that syncs the folder.
struct Device {
    home: tempfile::TempDir,
    driver: Option<Driver>,
}

impl Device {
    /// A new device that the owner's device paired, with an empty folder linked to `space`, syncing.
    async fn linked(owner: &Arc<Auth>, space: &SpaceInfo, name: &str) -> Device {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("folder")).unwrap();
        let mut device = Device { home, driver: None };
        let driver = device.open();
        let pairing = owner.api().pair_start(owner).await.unwrap();
        driver
            .pair(server_url().as_str(), &pairing.code, name)
            .await
            .unwrap();
        driver
            .link(&device.folder(), &space.space_id)
            .await
            .unwrap();
        driver.start();
        device.driver = Some(driver);
        device
    }

    fn open(&self) -> Driver {
        Driver::open(Paths::under(&self.home.path().join("profile"))).unwrap()
    }

    fn folder(&self) -> PathBuf {
        self.home.path().join("folder")
    }

    fn status(&self) -> FolderStatus {
        let driver = self.driver.as_ref().expect("the device is running");
        driver.status().folders.remove(0)
    }

    /// Stops syncing and closes the profile, before the test removes the device's directories.
    async fn stop(&mut self) {
        let driver = self.driver.take().expect("the device is running");
        driver.stop().await;
    }

    /// Stops the driver, closes the profile and starts again from what is on disk.
    async fn restart(&mut self) {
        self.stop().await;
        let driver = self.open();
        driver.start();
        self.driver = Some(driver);
    }
}

#[derive(PartialEq, Eq)]
enum Entry {
    Folder,
    File { executable: bool, bytes: Vec<u8> },
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Entry::Folder => write!(f, "folder"),
            Entry::File { executable, bytes } => {
                let executable = if *executable { ", executable" } else { "" };
                write!(f, "file of {} bytes{executable}", bytes.len())
            }
        }
    }
}

type Tree = BTreeMap<PathBuf, Entry>;

/// Everything under `root` except `.flarebox`, by relative path, with each file's bytes. An error means the folder
/// changed while it was read.
fn tree(root: &Path) -> io::Result<Tree> {
    fn walk(root: &Path, dir: &Path, tree: &mut Tree) -> io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if relative == Path::new(".flarebox") {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() {
                tree.insert(relative, Entry::Folder);
                walk(root, &path, tree)?;
            } else {
                let executable = metadata.permissions().mode() & 0o111 != 0;
                let bytes = fs::read(&path)?;
                tree.insert(relative, Entry::File { executable, bytes });
            }
        }
        Ok(())
    }
    let mut tree = Tree::new();
    walk(root, root, &mut tree)?;
    Ok(tree)
}

/// Waits until both folders hold the same entries with the same bytes and both devices report that nothing is left
/// to sync at the same seq. Returns the tree and the seq.
async fn converged(a: &Device, b: &Device) -> (Tree, u64) {
    let started = Instant::now();
    loop {
        let (in_a, in_b) = (tree(&a.folder()), tree(&b.folder()));
        let (status_a, status_b) = (a.status(), b.status());
        let done = |status: &FolderStatus| {
            status.state == FolderState::UpToDate && status.last_seq == status.head_seq
        };
        let same = matches!((&in_a, &in_b), (Ok(in_a), Ok(in_b)) if in_a == in_b);
        if same && done(&status_a) && done(&status_b) && status_a.last_seq == status_b.last_seq {
            return (in_a.unwrap(), status_a.last_seq.unwrap_or(0));
        }
        assert!(
            started.elapsed() < WAIT,
            "the devices did not converge within {WAIT:?}\nA: {status_a:#?}\n{in_a:#?}\nB: {status_b:#?}\n{in_b:#?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `len` bytes that do not repeat, the same on every run.
fn generated(len: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        bytes.extend_from_slice(&state.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

fn file(bytes: &[u8]) -> Entry {
    Entry::File {
        executable: false,
        bytes: bytes.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_file_created_on_one_device_appears_on_the_other() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "create").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    fs::write(a.folder().join("notes.txt"), b"written on a\n").unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"written on a\n"))]);
    assert_eq!(synced, expected);

    fs::create_dir(b.folder().join("replies")).unwrap();
    fs::write(b.folder().join("replies/reply.txt"), b"written on b\n").unwrap();
    fs::write(b.folder().join("replies/empty"), b"").unwrap();
    let script = b.folder().join("run.sh");
    fs::write(&script, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let (synced, seq) = converged(&a, &b).await;
    let expected = Tree::from([
        ("notes.txt".into(), file(b"written on a\n")),
        ("replies".into(), Entry::Folder),
        ("replies/empty".into(), file(b"")),
        ("replies/reply.txt".into(), file(b"written on b\n")),
        (
            "run.sh".into(),
            Entry::File {
                executable: true,
                bytes: b"#!/bin/sh\n".to_vec(),
            },
        ),
    ]);
    assert_eq!(synced, expected);

    a.restart().await;
    b.restart().await;
    converged(&a, &b).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (after, seq_after) = converged(&a, &b).await;
    assert_eq!(
        (after, seq_after),
        (expected, seq),
        "a restart changes neither the folders nor the space"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_file_of_several_chunks_arrives_with_the_same_bytes() {
    log();
    let bytes = generated(8 << 20);
    let chunks = flarebox_chunk::chunks(bytes.as_slice(), ChunkerConfig::V0).count();
    assert!(chunks > 1, "the file is one chunk");
    let owner = owner().await;
    let space = new_space(&owner, "several chunks").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;

    fs::write(a.folder().join("large.bin"), &bytes).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(synced, Tree::from([("large.bin".into(), file(&bytes))]));
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn an_edit_replaces_the_file_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "edit").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::write(a.folder().join("notes.txt"), b"first\n").unwrap();
    converged(&a, &b).await;

    fs::write(b.folder().join("notes.txt"), b"written in place on b\n").unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"written in place on b\n"))]);
    assert_eq!(synced, expected);

    let saved = a.folder().join(".notes.txt.swp");
    fs::write(&saved, b"saved by rename on a\n").unwrap();
    fs::rename(&saved, a.folder().join("notes.txt")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    let expected = Tree::from([("notes.txt".into(), file(b"saved by rename on a\n"))]);
    assert_eq!(synced, expected);
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_rename_moves_the_file_on_the_other_device() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "rename").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::create_dir_all(a.folder().join("docs/drafts")).unwrap();
    fs::write(a.folder().join("docs/plan.txt"), b"plan\n").unwrap();
    fs::write(a.folder().join("docs/drafts/one.txt"), b"one\n").unwrap();
    fs::write(a.folder().join("top.txt"), b"top\n").unwrap();
    converged(&a, &b).await;

    fs::rename(
        b.folder().join("top.txt"),
        b.folder().join("docs/moved.txt"),
    )
    .unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("docs".into(), Entry::Folder),
            ("docs/drafts".into(), Entry::Folder),
            ("docs/drafts/one.txt".into(), file(b"one\n")),
            ("docs/moved.txt".into(), file(b"top\n")),
            ("docs/plan.txt".into(), file(b"plan\n")),
        ])
    );

    fs::rename(a.folder().join("docs"), a.folder().join("papers")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("papers".into(), Entry::Folder),
            ("papers/drafts".into(), Entry::Folder),
            ("papers/drafts/one.txt".into(), file(b"one\n")),
            ("papers/moved.txt".into(), file(b"top\n")),
            ("papers/plan.txt".into(), file(b"plan\n")),
        ])
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a local Worker; run core/driver/scripts/integration-test.sh"]
async fn a_second_hard_link_arrives_as_a_file_of_its_own() {
    log();
    let owner = owner().await;
    let space = new_space(&owner, "hard link").await;
    let mut a = Device::linked(&owner, &space, "a").await;
    let mut b = Device::linked(&owner, &space, "b").await;
    converged(&a, &b).await;
    fs::write(a.folder().join("one.txt"), b"linked\n").unwrap();
    converged(&a, &b).await;

    fs::hard_link(a.folder().join("one.txt"), a.folder().join("two.txt")).unwrap();
    let (synced, _) = converged(&a, &b).await;
    assert_eq!(
        synced,
        Tree::from([
            ("one.txt".into(), file(b"linked\n")),
            ("two.txt".into(), file(b"linked\n")),
        ])
    );
    a.stop().await;
    b.stop().await;
}
