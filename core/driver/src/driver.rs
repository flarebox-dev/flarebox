// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use flarebox_fs::{Marker, probe_no_replace, read_marker, write_marker};
use flarebox_net::{ApiError, Auth};
use flarebox_proto::v1::{ClaimRequest, DeviceCredentials, PairCompleteRequest, SpaceInfo};
use flarebox_store::{GivenAnswer, Link, Store, StoredQuestion};
use rustix::fs::FlockOperation;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::claim::{self, Owner};
use crate::config::{Config, ConfigError};
use crate::credential::{Credential, CredentialError, api_for};
use crate::folder::FolderTask;
use crate::ignore::{IgnoreInput, Matcher, RuleFile, RuleSource, matcher};
use crate::paths::{Paths, PathsError};
use crate::question::{Answer, Question};
use crate::status::{Account, FolderState, FolderStatus, Status};

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("another Flarebox process is using this profile ({0})")]
    Busy(PathBuf),
    #[error("this device is not paired; run flarebox pair first")]
    NotPaired,
    #[error(
        "this profile is already paired as device {device_id}; a profile holds one device credential"
    )]
    AlreadyPaired { device_id: String },
    #[error("a device name is required")]
    EmptyName,
    #[error("a user name is required")]
    EmptyUserName,
    #[error(transparent)]
    Paths(#[from] PathsError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("the server refused or failed: {0}")]
    Api(#[from] ApiError),
    #[error("no space is called {0:?}")]
    SpaceNotFound(String),
    #[error("several spaces are called {name:?}; name one by id: {}", ids.join(", "))]
    SpaceAmbiguous { name: String, ids: Vec<String> },
    #[error("{0} does not exist")]
    FolderNotFound(PathBuf),
    #[error("{0} is not a folder")]
    NotAFolder(PathBuf),
    #[error(
        "the folder overlaps Flarebox's own files at {0}; link a folder that neither holds nor sits inside it"
    )]
    FolderHoldsProfile(PathBuf),
    #[error("the folder overlaps the linked folder {0}")]
    FolderOverlaps(PathBuf),
    #[error("another Flarebox process is syncing {0}; stop it, or unlink the folder there")]
    FolderInUse(PathBuf),
    #[error(
        "the folder is linked by device {device_id} of {server}; unlink it there first, or remove its .flarebox/device.json"
    )]
    FolderOfOtherDevice { device_id: String, server: String },
    #[error("the space is already linked to {0} on this device")]
    SpaceAlreadyLinked(PathBuf),
    #[error(
        "the folder is marked for space {0}; unlink it there first, or remove its .flarebox folder"
    )]
    MarkerOfOtherSpace(String),
    #[error(
        "the folder's file system cannot rename without replacing, so files cannot be applied safely"
    )]
    UnsupportedFileSystem,
    #[error("{0} is not a linked folder")]
    NotLinked(PathBuf),
    #[error("no unanswered question has the id {0:?}")]
    QuestionNotFound(String),
    #[error("the question accepts only {}", allowed.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(" or "))]
    AnswerNotAllowed { allowed: [Answer; 2] },
    #[error("the sync state at {path} cannot be used: {error}")]
    Store {
        path: PathBuf,
        error: flarebox_store::Error,
    },
    #[error("{path}: {error}")]
    Io { path: PathBuf, error: io::Error },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DriverError + '_ {
    move |error| DriverError::Io {
        path: path.to_path_buf(),
        error,
    }
}

fn store_error(path: &Path) -> impl FnOnce(flarebox_store::Error) -> DriverError + '_ {
    move |error| DriverError::Store {
        path: path.to_path_buf(),
        error,
    }
}

/// Runs sync for the folders one profile links. The Linux daemon and the macOS app use it alike: the daemon serves
/// it over a local socket, the app calls it in-process through `core/ffi`.
///
/// Opening a driver takes the profile's lock, so one process at a time writes a profile's credential and stores
/// (H6 of reports/BOX-71). The lock is released when the last clone is dropped.
#[derive(Clone)]
pub struct Driver {
    inner: Arc<Inner>,
}

struct Inner {
    paths: Paths,
    _lock: File,
    matcher: Arc<dyn Matcher>,
    credential: Mutex<Option<Credential>>,
    folders: Mutex<BTreeMap<String, Folder>>,
    running: AtomicBool,
    changed: Arc<watch::Sender<()>>,
}

struct Folder {
    link: Link,
    status: Arc<Mutex<FolderStatus>>,
    run: Option<Running>,
}

struct Running {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Driver {
    /// Opens the profile at `paths`: takes its lock, reads its configuration and credential, and finds its linked
    /// folders. Nothing syncs until [`Driver::start`].
    pub fn open(paths: Paths) -> Result<Self, DriverError> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&paths.state_dir)
            .map_err(io_error(&paths.state_dir))?;
        let lock_path = paths.lock();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .map_err(io_error(&lock_path))?;
        match rustix::fs::flock(&lock, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => return Err(DriverError::Busy(lock_path)),
            Err(error) => return Err(io_error(&lock_path)(error.into())),
        }
        let config = Config::load(&paths.config_file)?;
        let lines = config.sync.lines().expect("Config::load checked the rules");
        let mut input = IgnoreInput::default();
        if !lines.is_empty() {
            tracing::warn!(
                "the [sync] rules of the configuration are read, but no matcher applies them yet (GUN-270)"
            );
            input.files.push(RuleFile {
                dir: PathBuf::new(),
                source: RuleSource::Device,
                lines,
            });
        }
        let credential = Credential::load(&paths.credential())?;
        let folders = load_folders(&paths)?;
        Ok(Driver {
            inner: Arc::new(Inner {
                matcher: matcher(&input),
                paths,
                _lock: lock,
                credential: Mutex::new(credential),
                folders: Mutex::new(folders),
                running: AtomicBool::new(false),
                changed: Arc::new(watch::Sender::new(())),
            }),
        })
    }

    pub fn paths(&self) -> &Paths {
        &self.inner.paths
    }

    pub fn status(&self) -> Status {
        let account = self
            .inner
            .credential
            .lock()
            .unwrap()
            .as_ref()
            .map(Credential::account);
        let folders = self
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .map(|folder| folder.status.lock().unwrap().clone())
            .collect();
        Status {
            account,
            running: self.inner.running.load(Ordering::SeqCst),
            folders,
        }
    }

    /// A receiver that is marked changed whenever [`Driver::status`] or [`Driver::questions`] may have changed, so
    /// the app can wait for a change instead of polling. A change may be signalled without a visible difference.
    pub fn watch(&self) -> watch::Receiver<()> {
        self.inner.changed.subscribe()
    }

    fn changed(&self) {
        self.inner.changed.send_replace(());
    }

    /// Pairs this profile with a server, using a code from a device that is already paired (`POST
    /// /v1/pair/complete`), and stores the credential. A profile is paired once.
    pub async fn pair(
        &self,
        server: &str,
        code: &str,
        device_name: &str,
    ) -> Result<Account, DriverError> {
        self.check_unpaired()?;
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let api = api_for(server)?;
        let issued = api
            .pair_complete(&PairCompleteRequest {
                code: code.to_owned(),
                device_name: device_name.to_owned(),
                platform: std::env::consts::OS.to_owned(),
            })
            .await?;
        self.adopt(
            api.base().to_string(),
            issued,
            None,
            device_name,
            "paired this device",
        )
    }

    /// Claims a new server as its first device (`POST /v1/claim`), with the one-time code from the server's log,
    /// creating the user `user_name`, and stores the credential. Like pairing, it happens once per profile.
    pub async fn claim(
        &self,
        server: &str,
        code: &str,
        user_name: &str,
        device_name: &str,
    ) -> Result<Account, DriverError> {
        self.check_unpaired()?;
        if user_name.trim().is_empty() {
            return Err(DriverError::EmptyUserName);
        }
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let api = api_for(server)?;
        let issued = api
            .claim(&ClaimRequest {
                code: code.to_owned(),
                user_name: user_name.to_owned(),
                device_name: device_name.to_owned(),
                platform: std::env::consts::OS.to_owned(),
            })
            .await?;
        self.adopt(
            api.base().to_string(),
            issued,
            Some(user_name.to_owned()),
            device_name,
            "claimed the server",
        )
    }

    /// Signs this device out: stops syncing, unlinks every folder (their files stay) and deletes the credential, so
    /// the profile can be paired or claimed again. The server is not told; the device stays registered there.
    pub async fn sign_out(&self) -> Result<(), DriverError> {
        self.stop().await;
        let roots = roots(&self.inner.folders.lock().unwrap());
        for root in roots {
            self.unlink(&root).await?;
        }
        let path = self.inner.paths.credential();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&path)(error)),
        }
        *self.inner.credential.lock().unwrap() = None;
        tracing::info!("signed out");
        self.changed();
        Ok(())
    }

    fn check_unpaired(&self) -> Result<(), DriverError> {
        match self.inner.credential.lock().unwrap().as_ref() {
            Some(credential) => Err(DriverError::AlreadyPaired {
                device_id: credential.device_id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Stores the credential the server issued, unless another one was stored meanwhile.
    fn adopt(
        &self,
        server: String,
        issued: DeviceCredentials,
        user_name: Option<String>,
        device_name: &str,
        done: &str,
    ) -> Result<Account, DriverError> {
        let credential = Credential {
            server,
            user_id: issued.user_id,
            user_name,
            device_id: issued.device_id,
            device_name: device_name.to_owned(),
            refresh_credential: issued.refresh_credential,
        };
        let mut slot = self.inner.credential.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            return Err(DriverError::AlreadyPaired {
                device_id: existing.device_id.clone(),
            });
        }
        credential.save_new(&self.inner.paths.credential())?;
        tracing::info!(
            device_id = credential.device_id,
            server = credential.server,
            "{done}"
        );
        let account = credential.account();
        *slot = Some(credential);
        drop(slot);
        self.changed();
        Ok(account)
    }

    /// Links `folder` to the space with id or name `space`: checks the folder, writes its root marker, and creates
    /// its store. The folder starts syncing at once if the driver is running.
    pub async fn link(&self, folder: &Path, space: &str) -> Result<FolderStatus, DriverError> {
        let credential = self.credential()?;
        let root = fs::canonicalize(folder).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => DriverError::FolderNotFound(folder.to_path_buf()),
            _ => io_error(folder)(error),
        })?;
        if !root.is_dir() {
            return Err(DriverError::NotAFolder(root));
        }
        self.check_placement(&root, &self.inner.folders.lock().unwrap())?;
        if !probe_no_replace(&root).map_err(io_error(&root))? {
            return Err(DriverError::UnsupportedFileSystem);
        }
        let owner = Owner {
            server: credential.server.clone(),
            device_id: credential.device_id.clone(),
        };
        let lock = claim::lock(&root)
            .map_err(io_error(&root))?
            .ok_or_else(|| DriverError::FolderInUse(root.clone()))?;
        match claim::owner(&root).map_err(io_error(&root))? {
            Some(other) if other != owner => {
                return Err(DriverError::FolderOfOtherDevice {
                    device_id: other.device_id,
                    server: other.server,
                });
            }
            _ => {}
        }
        let api = credential.api()?;
        let auth = Auth::new(api.clone(), credential.net());
        let space = resolve_space(api.list_spaces(&auth).await?.spaces, space)?;

        let mut folders = self.inner.folders.lock().unwrap();
        self.check_placement(&root, &folders)?;
        if let Some(linked) = folders.get(&space.space_id) {
            return Err(DriverError::SpaceAlreadyLinked(linked.link.root.clone()));
        }
        match read_marker(&root).map_err(io_error(&root))? {
            Some(marker) if marker.space_id != space.space_id => {
                return Err(DriverError::MarkerOfOtherSpace(marker.space_id));
            }
            Some(_) => {}
            None => write_marker(
                &root,
                &Marker {
                    space_id: space.space_id.clone(),
                    effects: 0,
                },
            )
            .map_err(io_error(&root))?,
        }
        claim::set_owner(&root, &owner).map_err(io_error(&root))?;
        let link = Link {
            space_id: space.space_id.clone(),
            space_name: space.name.clone(),
            root_dev: fs::metadata(&root).map_err(io_error(&root))?.dev(),
            root,
            server_url: credential.server.clone(),
            device_id: credential.device_id.clone(),
            linked_at_ms: now_ms(),
        };
        let store_path = self.inner.paths.store(&link.space_id)?;
        let store_dir = store_path.parent().expect("a store path has a parent");
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(store_dir)
            .map_err(io_error(store_dir))?;
        Store::open(&store_path)
            .and_then(|mut store| store.transaction(|tx| tx.set_link(&link)))
            .map_err(store_error(&store_path))?;
        tracing::info!(space_id = link.space_id, path = %link.root.display(), "linked a folder");
        let status = Arc::new(Mutex::new(folder_status(&link, None, 0, 0)));
        let linked = roots(&folders);
        let mut folder = Folder {
            link,
            status: status.clone(),
            run: None,
        };
        drop(lock);
        if self.inner.running.load(Ordering::SeqCst) {
            self.spawn(&mut folder, &credential, linked);
        }
        folders.insert(space.space_id, folder);
        drop(folders);
        self.changed();
        let status = status.lock().unwrap().clone();
        Ok(status)
    }

    /// Stops syncing the folder at `folder` and forgets its sync state. The files stay, and so does the folder's
    /// `.flarebox` directory, whose local trash may hold displaced files.
    pub async fn unlink(&self, folder: &Path) -> Result<Link, DriverError> {
        let wanted = absolute(folder);
        let canonical = fs::canonicalize(folder).ok();
        let removed = {
            let mut folders = self.inner.folders.lock().unwrap();
            let space_id = folders
                .iter()
                .find(|(_, linked)| {
                    linked.link.root == wanted || Some(&linked.link.root) == canonical.as_ref()
                })
                .map(|(space_id, _)| space_id.clone())
                .ok_or_else(|| DriverError::NotLinked(folder.to_path_buf()))?;
            folders.remove(&space_id).expect("found above")
        };
        if let Some(run) = removed.run {
            let _ = run.stop.send(true);
            let _ = run.task.await;
        }
        let store_dir = self
            .inner
            .paths
            .store(&removed.link.space_id)?
            .parent()
            .expect("a store path has a parent")
            .to_path_buf();
        fs::remove_dir_all(&store_dir).map_err(io_error(&store_dir))?;
        let owner = Owner {
            server: removed.link.server_url.clone(),
            device_id: removed.link.device_id.clone(),
        };
        if let Err(error) = claim::release(&removed.link.root, &owner) {
            tracing::warn!(path = %removed.link.root.display(), %error, "the folder's owner record was not removed");
        }
        tracing::info!(space_id = removed.link.space_id, path = %removed.link.root.display(), "unlinked a folder");
        self.changed();
        Ok(removed.link)
    }

    /// Unanswered questions of every linked folder, oldest first per folder.
    pub fn questions(&self) -> Result<Vec<Question>, DriverError> {
        let mut questions = Vec::new();
        for space_id in self.space_ids() {
            for stored in self.stored_questions(&space_id)? {
                if stored.answer.is_none()
                    && let Some(question) = decode_question(&space_id, &stored)
                {
                    questions.push(question);
                }
            }
        }
        Ok(questions)
    }

    /// Records the answer to a question. The engine takes it as a `UserDecision` input.
    pub fn answer(&self, question_id: &str, answer: Answer) -> Result<(), DriverError> {
        for space_id in self.space_ids() {
            let path = self.inner.paths.store(&space_id)?;
            let mut store = Store::open(&path).map_err(store_error(&path))?;
            let outcome = store
                .transaction(|tx| {
                    let Some(stored) = tx.questions()?.into_iter().find(|stored| {
                        stored.question_id == question_id && stored.answer.is_none()
                    }) else {
                        return Ok(None);
                    };
                    let Some(question) = decode_question(&space_id, &stored) else {
                        return Ok(None);
                    };
                    let allowed = question.kind.answers();
                    if !allowed.contains(&answer) {
                        return Ok(Some(Err(DriverError::AnswerNotAllowed { allowed })));
                    }
                    let given = GivenAnswer {
                        answer: answer.as_str().to_owned(),
                        answered_at_ms: now_ms(),
                    };
                    tx.answer_question(question_id, &given)?;
                    let left = tx
                        .questions()?
                        .iter()
                        .filter(|q| q.answer.is_none())
                        .count();
                    Ok(Some(Ok(left as u64)))
                })
                .map_err(store_error(&path))?;
            match outcome {
                None => continue,
                Some(Err(error)) => return Err(error),
                Some(Ok(left)) => {
                    if let Some(folder) = self.inner.folders.lock().unwrap().get(&space_id) {
                        folder.status.lock().unwrap().questions = left;
                    }
                    tracing::info!(space_id, question_id, %answer, "answered a question");
                    self.changed();
                    return Ok(());
                }
            }
        }
        Err(DriverError::QuestionNotFound(question_id.to_owned()))
    }

    /// Starts syncing every linked folder, each in a task on the current Tokio runtime. Starting a running driver
    /// does nothing.
    pub fn start(&self) {
        if self.inner.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let credential = self.inner.credential.lock().unwrap().clone();
        let mut folders = self.inner.folders.lock().unwrap();
        let all = roots(&folders);
        for folder in folders.values_mut() {
            let linked = all
                .iter()
                .filter(|root| **root != folder.link.root)
                .cloned()
                .collect();
            match &credential {
                Some(credential) => self.spawn(folder, credential, linked),
                None => halt(
                    folder,
                    "this device is not paired; run flarebox pair".into(),
                ),
            }
        }
        drop(folders);
        self.changed();
    }

    /// Stops every folder and waits for their tasks to end. State on disk is kept.
    pub async fn stop(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        let runs: Vec<Running> = self
            .inner
            .folders
            .lock()
            .unwrap()
            .values_mut()
            .filter_map(|folder| folder.run.take())
            .collect();
        for run in &runs {
            let _ = run.stop.send(true);
        }
        for run in runs {
            let _ = run.task.await;
        }
        for folder in self.inner.folders.lock().unwrap().values() {
            folder.status.lock().unwrap().state = FolderState::Stopped;
        }
        self.changed();
    }

    /// Starts the task of `folder`. `linked` are the roots of the profile's other folders, which it must not overlap.
    fn spawn(&self, folder: &mut Folder, credential: &Credential, linked: Vec<PathBuf>) {
        if folder.link.device_id != credential.device_id
            || folder.link.server_url != credential.server
        {
            return halt(
                folder,
                format!(
                    "the folder was linked by device {} of {}, not by this profile's device {}; unlink it and link it again",
                    folder.link.device_id, folder.link.server_url, credential.device_id
                ),
            );
        }
        let api = match credential.api() {
            Ok(api) => api,
            Err(error) => return halt(folder, error.to_string()),
        };
        let store = match self.inner.paths.store(&folder.link.space_id) {
            Ok(store) => store,
            Err(error) => return halt(folder, error.to_string()),
        };
        let task = FolderTask {
            link: folder.link.clone(),
            store,
            auth: Arc::new(Auth::new(api, credential.net())),
            status: folder.status.clone(),
            matcher: self.inner.matcher.clone(),
            private_dirs: self.inner.paths.private_dirs(),
            linked,
            owner: Owner {
                server: credential.server.clone(),
                device_id: credential.device_id.clone(),
            },
            observer: Mutex::default(),
            changed: self.inner.changed.clone(),
        };
        let (stop, stopped) = watch::channel(false);
        folder.status.lock().unwrap().state = FolderState::Connecting;
        folder.run = Some(Running {
            stop,
            task: tokio::spawn(task.run(stopped)),
        });
    }

    fn credential(&self) -> Result<Credential, DriverError> {
        self.inner
            .credential
            .lock()
            .unwrap()
            .clone()
            .ok_or(DriverError::NotPaired)
    }

    fn space_ids(&self) -> Vec<String> {
        self.inner.folders.lock().unwrap().keys().cloned().collect()
    }

    fn stored_questions(&self, space_id: &str) -> Result<Vec<StoredQuestion>, DriverError> {
        let path = self.inner.paths.store(space_id)?;
        Store::open(&path)
            .and_then(|mut store| store.transaction(|tx| tx.questions()))
            .map_err(store_error(&path))
    }

    fn check_placement(
        &self,
        root: &Path,
        folders: &BTreeMap<String, Folder>,
    ) -> Result<(), DriverError> {
        check_placement(root, &self.inner.paths.private_dirs(), &roots(folders))
    }
}

fn roots(folders: &BTreeMap<String, Folder>) -> Vec<PathBuf> {
    folders
        .values()
        .map(|folder| folder.link.root.clone())
        .collect()
}

/// Refuses a folder that holds or sits inside one of the profile's private directories or another linked folder.
/// Folders are compared by the identity of each folder and its ancestors as well as by path, so a symbolic link or a
/// bind mount does not hide an overlap. Linking checks this, and so does each folder task before it observes the
/// folder or connects, because a profile can be moved after its folders were linked.
pub(crate) fn check_placement(
    root: &Path,
    private_dirs: &[PathBuf],
    linked: &[PathBuf],
) -> Result<(), DriverError> {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let root = canonical(root);
    for dir in private_dirs {
        let dir = canonical(dir);
        if holds(&root, &dir) || holds(&dir, &root) {
            return Err(DriverError::FolderHoldsProfile(dir));
        }
    }
    for other in linked {
        let canonical = canonical(other);
        if holds(&root, &canonical) || holds(&canonical, &root) {
            return Err(DriverError::FolderOverlaps(other.clone()));
        }
    }
    Ok(())
}

/// Whether `inner` is `outer` or lies under it: by path, or because one of `inner`'s existing ancestors is the same
/// folder as `outer`.
fn holds(outer: &Path, inner: &Path) -> bool {
    if inner.starts_with(outer) {
        return true;
    }
    let Ok(outer) = fs::metadata(outer) else {
        return false;
    };
    inner
        .ancestors()
        .filter_map(|dir| fs::metadata(dir).ok())
        .any(|dir| dir.dev() == outer.dev() && dir.ino() == outer.ino())
}

fn halt(folder: &mut Folder, problem: String) {
    tracing::warn!(
        space_id = folder.link.space_id,
        problem,
        "a folder cannot sync"
    );
    let mut status = folder.status.lock().unwrap();
    status.state = FolderState::Halted;
    status.problem = Some(problem);
}

fn folder_status(
    link: &Link,
    cursor: Option<flarebox_store::Cursor>,
    nodes: u64,
    questions: u64,
) -> FolderStatus {
    FolderStatus {
        space_id: link.space_id.clone(),
        space_name: link.space_name.clone(),
        path: link.root.display().to_string(),
        state: FolderState::Stopped,
        history_id: cursor.as_ref().map(|cursor| cursor.history_id.clone()),
        last_seq: cursor.map(|cursor| cursor.last_seq),
        head_seq: None,
        remote_nodes: nodes,
        local_entries: None,
        questions,
        problem: None,
    }
}

/// Every store under the profile's spaces directory that records a link.
fn load_folders(paths: &Paths) -> Result<BTreeMap<String, Folder>, DriverError> {
    let spaces = paths.spaces();
    let entries = match fs::read_dir(&spaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(io_error(&spaces)(error)),
    };
    let mut folders = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(io_error(&spaces))?;
        let Ok(path) = paths.store(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let mut store = Store::open(&path).map_err(store_error(&path))?;
        let (link, cursor, nodes, questions) = store
            .transaction(|tx| {
                let questions = tx
                    .questions()?
                    .iter()
                    .filter(|q| q.answer.is_none())
                    .count();
                Ok::<_, flarebox_store::Error>((
                    tx.link()?,
                    tx.cursor()?,
                    tx.remote_node_count()?,
                    questions,
                ))
            })
            .map_err(store_error(&path))?;
        let Some(link) = link else {
            tracing::warn!(path = %path.display(), "a store records no linked folder; ignoring it");
            continue;
        };
        let status = folder_status(&link, cursor, nodes, questions as u64);
        folders.insert(
            link.space_id.clone(),
            Folder {
                status: Arc::new(Mutex::new(status)),
                link,
                run: None,
            },
        );
    }
    Ok(folders)
}

fn resolve_space(spaces: Vec<SpaceInfo>, wanted: &str) -> Result<SpaceInfo, DriverError> {
    if let Some(space) = spaces.iter().find(|space| space.space_id == wanted) {
        return Ok(space.clone());
    }
    let mut named: Vec<SpaceInfo> = spaces
        .into_iter()
        .filter(|space| space.name == wanted)
        .collect();
    match named.len() {
        0 => Err(DriverError::SpaceNotFound(wanted.to_owned())),
        1 => Ok(named.remove(0)),
        _ => Err(DriverError::SpaceAmbiguous {
            name: wanted.to_owned(),
            ids: named.into_iter().map(|space| space.space_id).collect(),
        }),
    }
}

fn decode_question(space_id: &str, stored: &StoredQuestion) -> Option<Question> {
    match serde_json::from_slice::<Question>(&stored.body) {
        Ok(question) => Some(question),
        Err(error) => {
            tracing::warn!(space_id, question_id = stored.question_id, %error, "a stored question cannot be read");
            None
        }
    }
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|dir| dir.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::question::{HeldItem, QuestionKind};

    use super::*;

    const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

    fn credential(device_id: &str) -> Credential {
        Credential {
            server: "https://files.example.com/".into(),
            user_id: "u1".into(),
            user_name: None,
            device_id: device_id.into(),
            device_name: "vm".into(),
            refresh_credential: "secret".into(),
        }
    }

    /// A profile with one linked folder whose store holds `questions`.
    fn profile(questions: &[Question]) -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Flarebox");
        fs::create_dir_all(&root).unwrap();
        let store_path = paths.store(SPACE).unwrap();
        fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        let mut store = Store::open(&store_path).unwrap();
        store
            .transaction(|tx| {
                tx.set_link(&Link {
                    space_id: SPACE.into(),
                    space_name: "Projects".into(),
                    root: root.clone(),
                    root_dev: 1,
                    server_url: "https://files.example.com/".into(),
                    device_id: "d1".into(),
                    linked_at_ms: 1,
                })?;
                for question in questions {
                    tx.put_question(&StoredQuestion {
                        question_id: question.id.clone(),
                        asked_at_ms: question.asked_at_ms,
                        body: serde_json::to_vec(question).unwrap(),
                        answer: None,
                    })?;
                }
                Ok::<_, flarebox_store::Error>(())
            })
            .unwrap();
        (dir, paths, root)
    }

    fn mass_delete(id: &str) -> Question {
        Question {
            id: id.into(),
            space_id: SPACE.into(),
            asked_at_ms: 10,
            kind: QuestionKind::MassDelete {
                deletions: 1_001,
                affected_files: 1_001,
                space_files: 5_000,
            },
            items: vec![HeldItem {
                node_id: "n1".into(),
                path: "old".into(),
                files: 1_001,
            }],
        }
    }

    #[test]
    fn one_process_at_a_time_opens_a_profile() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        assert!(matches!(
            Driver::open(paths.clone()),
            Err(DriverError::Busy(_))
        ));
        drop(driver);
        Driver::open(paths).unwrap();
    }

    #[test]
    fn linked_folders_are_found_in_their_stores() {
        let (_dir, paths, root) = profile(&[mass_delete("q1")]);
        let status = Driver::open(paths).unwrap().status();
        assert_eq!(status.account, None);
        assert!(!status.running);
        assert_eq!(status.folders.len(), 1);
        let folder = &status.folders[0];
        assert_eq!(folder.space_id, SPACE);
        assert_eq!(folder.path, root.display().to_string());
        assert_eq!(folder.state, FolderState::Stopped);
        assert_eq!(folder.questions, 1);
    }

    #[test]
    fn questions_take_only_the_answers_they_offer() {
        let (_dir, paths, _root) = profile(&[mass_delete("q1"), mass_delete("q2")]);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(
            driver.questions().unwrap(),
            [mass_delete("q1"), mass_delete("q2")]
        );

        assert!(matches!(
            driver.answer("q1", Answer::Publish),
            Err(DriverError::AnswerNotAllowed {
                allowed: [Answer::Delete, Answer::Restore]
            })
        ));
        driver.answer("q1", Answer::Restore).unwrap();
        assert!(matches!(
            driver.answer("q1", Answer::Delete),
            Err(DriverError::QuestionNotFound(_))
        ));
        assert_eq!(driver.questions().unwrap(), [mass_delete("q2")]);
        assert_eq!(driver.status().folders[0].questions, 1);

        let stored = driver.stored_questions(SPACE).unwrap();
        assert_eq!(stored[0].answer.as_ref().unwrap().answer, "restore");
    }

    #[tokio::test]
    async fn a_folder_linked_by_another_credential_does_not_sync() {
        let (_dir, paths, _root) = profile(&[]);
        credential("d2").save_new(&paths.credential()).unwrap();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        let status = driver.status();
        assert!(status.running);
        assert_eq!(status.account.unwrap().device_id, "d2");
        let folder = &status.folders[0];
        assert_eq!(folder.state, FolderState::Halted);
        assert!(
            folder.problem.as_ref().unwrap().contains("device d1"),
            "{folder:?}"
        );
        driver.stop().await;
        assert_eq!(driver.status().folders[0].state, FolderState::Stopped);
    }

    /// A server address nobody answers.
    const NOWHERE: &str = "http://127.0.0.1:9/";

    /// Pairs the profile at `paths` as `device_id` with a server nobody answers.
    fn paired(paths: &Paths, device_id: &str) {
        Credential {
            server: NOWHERE.into(),
            ..credential(device_id)
        }
        .save_new(&paths.credential())
        .unwrap();
    }

    /// Pairs the profile at `paths` as `device_id` and links `root` as a folder that passes its root check.
    fn linked(paths: &Paths, root: &Path, device_id: &str) {
        paired(paths, device_id);
        fs::create_dir_all(root).unwrap();
        write_marker(
            root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        let store_path = paths.store(SPACE).unwrap();
        fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        let link = Link {
            space_id: SPACE.into(),
            space_name: "Projects".into(),
            root: root.to_path_buf(),
            root_dev: fs::metadata(root).unwrap().dev(),
            server_url: NOWHERE.into(),
            device_id: device_id.into(),
            linked_at_ms: 1,
        };
        Store::open(&store_path)
            .unwrap()
            .transaction(|tx| tx.set_link(&link))
            .unwrap();
    }

    async fn eventually(what: &str, done: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "not in time: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until the folder of `driver` halts, and returns its problem.
    async fn halted(driver: &Driver) -> String {
        eventually("the folder halts", || {
            driver.status().folders[0].state == FolderState::Halted
        })
        .await;
        driver.status().folders[0].problem.clone().unwrap()
    }

    #[tokio::test]
    async fn a_folder_that_holds_a_reopened_profile_halts_before_it_is_observed() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().join("Flarebox");
        let paths = Paths::under(&root.join("moved-profile"));
        linked(&paths, &root, "d1");
        fs::write(root.join("file"), b"synced").unwrap();

        let driver = Driver::open(paths).unwrap();
        driver.start();
        let problem = halted(&driver).await;
        assert!(
            problem.contains("overlaps Flarebox's own files"),
            "{problem}"
        );
        assert_eq!(driver.status().folders[0].local_entries, None);
        assert_eq!(claim::owner(&root).unwrap(), None);
        driver.stop().await;
    }

    #[test]
    fn an_overlap_is_found_through_a_symbolic_link() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let state = root.join("profile/state");
        fs::create_dir_all(&state).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(matches!(
            check_placement(&alias, &[state], &[]),
            Err(DriverError::FolderHoldsProfile(_))
        ));
        assert!(matches!(
            check_placement(&root.join("profile"), &[], &[alias]),
            Err(DriverError::FolderOverlaps(_))
        ));
    }

    #[tokio::test]
    async fn two_profiles_never_sync_one_folder_at_once_or_in_turn() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
        linked(&a, &root, "d1");
        linked(&b, &root, "d2");
        fs::write(root.join("one"), b"1").unwrap();
        let first = Driver::open(a).unwrap();
        let second = Driver::open(b).unwrap();
        let entries = |driver: &Driver| driver.status().folders[0].local_entries;

        first.start();
        eventually("the first profile observes the folder", || {
            entries(&first) == Some(1)
        })
        .await;
        second.start();
        let problem = halted(&second).await;
        assert!(problem.contains("another Flarebox process"), "{problem}");
        second.stop().await;

        first.stop().await;
        drop(
            claim::lock(&root)
                .unwrap()
                .expect("stopping releases the folder's lock"),
        );
        fs::write(root.join("two"), b"2").unwrap();
        first.start();
        eventually("the restarted profile observes the folder", || {
            entries(&first) == Some(2)
        })
        .await;
        assert_ne!(first.status().folders[0].state, FolderState::Halted);
        second.start();
        let problem = halted(&second).await;
        assert!(problem.contains("another Flarebox process"), "{problem}");
        second.stop().await;
        first.stop().await;

        second.start();
        let problem = halted(&second).await;
        assert!(problem.contains("linked by device d1"), "{problem}");
        second.stop().await;
        assert!(claim::lock(&root).unwrap().is_some());
    }

    #[tokio::test]
    async fn deleting_the_private_directory_does_not_let_another_profile_in() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let (a, b) = (Paths::under(&base.join("a")), Paths::under(&base.join("b")));
        linked(&a, &root, "d1");
        linked(&b, &root, "d2");
        fs::write(root.join("one"), b"1").unwrap();
        let first = Driver::open(a).unwrap();
        first.start();
        eventually("the first profile observes the folder", || {
            first.status().folders[0].local_entries == Some(1)
        })
        .await;

        fs::remove_dir_all(root.join(flarebox_fs::PRIVATE_DIR)).unwrap();
        let third = Paths::under(&base.join("c"));
        paired(&third, "d3");
        assert!(matches!(
            Driver::open(third).unwrap().link(&root, "Projects").await,
            Err(DriverError::FolderInUse(_))
        ));
        write_marker(
            &root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        let second = Driver::open(b).unwrap();
        second.start();
        let problem = halted(&second).await;
        assert!(problem.contains("another Flarebox process"), "{problem}");
        assert_ne!(first.status().folders[0].state, FolderState::Halted);
        second.stop().await;
        first.stop().await;
    }

    #[tokio::test]
    async fn a_folder_replaced_under_its_task_halts_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;

        fs::rename(&root, base.join("Moved")).unwrap();
        fs::create_dir(&root).unwrap();
        let problem = halted(&driver).await;
        assert!(problem.contains("no longer the folder"), "{problem}");
        driver.stop().await;
        assert!(claim::lock(&base.join("Moved")).unwrap().is_some());
    }

    /// Serves one request on a loopback port with `reply`, a protobuf body, and returns the server's address and
    /// what the request was: its path and its body.
    async fn serve_once(reply: Vec<u8>) -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..n]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap_or(0);
            while request.len() < head_end + length {
                let n = socket.read(&mut chunk).await.unwrap();
                request.extend_from_slice(&chunk[..n]);
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                reply.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(&reply).await.unwrap();
            let path = head.split_whitespace().nth(1).unwrap().to_owned();
            (path, request[head_end..head_end + length].to_vec())
        });
        (server, served)
    }

    fn issued(device_id: &str) -> Vec<u8> {
        prost::Message::encode_to_vec(&DeviceCredentials {
            user_id: "u1".into(),
            device_id: device_id.into(),
            refresh_credential: "secret".into(),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn claiming_creates_the_first_device_with_its_user_name() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        assert!(matches!(
            driver
                .claim("http://127.0.0.1:9/", "code", " ", "laptop")
                .await,
            Err(DriverError::EmptyUserName)
        ));

        let (server, served) = serve_once(issued("d1")).await;
        let account = driver
            .claim(&server, "claim-code", "Ada", "laptop")
            .await
            .unwrap();
        assert_eq!(account.user_name.as_deref(), Some("Ada"));
        assert_eq!(account.device_id, "d1");
        let (path, body) = served.await.unwrap();
        assert_eq!(path, "/v1/claim");
        let request: ClaimRequest = prost::Message::decode(body.as_slice()).unwrap();
        assert_eq!(
            (
                request.code.as_str(),
                request.user_name.as_str(),
                request.device_name.as_str()
            ),
            ("claim-code", "Ada", "laptop")
        );
        assert!(matches!(
            driver.claim(&server, "claim-code", "Ada", "laptop").await,
            Err(DriverError::AlreadyPaired { .. })
        ));
        drop(driver);
        let reopened = Driver::open(paths).unwrap();
        assert_eq!(reopened.status().account, Some(account));
    }

    #[tokio::test]
    async fn a_paired_device_does_not_know_the_user_name() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(dir.path())).unwrap();
        let (server, served) = serve_once(issued("d2")).await;
        let account = driver.pair(&server, "pair-code", "desktop").await.unwrap();
        assert_eq!(served.await.unwrap().0, "/v1/pair/complete");
        assert_eq!(account.user_name, None);
        assert_eq!(account.device_name, "desktop");
    }

    #[tokio::test]
    async fn watchers_wake_when_status_or_questions_change() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        fs::write(root.join("one"), b"1").unwrap();
        let store = paths.store(SPACE).unwrap();
        Store::open(&store)
            .unwrap()
            .transaction(|tx| {
                tx.put_question(&StoredQuestion {
                    question_id: "q1".into(),
                    asked_at_ms: 10,
                    body: serde_json::to_vec(&mass_delete("q1")).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        let driver = Driver::open(paths).unwrap();
        let mut watch = driver.watch();
        assert!(!watch.has_changed().unwrap());

        driver.answer("q1", Answer::Restore).unwrap();
        assert!(watch.has_changed().unwrap());
        watch.borrow_and_update();

        driver.start();
        assert!(watch.has_changed().unwrap());
        watch.borrow_and_update();
        tokio::time::timeout(Duration::from_secs(10), async {
            while driver.status().folders[0].local_entries != Some(1) {
                watch.changed().await.unwrap();
            }
        })
        .await
        .expect("the folder task's progress wakes the watcher");

        watch.borrow_and_update();
        driver.stop().await;
        assert!(watch.has_changed().unwrap());
    }

    #[tokio::test]
    async fn signing_out_unlinks_every_folder_and_forgets_the_credential() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        fs::write(root.join("keep.txt"), b"mine").unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        let mut watch = driver.watch();
        watch.borrow_and_update();

        driver.sign_out().await.unwrap();
        let status = driver.status();
        assert_eq!(status.account, None);
        assert!(!status.running);
        assert!(status.folders.is_empty());
        assert!(watch.has_changed().unwrap());
        assert!(!paths.credential().exists());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"mine");

        let (server, served) = serve_once(issued("d3")).await;
        driver.pair(&server, "pair-code", "laptop").await.unwrap();
        served.await.unwrap();
        drop(driver);
        let reopened = Driver::open(paths).unwrap().status();
        assert_eq!(reopened.account.unwrap().device_id, "d3");
        assert!(reopened.folders.is_empty());
    }

    #[tokio::test]
    async fn linking_refuses_a_folder_of_another_device_or_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Flarebox");
        fs::create_dir(&root).unwrap();
        let paths = Paths::under(&base.join("profile"));
        paired(&paths, "d2");
        let driver = Driver::open(paths).unwrap();
        fs::create_dir(root.join(flarebox_fs::PRIVATE_DIR)).unwrap();
        let first = Owner {
            server: NOWHERE.into(),
            device_id: "d1".into(),
        };
        claim::set_owner(&root, &first).unwrap();
        assert!(matches!(
            driver.link(&root, "Projects").await,
            Err(DriverError::FolderOfOtherDevice { device_id, .. }) if device_id == "d1"
        ));

        claim::release(&root, &first).unwrap();
        let held = claim::lock(&root).unwrap().unwrap();
        assert!(matches!(
            driver.link(&root, "Projects").await,
            Err(DriverError::FolderInUse(_))
        ));
        drop(held);
        assert!(matches!(
            driver.link(&root, "Projects").await,
            Err(DriverError::Api(_))
        ));
        assert!(driver.status().folders.is_empty());
    }

    #[tokio::test]
    async fn unlinking_forgets_the_store_and_keeps_the_files() {
        let (_dir, paths, root) = profile(&[]);
        fs::write(root.join("keep.txt"), b"mine").unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        assert!(matches!(
            driver.unlink(&root.join("elsewhere")).await,
            Err(DriverError::NotLinked(_))
        ));
        let link = driver.unlink(&root).await.unwrap();
        assert_eq!(link.space_id, SPACE);
        assert!(driver.status().folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert_eq!(fs::read(root.join("keep.txt")).unwrap(), b"mine");
        drop(driver);
        assert!(Driver::open(paths).unwrap().status().folders.is_empty());
    }

    #[tokio::test]
    async fn linking_needs_a_paired_device() {
        let dir = tempfile::tempdir().unwrap();
        let driver = Driver::open(Paths::under(&dir.path().join("profile"))).unwrap();
        assert!(matches!(
            driver.link(dir.path(), "Projects").await,
            Err(DriverError::NotPaired)
        ));
    }

    #[test]
    fn a_folder_may_not_hold_or_sit_inside_the_profile_or_another_folder() {
        let (dir, paths, root) = profile(&[]);
        let base = fs::canonicalize(dir.path()).unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let folders = driver.inner.folders.lock().unwrap();
        let check = |path: &Path| driver.check_placement(path, &folders);
        assert!(matches!(
            check(&base),
            Err(DriverError::FolderHoldsProfile(_))
        ));
        assert!(matches!(
            check(&paths.state_dir.join("spaces")),
            Err(DriverError::FolderHoldsProfile(_))
        ));
        assert!(matches!(
            check(&root.join("inner")),
            Err(DriverError::FolderOverlaps(_))
        ));
        check(&base.join("Other")).unwrap();
    }

    #[test]
    fn spaces_resolve_by_id_then_by_unique_name() {
        let space = |id: &str, name: &str| SpaceInfo {
            space_id: id.into(),
            name: name.into(),
            ..Default::default()
        };
        let spaces = vec![
            space("a1", "Work"),
            space("b2", "Home"),
            space("c3", "Home"),
            space("d4", "a1"),
        ];
        assert_eq!(resolve_space(spaces.clone(), "a1").unwrap().space_id, "a1");
        assert_eq!(
            resolve_space(spaces.clone(), "Work").unwrap().space_id,
            "a1"
        );
        assert!(matches!(
            resolve_space(spaces.clone(), "Home"),
            Err(DriverError::SpaceAmbiguous { ids, .. }) if ids == ["b2", "c3"]
        ));
        assert!(matches!(
            resolve_space(spaces, "work"),
            Err(DriverError::SpaceNotFound(_))
        ));
    }
}
