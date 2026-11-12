// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use koidrive_engine::commit::{Commits, Lift, LiftError};
use koidrive_engine::guard::Window;
use koidrive_engine::ignore::LocalRules;
use koidrive_fs::{Marker, probe_no_replace, read_marker, write_marker};
use koidrive_ignore::LocalRulesError;
use koidrive_net::{ApiError, Auth, ROTATION_PERIOD, UploadGate};
use koidrive_proto::v1::{ClaimRequest, DeviceCredentials, PairCompleteRequest, SpaceInfo};
use koidrive_store::{EngineState, GivenAnswer, Link, Store, StoredQuestion};
use tokio::sync::{Notify, watch};

use crate::claim::{self, Flock, Owner};
use crate::config::{Config, ConfigError};
use crate::credential::{
    Credential, CredentialError, CredentialFiles, NewCredentialFile, StoredCredential, api_for,
    next_path,
};
use crate::failures::{self, Failure, Halted};
use crate::folder::FolderTask;
use crate::ignore::{Matcher, local_rules, matcher};
use crate::paths::{Paths, PathsError};
use crate::question::{Answer, Limit, Question, QuestionKind};
use crate::restore::{self, RestoreRequest, RestoreStatus};
use crate::status::{Account, FolderState, FolderStatus, Status};

/// How long a sign-out waits for each call to the server before it goes on without it.
const SERVER_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error("another Koidrive process is using this profile ({})", koidrive_fs::quoted(.0))]
    Busy(PathBuf),
    #[error("this device is not set up; run koi setup first")]
    NotPaired,
    #[error(
        "this profile is already paired as device {device_id}; a profile holds one device credential"
    )]
    AlreadyPaired { device_id: String },
    #[error("a device name is required")]
    EmptyName,
    #[error("a user name is required")]
    EmptyUserName,
    #[error(
        "this is the last active device of its user; after it signs out, only owner recovery adds a device again"
    )]
    LastDevice,
    #[error(
        "the server did not confirm that it revoked this device ({0}); if this is the last active device of its user, only owner recovery adds a device again after it signs out"
    )]
    NotRevoked(String),
    #[error(
        "the saved credential now belongs to device {device_id}, not to the one the server refused, so it was kept; run the command again"
    )]
    CredentialReplaced { device_id: String },
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
    #[error("{} does not exist", koidrive_fs::quoted(.0))]
    FolderNotFound(PathBuf),
    #[error("{} is not a folder", koidrive_fs::quoted(.0))]
    NotAFolder(PathBuf),
    #[error(
        "the folder overlaps Koidrive's own files at {}; link a folder that neither holds nor sits inside it",
        koidrive_fs::quoted(.0)
    )]
    FolderHoldsProfile(PathBuf),
    #[error("the folder overlaps the linked folder {}", koidrive_fs::quoted(.0))]
    FolderOverlaps(PathBuf),
    #[error("another Koidrive process is syncing {}; stop it, or unlink the folder there", koidrive_fs::quoted(.0))]
    FolderInUse(PathBuf),
    #[error(
        "the folder is linked by device {device_id} of {server}; unlink it there first, or remove its .koidrive/device.json"
    )]
    FolderOfOtherDevice { device_id: String, server: String },
    #[error("the space is already linked to {} on this device", koidrive_fs::quoted(.0))]
    SpaceAlreadyLinked(PathBuf),
    #[error(
        "the folder is marked for space {0}; unlink it there first, or remove its .koidrive folder"
    )]
    MarkerOfOtherSpace(String),
    #[error(
        "the folder's file system cannot rename without replacing, so files cannot be applied safely"
    )]
    UnsupportedFileSystem,
    #[error("{} is not a linked folder", koidrive_fs::quoted(.0))]
    NotLinked(PathBuf),
    #[error("no unanswered question has the id {0:?}")]
    QuestionNotFound(String),
    #[error("the question was answered with {given}")]
    AnsweredOtherwise { given: Answer },
    #[error("the question accepts only {}", allowed.iter().map(|a| a.as_str()).collect::<Vec<_>>().join(" or "))]
    AnswerNotAllowed { allowed: &'static [Answer] },
    #[error("no linked folder syncs the space {0:?}")]
    SpaceNotLinked(String),
    #[error("the restore's {0} is not an id the server gave")]
    InvalidRestore(&'static str),
    #[error("the folder holds no changes the server refused")]
    NothingToLift,
    #[error(
        "the server waits for the space's owner to confirm the folder's changes, and Koidrive cannot ask for that yet"
    )]
    NeedsConfirmation,
    #[error("the sync state at {} cannot be used: {error}", koidrive_fs::quoted(.path))]
    Store {
        path: PathBuf,
        error: koidrive_store::Error,
    },
    #[error("{}: {error}", koidrive_fs::quoted(.path))]
    Io { path: PathBuf, error: io::Error },
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> DriverError + '_ {
    move |error| DriverError::Io {
        path: path.to_path_buf(),
        error,
    }
}

fn store_error(path: &Path) -> impl FnOnce(koidrive_store::Error) -> DriverError + '_ {
    move |error| DriverError::Store {
        path: path.to_path_buf(),
        error,
    }
}

/// Lifts the policy halt the store at `path` holds (RFC 0006 section 1, point 5). No task may hold the store.
pub(crate) fn lift_halt(path: &Path, lift: Lift) -> Result<(), DriverError> {
    let lifted = Store::open(path)
        .and_then(|mut store| {
            store.transaction(|tx| {
                let before = tx.engine_state()?;
                let mut commits = Commits::from_state(before.commits.clone()).map_err(|error| {
                    koidrive_store::Error::InvalidEngineState(format!("{error:?}"))
                })?;
                let lifted = commits.lift(lift);
                if lifted.is_ok() {
                    let after = EngineState {
                        commits: commits.state(),
                        ..before.clone()
                    };
                    tx.update_engine_state(&before, &after)?;
                }
                Ok(lifted)
            })
        })
        .map_err(store_error(path))?;
    lifted.map_err(|error| match error {
        LiftError::NotHalted => DriverError::NothingToLift,
        LiftError::NeedsConfirmation => DriverError::NeedsConfirmation,
    })
}

/// What [`Driver::answer`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answered {
    /// The answer is recorded now.
    Now,
    /// The question had this answer already, waiting for the engine or taken by it, and nothing changed.
    Already,
}

/// What [`Driver::link`] did, with the folder's status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Linked {
    /// The folder is linked now.
    New(FolderStatus),
    /// The folder was already linked to the space, and nothing changed.
    Already(FolderStatus),
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
    _lock: Flock,
    matcher: Arc<dyn Matcher>,
    /// The `[sync]` list that `matcher` applies, for the engine of each folder.
    local: LocalRules,
    /// The size cap in bytes of each folder's local trash, from `[trash]`.
    trash_cap: u64,
    /// The limit on chunk uploads in flight from `[upload]`, which the uploads of every folder share.
    upload_gate: UploadGate,
    /// Held for the whole of a pair, claim, sign-out, link or unlink, so one runs at a time: a sign-out waits for a
    /// claim or a link in flight and then undoes it, and a credential the server issued is always stored.
    lifecycle: tokio::sync::Mutex<()>,
    credential: Mutex<Option<Credential>>,
    /// The one [`Auth`] of this process while it is signed in, which every caller uses, so that a rotation of the
    /// refresh credential reaches all of them (RFC 0011 section 2). None while signed out, or when the credential's
    /// server URL cannot be used.
    auth: Mutex<Option<Arc<Auth>>>,
    /// Held by every write to the credential files and by the sign-out while it removes them (`CredentialFiles`).
    credential_writes: Arc<Mutex<()>>,
    folders: Mutex<BTreeMap<String, Folder>>,
    running: AtomicBool,
    changed: Arc<watch::Sender<()>>,
    #[cfg(test)]
    observe_fault: Option<Arc<crate::folder::ObserveFault>>,
}

/// A linked folder. It stays in the driver's map until unlinking it has finished, so a stop, an unlink or a sign-out
/// that starts later still finds its task and its cleanup, also after an earlier caller gave up waiting.
struct Folder {
    link: Link,
    status: Arc<Mutex<FolderStatus>>,
    /// The folder's task, from its start until a stop has seen it end.
    run: Option<Running>,
    /// Set from the moment an unlink begins until it has removed the folder or failed. No task starts for the folder
    /// meanwhile, also after its last task has ended, so the cleanup never leaves a task it does not wait for.
    unlink: Option<Unlink>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Unlink {
    Running,
    /// Its caller stopped waiting before it finished. Another unlink or a sign-out finishes it.
    Abandoned,
}

/// Marks the folder's unlink abandoned if it is dropped while the unlink still runs, that is, when the caller cancels it.
struct Unlinking<'a> {
    driver: &'a Driver,
    space_id: String,
}

impl Drop for Unlinking<'_> {
    fn drop(&mut self) {
        let mut folders = self.driver.inner.folders.lock().unwrap();
        let Some(folder) = folders.get_mut(&self.space_id) else {
            return;
        };
        if folder.unlink == Some(Unlink::Running) {
            folder.unlink = Some(Unlink::Abandoned);
            drop(folders);
            self.driver.changed();
        }
    }
}

struct Running {
    stop: watch::Sender<bool>,
    /// Tells the task that the folder's store holds a new restore request.
    requests: Arc<Notify>,
    /// Closed when the task has ended, its observer and its hold on the folder included.
    done: watch::Receiver<()>,
}

impl Running {
    /// Tells the task to stop and returns what closes once it has.
    fn stop(&self) -> watch::Receiver<()> {
        let _ = self.stop.send(true);
        self.done.clone()
    }

    fn ended(&self) -> bool {
        self.done.has_changed().is_err()
    }
}

/// Waits until a task that [`Running::stop`] told to stop has ended. Any number of callers can wait for one task.
async fn ended(mut done: watch::Receiver<()>) {
    while done.changed().await.is_ok() {}
}

impl Driver {
    /// The folders the profile at `paths` links, read without writing anything: no directory, lock file or store is
    /// created or changed. The stores are read under a shared lock on the profile's lock file when it exists, which keeps
    /// writers out, so a profile that another process holds is refused with [`DriverError::Busy`].
    pub fn linked_folders(paths: &Paths) -> Result<Vec<FolderStatus>, DriverError> {
        let lock_path = paths.lock();
        let _lock = match File::open(&lock_path) {
            Ok(file) => match Flock::share(file).map_err(io_error(&lock_path))? {
                Some(lock) => Some(lock),
                None => return Err(DriverError::Busy(lock_path)),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(io_error(&lock_path)(error)),
        };
        Ok(read_folders(paths, |path| Store::open_read_only(path))?
            .into_iter()
            .map(|(_, status)| status)
            .collect())
    }

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
        let Some(lock) = Flock::take(lock).map_err(io_error(&lock_path))? else {
            return Err(DriverError::Busy(lock_path));
        };
        let config = Config::load(&paths.config_file)?;
        let invalid = |error: LocalRulesError| ConfigError::Invalid {
            path: paths.config_file.clone(),
            reason: error.to_string(),
        };
        let local = local_rules(&config.sync).map_err(invalid)?;
        let matcher = matcher(&config.sync).map_err(invalid)?;
        let stored = Credential::load_stored(&paths.credential())?;
        if let Some(stored) = &stored {
            Credential::keep_out_of_backups(&paths.credential())?;
            if stored.staged.is_some() {
                Credential::keep_out_of_backups(&next_path(&paths.credential()))?;
            }
        }
        let credential_writes = Arc::default();
        let auth = stored
            .as_ref()
            .and_then(|stored| rotating_auth(&paths, stored, &credential_writes).ok());
        let credential = stored.map(|stored| stored.credential);
        let folders = load_folders(&paths)?;
        Ok(Driver {
            inner: Arc::new(Inner {
                matcher,
                local,
                trash_cap: config.trash.cap(),
                upload_gate: UploadGate::new(config.upload.concurrency.get()),
                paths,
                _lock: lock,
                lifecycle: tokio::sync::Mutex::new(()),
                credential: Mutex::new(credential),
                auth: Mutex::new(auth),
                credential_writes,
                folders: Mutex::new(folders),
                running: AtomicBool::new(false),
                changed: Arc::new(watch::Sender::new(())),
                #[cfg(test)]
                observe_fault: None,
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
            .map(|folder| {
                let mut status = folder.status.lock().unwrap().clone();
                if folder.unlink == Some(Unlink::Abandoned) {
                    status.state = FolderState::Halted;
                    status.problem = Some(
                        "unlinking the folder did not finish; unlink it again to finish".into(),
                    );
                }
                status
            })
            .collect();
        Status {
            account,
            running: self.inner.running.load(Ordering::SeqCst),
            folders,
        }
    }

    /// A receiver that is marked changed whenever [`Driver::status`], [`Driver::questions`] or [`Driver::restores`]
    /// may have changed, so the app can wait for a change instead of polling. A change may be signalled without a
    /// visible difference.
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
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let file = NewCredentialFile::create(&self.inner.paths.credential())?;
        self.unlink_folders_of_lost_credential().await?;
        let api = api_for(server)?;
        let issued = api
            .pair_complete(&PairCompleteRequest {
                code: code.to_owned(),
                device_name: device_name.to_owned(),
                platform: std::env::consts::OS.to_owned(),
            })
            .await?;
        self.adopt(
            file,
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
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        if user_name.trim().is_empty() {
            return Err(DriverError::EmptyUserName);
        }
        if device_name.trim().is_empty() {
            return Err(DriverError::EmptyName);
        }
        let file = NewCredentialFile::create(&self.inner.paths.credential())?;
        self.unlink_folders_of_lost_credential().await?;
        let api = api_for(server)?;
        let issued = api
            .claim(&ClaimRequest {
                code: code.to_owned(),
                user_name: user_name.to_owned(),
                device_name: device_name.to_owned(),
                platform: std::env::consts::OS.to_owned(),
                setup_attempt: String::new(),
            })
            .await?;
        self.adopt(
            file,
            api.base().to_string(),
            issued,
            Some(user_name.to_owned()),
            device_name,
            "claimed the server",
        )
    }

    /// Signs this device out: stops syncing, revokes the device on the server so that a copy of the credential stops
    /// working too, unlinks every folder (their files stay) and deletes the credential, so the profile can be paired
    /// or claimed again (RFC 0009 section 5).
    ///
    /// Unless `confirm_last_device` is set, the sign-out goes on only once the server has revoked the device, or
    /// answers that another device already did. Otherwise it keeps the credential and the folders, syncs again if it
    /// was syncing, and returns [`DriverError::LastDevice`] for any HTTP 409, the server's refusal to revoke the
    /// user's last active device, or [`DriverError::NotRevoked`] when the server cannot be reached or does not
    /// answer, so the person can be told that this may be their last device and confirm. With `confirm_last_device`
    /// the revocation is best effort: a sign-out without the server finishes on the device, and the device stays active
    /// there until another device revokes it. A folder that cannot be unlinked stops the sign-out with its error and
    /// stays linked, so signing out again finishes it.
    pub async fn sign_out(&self, confirm_last_device: bool) -> Result<(), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.signing_out(confirm_last_device).await
    }

    /// Signs out like [`Driver::sign_out`] with `confirm_last_device`, but only while the saved credential is
    /// `device_id`'s. A caller that found the server refusing that device's credential uses it, so a credential another
    /// run saved meanwhile is kept and [`DriverError::CredentialReplaced`] names its device.
    pub async fn sign_out_device(&self, device_id: &str) -> Result<(), DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        if let Some(saved) = self.inner.credential.lock().unwrap().as_ref()
            && saved.device_id != device_id
        {
            return Err(DriverError::CredentialReplaced {
                device_id: saved.device_id.clone(),
            });
        }
        self.signing_out(true).await
    }

    /// The sign-out itself, run while the caller holds the lifecycle lock.
    async fn signing_out(&self, confirm_last_device: bool) -> Result<(), DriverError> {
        let was_running = self.inner.running.load(Ordering::SeqCst);
        self.stop().await;
        let signed_in = self.inner.credential.lock().unwrap().clone();
        if let Some(credential) = &signed_in
            && let Err(refused) =
                revoke(credential, self.auth(credential), confirm_last_device).await
        {
            if was_running {
                self.start();
            }
            return Err(refused);
        }
        let roots = roots(&self.inner.folders.lock().unwrap());
        for root in roots {
            self.unlink_folder(&root).await?;
        }
        let path = self.inner.paths.credential();
        let _writes = self.inner.credential_writes.lock().unwrap();
        for path in [next_path(&path), path] {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_error(&path)(error)),
            }
        }
        *self.inner.auth.lock().unwrap() = None;
        *self.inner.credential.lock().unwrap() = None;
        self.inner.running.store(false, Ordering::SeqCst);
        tracing::info!("signed out");
        self.changed();
        Ok(())
    }

    /// Unlinks the folders of a profile that has no credential, before it pairs or claims. Such folders were linked by
    /// a device whose credential is gone, typically on a Mac restored from a backup, which leaves the credential out.
    /// Their stores and owner records belong to that device, so the new device links the folders again; their files
    /// stay.
    async fn unlink_folders_of_lost_credential(&self) -> Result<(), DriverError> {
        let roots = roots(&self.inner.folders.lock().unwrap());
        for root in roots {
            self.unlink_folder(&root).await?;
        }
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

    /// Stores the credential the server issued in `file`, unless another one was stored meanwhile.
    fn adopt(
        &self,
        file: NewCredentialFile,
        server: String,
        issued: DeviceCredentials,
        user_name: Option<String>,
        device_name: &str,
        done: &str,
    ) -> Result<Account, DriverError> {
        let account = self.store_credential(
            Credential {
                server,
                user_id: issued.user_id,
                user_name,
                device_id: issued.device_id,
                device_name: device_name.to_owned(),
                refresh_credential: issued.refresh_credential,
            },
            |credential| file.write(credential),
        )?;
        tracing::info!(
            device_id = account.device_id,
            server = account.server,
            "{done}"
        );
        Ok(account)
    }

    /// Stores a credential this profile was issued elsewhere, such as by `koi setup`'s claim, the same way
    /// [`Driver::pair`] and [`Driver::claim`] store theirs, unless another one was stored meanwhile. Like them, it first
    /// unlinks the folders of a credential that was lost. The file is written before this returns, so a caller can clean
    /// up after it. Like every change to the credential, it waits for the lifecycle lock, so a sign-out that checked the
    /// slot finishes before a new credential appears.
    pub async fn save_credential(&self, credential: Credential) -> Result<Account, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.check_unpaired()?;
        self.unlink_folders_of_lost_credential().await?;
        let path = self.inner.paths.credential();
        self.store_credential(credential, |credential| credential.save_new(&path))
    }

    /// Stores `credential` with `write` unless the slot holds one, while the caller holds the lifecycle lock.
    fn store_credential(
        &self,
        credential: Credential,
        write: impl FnOnce(&Credential) -> Result<(), CredentialError>,
    ) -> Result<Account, DriverError> {
        let mut slot = self.inner.credential.lock().unwrap();
        if let Some(existing) = slot.as_ref() {
            return Err(DriverError::AlreadyPaired {
                device_id: existing.device_id.clone(),
            });
        }
        write(&credential)?;
        let account = credential.account();
        let stored = StoredCredential {
            credential: credential.clone(),
            staged: None,
            changed_at: SystemTime::now(),
        };
        *self.inner.auth.lock().unwrap() =
            rotating_auth(&self.inner.paths, &stored, &self.inner.credential_writes).ok();
        *slot = Some(credential);
        drop(slot);
        self.changed();
        Ok(account)
    }

    /// Links `folder` to the space with id or name `space`: checks the folder, writes its root marker, and creates
    /// its store. The folder starts syncing at once if the driver is running. A folder already linked to that space
    /// stays as it is, and one linked to another space is refused.
    pub async fn link(&self, folder: &Path, space: &str) -> Result<Linked, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let credential = self.credential()?;
        let root = fs::canonicalize(folder).map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => DriverError::FolderNotFound(folder.to_path_buf()),
            _ => io_error(folder)(error),
        })?;
        if !root.is_dir() {
            return Err(DriverError::NotAFolder(root));
        }
        let here = (self.inner.folders.lock().unwrap().values())
            .find(|linked| linked.link.root == root)
            .map(|linked| {
                (
                    linked.link.space_id.clone(),
                    linked.status.lock().unwrap().clone(),
                )
            });
        if let Some((space_id, status)) = here {
            let same = space == space_id || {
                let auth = self.auth(&credential)?;
                resolve_space(auth.api().list_spaces(&auth).await?.spaces, space)?.space_id
                    == space_id
            };
            return match same {
                true => Ok(Linked::Already(status)),
                false => Err(DriverError::FolderOverlaps(root)),
            };
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
        let auth = self.auth(&credential)?;
        let space = resolve_space(auth.api().list_spaces(&auth).await?.spaces, space)?;

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
        tracing::info!(space_id = link.space_id, path = %koidrive_fs::quoted(&link.root), "linked a folder");
        let status = Arc::new(Mutex::new(folder_status(&link, None, 0, 0)));
        let linked = roots(&folders);
        let mut folder = Folder {
            link,
            status: status.clone(),
            run: None,
            unlink: None,
        };
        drop(lock);
        if self.inner.running.load(Ordering::SeqCst) {
            self.spawn(&mut folder, &credential, linked);
        }
        folders.insert(space.space_id, folder);
        drop(folders);
        self.changed();
        let status = status.lock().unwrap().clone();
        Ok(Linked::New(status))
    }

    /// Stops syncing the folder at `folder` and forgets its sync state. The files stay, and so does the folder's
    /// `.koidrive` directory, whose local trash may hold displaced files. If the owner record or the store cannot be
    /// removed, the folder stays linked and halted with the error, so unlinking it again finishes the job.
    pub async fn unlink(&self, folder: &Path) -> Result<Link, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.unlink_folder(folder).await
    }

    async fn unlink_folder(&self, folder: &Path) -> Result<Link, DriverError> {
        let (link, done) = {
            let mut folders = self.inner.folders.lock().unwrap();
            let found = find(&mut folders, folder)?;
            found.unlink = Some(Unlink::Running);
            (found.link.clone(), found.run.as_ref().map(Running::stop))
        };
        let _unlinking = Unlinking {
            driver: self,
            space_id: link.space_id.clone(),
        };
        if let Some(done) = done {
            ended(done).await;
        }
        let forgotten = self.forget(&link);
        let mut folders = self.inner.folders.lock().unwrap();
        match forgotten {
            Ok(()) => {
                folders.remove(&link.space_id);
                tracing::info!(space_id = link.space_id, path = %koidrive_fs::quoted(&link.root), "unlinked a folder");
            }
            Err(ref error) => {
                if let Some(folder) = folders.get_mut(&link.space_id) {
                    folder.run = None;
                    folder.unlink = None;
                    halt(
                        folder,
                        Halted::Unlink,
                        format!("the folder could not be unlinked: {error}"),
                    );
                }
            }
        }
        drop(folders);
        self.changed();
        forgotten.map(|()| link)
    }

    /// Removes the folder's owner record, then its store. The store records the link, so it goes last: until both are
    /// gone, the folder is found again to finish unlinking it. Either one already missing counts as removed.
    fn forget(&self, link: &Link) -> Result<(), DriverError> {
        let owner = Owner {
            server: link.server_url.clone(),
            device_id: link.device_id.clone(),
        };
        match claim::release(&link.root, &owner) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(io_error(&link.root)(error)),
        }
        let store_dir = self
            .inner
            .paths
            .store(&link.space_id)?
            .parent()
            .expect("a store path has a parent")
            .to_path_buf();
        match fs::remove_dir_all(&store_dir) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(&store_dir)(error)),
        }
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

    /// Records the answer to a question. The folder's task takes it as the engine's `UserDecision` input, now if it
    /// runs and otherwise when it next opens the folder. The same answer again, while it waits for the task or after
    /// the engine took it, changes nothing. Another answer is refused, and so is any answer to a question the engine
    /// dropped before it took the answer, as when it asks again about a changed set.
    pub fn answer(&self, question_id: &str, answer: Answer) -> Result<Answered, DriverError> {
        let earlier = |given: &str| match given.parse::<Answer>() {
            Ok(given) if given == answer => Ok(None),
            Ok(given) => Err(DriverError::AnsweredOtherwise { given }),
            Err(_) => Err(DriverError::QuestionNotFound(question_id.to_owned())),
        };
        for space_id in self.space_ids() {
            let path = self.inner.paths.store(&space_id)?;
            let mut store = Store::open(&path).map_err(store_error(&path))?;
            let outcome = store
                .transaction(|tx| {
                    let stored = (tx.questions()?.into_iter())
                        .find(|stored| stored.question_id == question_id);
                    let stored = match stored {
                        None => {
                            return Ok(tx.taken_answer(question_id)?.map(|given| earlier(&given)));
                        }
                        Some(StoredQuestion {
                            answer: Some(given),
                            ..
                        }) => return Ok(Some(earlier(&given.answer))),
                        Some(stored) => stored,
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
                    Ok(Some(Ok(Some(left as u64))))
                })
                .map_err(store_error(&path))?;
            match outcome {
                None => continue,
                Some(Err(error)) => return Err(error),
                Some(Ok(None)) => return Ok(Answered::Already),
                Some(Ok(Some(left))) => {
                    if let Some(folder) = self.inner.folders.lock().unwrap().get(&space_id) {
                        folder.status.lock().unwrap().questions = left;
                        if let Some(run) = &folder.run {
                            run.requests.notify_one();
                        }
                    }
                    tracing::info!(space_id, question_id, %answer, "answered a question");
                    self.changed();
                    return Ok(Answered::Now);
                }
            }
        }
        Err(DriverError::QuestionNotFound(question_id.to_owned()))
    }

    /// Asks for a restore in a linked space: a trashed node back from the trash, or an earlier version of a file
    /// made current. Returns the restore's ID once the request is in the folder's store, where it waits for the
    /// engine, also while the driver is stopped, offline or halted. The engine sends it as a `Restore` op in its next
    /// commit, and [`Driver::restores`] tells how it ended. A request is not sent twice and cannot be taken back.
    ///
    /// A malformed ID is refused here, because the server refuses a whole commit for one. Whether the node or the
    /// version still exists is the server's answer.
    pub fn restore(&self, space_id: &str, request: RestoreRequest) -> Result<String, DriverError> {
        if !self.inner.folders.lock().unwrap().contains_key(space_id) {
            return Err(DriverError::SpaceNotLinked(space_id.to_owned()));
        }
        let intent = request
            .intent()
            .map_err(|malformed| DriverError::InvalidRestore(malformed.0))?;
        let id = restore::new_id(now_ms() as u64);
        let path = self.inner.paths.store(space_id)?;
        Store::open(&path)
            .and_then(|mut store| store.transaction(|tx| tx.request_restore(&id, &intent)))
            .map_err(store_error(&path))?;
        tracing::info!(space_id, restore_id = %id, "recorded a restore request");
        // A task that is not running takes the request when it next opens the folder.
        let folders = self.inner.folders.lock().unwrap();
        if let Some(run) = folders.get(space_id).and_then(|folder| folder.run.as_ref()) {
            run.requests.notify_one();
        }
        drop(folders);
        self.changed();
        Ok(id)
    }

    /// The restores asked for in a linked space: first those that ended, in the order they ended, then those
    /// that wait, in request order. A waiting restore whose node has a problem says so, because it stays pending
    /// until the problem is cleared. The store keeps the last 1,000 that ended and drops older ones.
    pub fn restores(&self, space_id: &str) -> Result<Vec<RestoreStatus>, DriverError> {
        if !self.inner.folders.lock().unwrap().contains_key(space_id) {
            return Err(DriverError::SpaceNotLinked(space_id.to_owned()));
        }
        let path = self.inner.paths.store(space_id)?;
        Store::open(&path)
            .and_then(|mut store| store.transaction(restore::list))
            .map_err(store_error(&path))
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
            self.start_folder(folder, credential.as_ref(), &all);
        }
        drop(folders);
        self.changed();
    }

    /// Syncs the folder at `folder` again from the start: stops its task, waits for it to end, and starts a new one.
    /// A folder halted by a problem only a person can fix stays halted until this is called. While the driver is
    /// stopped, the folder only stops, and the next start syncs it.
    pub async fn restart(&self, folder: &Path) -> Result<FolderStatus, DriverError> {
        self.restart_with(folder, None).await
    }

    /// Lifts the halt a refusal of the server put on the folder at `folder` the way a person chose, and syncs it
    /// again as [`Driver::restart`] does. [`Lift::Resend`] sends the refused commit again, unchanged;
    /// [`Lift::Replan`] plans its changes again into a new commit. A folder with no such halt, or whose halt waits
    /// for the space's owner to confirm the changes, keeps its state, is restarted all the same, and the error
    /// says why nothing was lifted.
    pub async fn lift(&self, folder: &Path, lift: Lift) -> Result<FolderStatus, DriverError> {
        self.restart_with(folder, Some(lift)).await
    }

    /// Stops the task of the folder at `folder`, applies `lift` to its store once the task has ended, and starts
    /// a new task.
    async fn restart_with(
        &self,
        folder: &Path,
        lift: Option<Lift>,
    ) -> Result<FolderStatus, DriverError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let (space_id, done) = {
            let mut folders = self.inner.folders.lock().unwrap();
            let found = find(&mut folders, folder)?;
            (
                found.link.space_id.clone(),
                found.run.as_ref().map(Running::stop),
            )
        };
        if let Some(done) = done {
            ended(done).await;
        }
        let lifted = match lift {
            Some(lift) => (self.inner.paths.store(&space_id))
                .map_err(DriverError::from)
                .and_then(|path| lift_halt(&path, lift)),
            None => Ok(()),
        };
        let credential = self.inner.credential.lock().unwrap().clone();
        let mut folders = self.inner.folders.lock().unwrap();
        let all = roots(&folders);
        let folder = folders
            .get_mut(&space_id)
            .expect("only an unlink removes a folder, and it holds the lifecycle lock");
        folder.run = None;
        folder.status.lock().unwrap().state = FolderState::Stopped;
        if self.inner.running.load(Ordering::SeqCst) {
            self.start_folder(folder, credential.as_ref(), &all);
        }
        let status = folder.status.lock().unwrap().clone();
        drop(folders);
        self.changed();
        lifted.map(|()| status)
    }

    /// Starts the task of `folder` unless it is being unlinked or its task still runs. `all` are the roots of every
    /// linked folder.
    fn start_folder(&self, folder: &mut Folder, credential: Option<&Credential>, all: &[PathBuf]) {
        if folder.unlink.is_some() || folder.run.as_ref().is_some_and(|run| !run.ended()) {
            return;
        }
        let linked = all
            .iter()
            .filter(|root| **root != folder.link.root)
            .cloned()
            .collect();
        match credential {
            Some(credential) => self.spawn(folder, credential, linked),
            None => halt(
                folder,
                Halted::SignedOut,
                "this device is signed out, for example after restoring a backup, which leaves out its sign-in; set it up again to sync this folder, and its files stay where they are".into(),
            ),
        }
    }

    /// Stops every folder and waits for their tasks to end, also those an earlier stop or unlink is still waiting for.
    /// State on disk is kept.
    pub async fn stop(&self) {
        self.inner.running.store(false, Ordering::SeqCst);
        let stopping: Vec<_> = self
            .inner
            .folders
            .lock()
            .unwrap()
            .values()
            .filter_map(|folder| folder.run.as_ref().map(Running::stop))
            .collect();
        for done in stopping {
            ended(done).await;
        }
        for folder in self.inner.folders.lock().unwrap().values_mut() {
            if folder.run.as_ref().is_none_or(Running::ended) {
                folder.run = None;
                folder.status.lock().unwrap().state = FolderState::Stopped;
            }
        }
        self.changed();
    }

    /// Starts the task of `folder`. `linked` are the roots of the profile's other folders, which it must not overlap.
    fn spawn(&self, folder: &mut Folder, credential: &Credential, linked: Vec<PathBuf>) {
        let task = match self.task(folder, credential, linked) {
            Ok(task) => task,
            Err(problem) => return halt(folder, Halted::Start, problem),
        };
        let requests = task.requests.clone();
        let (stop, stopped) = watch::channel(false);
        let (finished, done) = watch::channel(());
        folder.status.lock().unwrap().state = FolderState::Connecting;
        tokio::spawn(async move {
            task.run(stopped).await;
            drop(finished);
        });
        folder.run = Some(Running {
            stop,
            requests,
            done,
        });
    }

    /// The task that syncs `folder`, or the problem that keeps it from starting.
    fn task(
        &self,
        folder: &Folder,
        credential: &Credential,
        linked: Vec<PathBuf>,
    ) -> Result<FolderTask, String> {
        if folder.link.device_id != credential.device_id
            || folder.link.server_url != credential.server
        {
            return Err(format!(
                "the folder was linked by device {} of {}, not by this profile's device {}; unlink it and link it again",
                folder.link.device_id, folder.link.server_url, credential.device_id
            ));
        }
        let auth = self.auth(credential).map_err(|error| error.to_string())?;
        let store = (self.inner.paths)
            .store(&folder.link.space_id)
            .map_err(|error| error.to_string())?;
        Ok(FolderTask {
            link: folder.link.clone(),
            store,
            auth,
            status: folder.status.clone(),
            matcher: self.inner.matcher.clone(),
            local: self.inner.local.clone(),
            trash_cap: self.inner.trash_cap,
            upload_gate: self.inner.upload_gate.clone(),
            private_dirs: self.inner.paths.private_dirs(),
            linked,
            owner: Owner {
                server: credential.server.clone(),
                device_id: credential.device_id.clone(),
            },
            observer: Mutex::default(),
            changed: self.inner.changed.clone(),
            requests: Arc::default(),
            #[cfg(test)]
            observe_fault: self.inner.observe_fault.clone(),
        })
    }

    /// The process's one [`Auth`] for `credential`, the one this profile is signed in with.
    fn auth(&self, credential: &Credential) -> Result<Arc<Auth>, ApiError> {
        let mut slot = self.inner.auth.lock().unwrap();
        if let Some(auth) = slot.as_ref() {
            return Ok(auth.clone());
        }
        let stored = Credential::load_stored(&self.inner.paths.credential())
            .map_err(|error| ApiError::Credential(io::Error::other(error)))?
            .unwrap_or_else(|| StoredCredential {
                credential: credential.clone(),
                staged: None,
                changed_at: SystemTime::now(),
            });
        let auth = rotating_auth(&self.inner.paths, &stored, &self.inner.credential_writes)?;
        *slot = Some(auth.clone());
        Ok(auth)
    }

    /// The profile's one [`Auth`], for calls to the server that the driver does not make itself. They present the
    /// credential that the last rotation left current, like every caller in this process.
    pub fn signed_in(&self) -> Result<Arc<Auth>, DriverError> {
        let credential = self.credential()?;
        Ok(self.auth(&credential)?)
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

/// The linked folder at `folder`, by the path it was linked at or by the folder that path leads to now.
fn find<'a>(
    folders: &'a mut BTreeMap<String, Folder>,
    folder: &Path,
) -> Result<&'a mut Folder, DriverError> {
    let wanted = absolute(folder);
    let canonical = fs::canonicalize(folder).ok();
    folders
        .values_mut()
        .find(|linked| linked.link.root == wanted || Some(&linked.link.root) == canonical.as_ref())
        .ok_or_else(|| DriverError::NotLinked(folder.to_path_buf()))
}

fn halt(folder: &mut Folder, why: Halted, problem: String) {
    tracing::warn!(
        space_id = folder.link.space_id,
        problem,
        "a folder cannot sync"
    );
    let mut status = folder.status.lock().unwrap();
    if status.state != FolderState::Halted {
        failures::failed(Failure::Halted(why));
    }
    status.state = FolderState::Halted;
    status.problem = Some(problem);
}

fn folder_status(
    link: &Link,
    cursor: Option<koidrive_store::Cursor>,
    nodes: u64,
    questions: u64,
) -> FolderStatus {
    FolderStatus {
        space_id: link.space_id.clone(),
        space_name: link.space_name.clone(),
        path: link.root.to_string_lossy().into_owned(),
        state: FolderState::Stopped,
        history_id: cursor.as_ref().map(|cursor| cursor.history_id.clone()),
        last_seq: cursor.map(|cursor| cursor.last_seq),
        head_seq: None,
        remote_nodes: nodes,
        local_entries: None,
        questions,
        problem: None,
        wait: None,
    }
}

/// Every store under the profile's spaces directory that records a link, with the folder's status.
fn load_folders(paths: &Paths) -> Result<BTreeMap<String, Folder>, DriverError> {
    Ok(read_folders(paths, |path| Store::open(path))?
        .into_iter()
        .map(|(link, status)| {
            (
                link.space_id.clone(),
                Folder {
                    status: Arc::new(Mutex::new(status)),
                    link,
                    run: None,
                    unlink: None,
                },
            )
        })
        .collect())
}

/// The link and the status of every store under the profile's spaces directory that records a link, each store opened
/// with `open`.
fn read_folders(
    paths: &Paths,
    open: impl Fn(&Path) -> Result<Store, koidrive_store::Error>,
) -> Result<Vec<(Link, FolderStatus)>, DriverError> {
    let spaces = paths.spaces();
    let entries = match fs::read_dir(&spaces) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error(&spaces)(error)),
    };
    let mut folders = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_error(&spaces))?;
        let Ok(path) = paths.store(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        if !path.exists() {
            continue;
        }
        let mut store = open(&path).map_err(store_error(&path))?;
        let (link, cursor, nodes, questions) = store
            .transaction(|tx| {
                let questions = tx
                    .questions()?
                    .iter()
                    .filter(|q| q.answer.is_none())
                    .count();
                Ok::<_, koidrive_store::Error>((
                    tx.link()?,
                    tx.cursor()?,
                    tx.remote_node_count()?,
                    questions,
                ))
            })
            .map_err(store_error(&path))?;
        let Some(link) = link else {
            tracing::warn!(path = %koidrive_fs::quoted(&path), "a store records no linked folder; ignoring it");
            continue;
        };
        let status = folder_status(&link, cursor, nodes, questions as u64);
        folders.push((link, status));
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

/// The question `stored` holds. A mass-delete question stored before questions kept the limits
/// they crossed names those its counts cross, or the deletions limit when they cross none.
fn decode_question(space_id: &str, stored: &StoredQuestion) -> Option<Question> {
    match serde_json::from_slice::<Question>(&stored.body) {
        Ok(mut question) => {
            if let QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded,
            } = &mut question.kind
                && exceeded.is_empty()
            {
                let window = Window {
                    deletions: *deletions,
                    affected_files: *affected_files,
                    space_files: *space_files,
                };
                *exceeded = window.exceeded();
                if exceeded.is_empty() {
                    exceeded.push(Limit::Deletions);
                }
            }
            if question.lineage.is_empty() {
                question.lineage = question.id.clone();
            }
            Some(question)
        }
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

/// Revokes the credential's own device on the server. An answer that another device already revoked it counts as
/// revoked. Without `confirm_last_device`, any HTTP 409 is the server's refusal to revoke the user's last active device,
/// [`DriverError::LastDevice`], whatever its body, and any other failure, including no answer within [`SERVER_WAIT`],
/// is [`DriverError::NotRevoked`]. With it, every failure, a 409 included, is logged and the device stays active on the
/// server until another device revokes it.
/// An [`Auth`] that owns the profile's credential files and rotates the refresh credential they hold.
fn rotating_auth(
    paths: &Paths,
    stored: &StoredCredential,
    writes: &Arc<Mutex<()>>,
) -> Result<Arc<Auth>, ApiError> {
    let files = CredentialFiles::new(
        &paths.credential(),
        stored.credential.clone(),
        writes.clone(),
    );
    Ok(Arc::new(Auth::rotating(
        stored.credential.api()?,
        stored.net(),
        Arc::new(files),
        ROTATION_PERIOD,
    )))
}

async fn revoke(
    credential: &Credential,
    auth: Result<Arc<Auth>, ApiError>,
    confirm_last_device: bool,
) -> Result<(), DriverError> {
    let revoked = async {
        let auth = auth?;
        auth.api().revoke_self(&auth, confirm_last_device).await
    };
    let failure = match tokio::time::timeout(SERVER_WAIT, revoked).await {
        Ok(Ok(())) => {
            tracing::info!(
                device_id = credential.device_id,
                "revoked this device on the server"
            );
            return Ok(());
        }
        Ok(Err(error)) if !confirm_last_device && error.status() == Some(409) => {
            return Err(DriverError::LastDevice);
        }
        Ok(Err(error)) if error.is_unauthenticated() => {
            tracing::info!(
                device_id = credential.device_id,
                "the server had already revoked this device"
            );
            return Ok(());
        }
        Ok(Err(error)) => error.to_string(),
        Err(_) => format!("no answer within {} seconds", SERVER_WAIT.as_secs()),
    };
    if !confirm_last_device {
        return Err(DriverError::NotRevoked(failure));
    }
    tracing::warn!(
        failure,
        "the server did not revoke this device; revoke it from another device"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use crate::question::{HeldItem, QuestionKind};
    use crate::restore::{RestoreRejection, RestoreState, RestoreWait};

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
        let root = base.join("Koidrive");
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
                Ok::<_, koidrive_store::Error>(())
            })
            .unwrap();
        (dir, paths, root)
    }

    fn mass_delete(id: &str) -> Question {
        Question {
            id: id.into(),
            lineage: id.into(),
            space_id: SPACE.into(),
            asked_at_ms: 10,
            kind: QuestionKind::MassDelete {
                deletions: 1_001,
                affected_files: 1_001,
                space_files: 5_000,
                exceeded: vec![crate::question::Limit::Deletions],
            },
            items: vec![HeldItem {
                node_id: "n1".into(),
                path: "old".into(),
                files: 1_001,
                device_id: None,
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
    fn a_profile_opens_again_while_a_child_process_holds_a_copy_of_its_locks_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let _child = claim::tests::Holder::of(&driver.inner._lock);
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
        assert_eq!(driver.answer("q1", Answer::Restore).unwrap(), Answered::Now);
        assert!(matches!(
            driver.answer("q1", Answer::Delete),
            Err(DriverError::AnsweredOtherwise {
                given: Answer::Restore
            })
        ));
        assert_eq!(driver.questions().unwrap(), [mass_delete("q2")]);
        assert_eq!(driver.status().folders[0].questions, 1);

        let stored = driver.stored_questions(SPACE).unwrap();
        assert_eq!(stored[0].answer.as_ref().unwrap().answer, "restore");
    }

    #[test]
    fn the_same_answer_again_changes_nothing_until_the_engine_drops_the_question_without_it() {
        let (_dir, paths, _root) = profile(&[mass_delete("q1"), mass_delete("q2")]);
        let driver = Driver::open(paths.clone()).unwrap();
        let store = || Store::open(paths.store(SPACE).unwrap()).unwrap();
        assert_eq!(driver.answer("q1", Answer::Delete).unwrap(), Answered::Now);
        assert_eq!(
            driver.answer("q1", Answer::Delete).unwrap(),
            Answered::Already
        );

        assert!(store().transaction(|tx| tx.take_answer("q1")).unwrap());
        assert_eq!(
            driver.answer("q1", Answer::Delete).unwrap(),
            Answered::Already
        );
        assert!(matches!(
            driver.answer("q1", Answer::Restore),
            Err(DriverError::AnsweredOtherwise {
                given: Answer::Delete
            })
        ));

        assert_eq!(driver.answer("q2", Answer::Delete).unwrap(), Answered::Now);
        store()
            .transaction(|tx| {
                tx.remove_question("q2")?;
                tx.put_question(&StoredQuestion {
                    question_id: "q3".into(),
                    asked_at_ms: 20,
                    body: serde_json::to_vec(&mass_delete("q3")).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        assert!(matches!(
            driver.answer("q2", Answer::Delete),
            Err(DriverError::QuestionNotFound(_))
        ));
        assert_eq!(driver.questions().unwrap(), [mass_delete("q3")]);
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

    #[tokio::test]
    async fn a_folder_whose_task_cannot_start_is_counted_as_halted() {
        let halted = Failure::Halted(Halted::Start);
        let before = failures::count(halted);
        let (_dir, paths, _root) = profile(&[]);
        credential("d2").save_new(&paths.credential()).unwrap();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        assert!(failures::count(halted) > before);
        driver.stop().await;
    }

    #[tokio::test]
    async fn a_restart_that_halts_at_once_returns_the_halt() {
        let (_dir, paths, root) = profile(&[]);
        credential("d2").save_new(&paths.credential()).unwrap();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        let restarted = driver.restart(&root).await.unwrap();
        assert_eq!(restarted.state, FolderState::Halted);
        assert!(
            restarted.problem.as_ref().unwrap().contains("device d1"),
            "{restarted:?}"
        );
        driver.stop().await;
    }

    #[tokio::test]
    async fn a_lift_with_nothing_refused_says_so_and_restarts_the_folder() {
        let (_dir, paths, root) = profile(&[]);
        credential("d2").save_new(&paths.credential()).unwrap();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        let error = driver.lift(&root, Lift::Resend).await.unwrap_err();
        assert!(matches!(error, DriverError::NothingToLift), "{error}");
        let folder = &driver.status().folders[0];
        assert_eq!(folder.state, FolderState::Halted);
        assert!(
            folder.problem.as_ref().unwrap().contains("device d1"),
            "{folder:?}"
        );
        driver.stop().await;
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
        linked_to(paths, root, device_id, SPACE);
    }

    /// Links `root` for the paired `device_id` as a folder of `space_id` that passes its root check.
    fn linked_to(paths: &Paths, root: &Path, device_id: &str, space_id: &str) {
        fs::create_dir_all(root).unwrap();
        write_marker(
            root,
            &Marker {
                space_id: space_id.into(),
                effects: 0,
            },
        )
        .unwrap();
        let store_path = paths.store(space_id).unwrap();
        fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        let link = Link {
            space_id: space_id.into(),
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

    /// Waits until `done`. No deadline fails it, since a loaded machine can take any time: a wait that never ends
    /// names `what` each minute, and the CI job's timeout stops it.
    async fn eventually(what: &str, done: impl Fn() -> bool) {
        let mut waited = Instant::now();
        while !done() {
            if waited.elapsed() >= Duration::from_secs(60) {
                eprintln!("still waiting: {what}");
                waited = Instant::now();
            }
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
    async fn the_folders_of_a_profile_share_one_limit_on_uploads_in_flight() {
        use koidrive_proto::v1::ChunkRef;

        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        paired(&paths, "d1");
        linked_to(&paths, &base.join("A"), "d1", SPACE);
        linked_to(
            &paths,
            &base.join("B"),
            "d1",
            "0190f0a0-0000-7000-8000-000000000002",
        );
        fs::create_dir_all(paths.config_file.parent().unwrap()).unwrap();
        fs::write(&paths.config_file, "[upload]\nconcurrency = 3\n").unwrap();
        let driver = Driver::open(paths).unwrap();
        let credential = driver.credential().unwrap();
        let tasks: Vec<_> = (driver.inner.folders.lock().unwrap().values())
            .map(|folder| driver.task(folder, &credential, Vec::new()).unwrap())
            .collect();
        assert_eq!(tasks.len(), 2);

        let (relay, seen) = crate::scripted::relay(3).await;
        let uploads = tasks.iter().enumerate().map(|(folder, task)| async move {
            let (session, _events, _) = crate::scripted::session(relay).await;
            let chunks: Vec<_> = (0..6)
                .map(|i| {
                    let bytes = format!("chunk {i} of folder {folder}");
                    let chunk = ChunkRef {
                        hash: koidrive_chunk::chunk_hash(bytes.as_bytes()).to_vec(),
                        size: bytes.len() as u32,
                        ..Default::default()
                    };
                    (chunk, bytes::Bytes::from(bytes))
                })
                .collect();
            let refs: Vec<_> = chunks.iter().map(|(chunk, _)| chunk.clone()).collect();
            let read = |wanted: &ChunkRef| {
                let found = chunks.iter().find(|(chunk, _)| chunk == wanted).unwrap();
                std::future::ready(Ok(found.1.clone()))
            };
            task.transfer().upload(&session, &refs, read).await.unwrap()
        });
        let receipts = futures_util::future::join_all(uploads).await;

        assert_eq!(receipts.iter().map(Vec::len).sum::<usize>(), 12);
        assert_eq!(seen.lock().unwrap().peak, 3);
    }

    #[tokio::test]
    async fn a_folder_that_holds_a_reopened_profile_halts_before_it_is_observed() {
        let dir = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(dir.path()).unwrap().join("Koidrive");
        let paths = Paths::under(&root.join("moved-profile"));
        linked(&paths, &root, "d1");
        fs::write(root.join("file"), b"synced").unwrap();

        let driver = Driver::open(paths).unwrap();
        driver.start();
        let problem = halted(&driver).await;
        assert!(
            problem.contains("overlaps Koidrive's own files"),
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
        let root = base.join("Koidrive");
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
        let root = base.join("Koidrive");
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
        assert!(problem.contains("another Koidrive process"), "{problem}");
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
        assert!(problem.contains("another Koidrive process"), "{problem}");
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
        let root = base.join("Koidrive");
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

        fs::remove_dir_all(root.join(koidrive_fs::PRIVATE_DIR)).unwrap();
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
        assert!(problem.contains("another Koidrive process"), "{problem}");
        assert_ne!(first.status().folders[0].state, FolderState::Halted);
        second.stop().await;
        first.stop().await;
    }

    #[tokio::test]
    async fn a_folder_replaced_under_its_task_halts_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
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

    #[tokio::test]
    async fn stopping_does_not_wait_for_the_first_scan_to_read_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        for n in 0..16 {
            let file = fs::File::create(root.join(format!("large-{n}"))).unwrap();
            file.set_len(1 << 40).unwrap();
        }
        let mut driver = Driver::open(paths).unwrap();
        let (entered, scanning) = tokio::sync::oneshot::channel();
        Arc::get_mut(&mut driver.inner).unwrap().matcher = Arc::new(Paused {
            entered: Mutex::new(Some(entered)),
            gate: Arc::new((Mutex::new(true), std::sync::Condvar::new())),
        });
        driver.start();
        scanning.await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;

        driver.stop().await;
    }

    /// The observer's first observation fails, and the pause after it lasts a day, so a stop that waits it out never
    /// returns, and a stop that leaves the observer's thread in it leaves the folder locked.
    #[tokio::test]
    async fn stopping_does_not_wait_out_the_observers_pause_after_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let mut driver = Driver::open(paths).unwrap();
        let (paused, pausing) = tokio::sync::oneshot::channel();
        Arc::get_mut(&mut driver.inner).unwrap().observe_fault =
            Some(Arc::new(crate::folder::ObserveFault {
                paused: Mutex::new(Some(paused)),
            }));
        driver.start();
        pausing.await.unwrap();

        driver.stop().await;
        assert!(lockable(&root), "the observer's thread released the folder");
    }

    #[tokio::test]
    async fn a_folder_moved_away_and_back_syncs_again() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;

        fs::rename(&root, base.join("Moved")).unwrap();
        let problem = halted(&driver).await;
        assert!(problem.contains("no longer"), "{problem}");
        fs::rename(base.join("Moved"), &root).unwrap();
        eventually("the folder syncs again", || {
            driver.status().folders[0].state != FolderState::Halted
                && claim::lock(&root).unwrap().is_none()
        })
        .await;
        driver.stop().await;
        assert!(claim::lock(&root).unwrap().is_some());
    }

    #[tokio::test]
    async fn another_folder_with_the_spaces_marker_waits_for_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let original = fs::metadata(&root).unwrap().ino();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        eventually("the folder is claimed", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;

        fs::rename(&root, base.join("Moved")).unwrap();
        fs::create_dir(&root).unwrap();
        write_marker(
            &root,
            &Marker {
                space_id: SPACE.into(),
                effects: 0,
            },
        )
        .unwrap();
        fs::write(root.join("replacement-only.txt"), b"not synced on its own").unwrap();
        assert_ne!(fs::metadata(&root).unwrap().ino(), original);
        eventually("the folder waits for a restart", || {
            driver.status().folders[0]
                .problem
                .as_ref()
                .is_some_and(|problem| problem.contains("koi folder restart"))
        })
        .await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let status = driver.status();
        assert_eq!(status.folders[0].state, FolderState::Halted);
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert!(claim::lock(&root).unwrap().is_some());

        driver.restart(&root).await.unwrap();
        eventually("the restart claims the folder at the path", || {
            claim::owner(&root).unwrap().is_some()
        })
        .await;
        driver.stop().await;
    }

    #[tokio::test]
    async fn a_restart_syncs_a_folder_again_once_its_problem_is_fixed() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let other = Owner {
            server: NOWHERE.into(),
            device_id: "d2".into(),
        };
        claim::set_owner(&root, &other).unwrap();
        let driver = Driver::open(paths).unwrap();
        driver.start();
        let problem = halted(&driver).await;
        assert!(problem.contains("device d2"), "{problem}");

        claim::release(&root, &other).unwrap();
        let restarted = driver.restart(&root).await.unwrap();
        assert_ne!(restarted.state, FolderState::Halted);
        eventually("the folder is claimed by this device", || {
            claim::owner(&root)
                .unwrap()
                .is_some_and(|owner| owner.device_id == "d1")
        })
        .await;
        driver.stop().await;
    }

    /// Reads one HTTP request and returns its path and its body.
    async fn read_request(socket: &mut tokio::net::TcpStream) -> (String, Vec<u8>) {
        use tokio::io::AsyncReadExt;

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
        let path = head.split_whitespace().nth(1).unwrap().to_owned();
        (path, request[head_end..head_end + length].to_vec())
    }

    async fn write_reply(socket: &mut tokio::net::TcpStream, reply: &[u8]) {
        use tokio::io::AsyncWriteExt;

        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            reply.len()
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.write_all(reply).await.unwrap();
    }

    /// Serves one request on a loopback port with `reply`, a protobuf body, and returns the server's address and
    /// what the request was: its path and its body.
    async fn serve_once(reply: Vec<u8>) -> (String, tokio::task::JoinHandle<(String, Vec<u8>)>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_request(&mut socket).await;
            write_reply(&mut socket, &reply).await;
            request
        });
        (server, served)
    }

    /// A request the server has read and not answered yet.
    struct Held {
        /// Fires once the last request has arrived.
        arrived: tokio::sync::oneshot::Receiver<()>,
        /// Lets the server answer it.
        release: tokio::sync::oneshot::Sender<()>,
        /// The paths of the requests, once all are answered.
        served: tokio::task::JoinHandle<Vec<String>>,
    }

    /// Serves one request per entry of `replies` on a loopback port, in order, and holds the answer to the last one
    /// until it is released.
    async fn serve_held(replies: Vec<Vec<u8>>) -> (String, Held) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let (arrived_tx, arrived) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let served = tokio::spawn(async move {
            let mut paths = Vec::new();
            let mut arrived_tx = Some(arrived_tx);
            let mut released = Some(released);
            let last = replies.len() - 1;
            for (i, reply) in replies.into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                paths.push(read_request(&mut socket).await.0);
                if i == last {
                    arrived_tx.take().unwrap().send(()).unwrap();
                    released.take().unwrap().await.unwrap();
                }
                write_reply(&mut socket, &reply).await;
            }
            paths
        });
        (
            server,
            Held {
                arrived,
                release,
                served,
            },
        )
    }

    /// Whether `task` is still waiting a while after it was started.
    async fn still_waiting<T>(task: &tokio::task::JoinHandle<T>) -> bool {
        tokio::time::sleep(Duration::from_millis(200)).await;
        !task.is_finished()
    }

    /// Sets the mode of `dir` and tells whether it keeps this process out: it does not for root. The tests that rely
    /// on it skip when it does not.
    fn close_dir(dir: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
        let probe = dir.join("probe");
        if fs::write(&probe, b"").is_err() {
            return true;
        }
        fs::remove_file(probe).unwrap();
        open_dir(dir);
        assert!(rustix::process::geteuid().is_root());
        eprintln!("SKIPPED: running as root, so permissions do not block writes");
        false
    }

    fn open_dir(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(dir, fs::Permissions::from_mode(0o755)).unwrap();
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
        let root = base.join("Koidrive");
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
        while driver.status().folders[0].local_entries != Some(1) {
            watch.changed().await.unwrap();
        }

        watch.borrow_and_update();
        driver.stop().await;
        assert!(watch.has_changed().unwrap());
    }

    #[tokio::test]
    async fn signing_out_unlinks_every_folder_and_forgets_the_credential() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
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

        driver.sign_out(true).await.unwrap();
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
    async fn saving_a_credential_unlinks_only_the_folders_of_a_lost_one() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        fs::write(root.join("notes.txt"), b"kept").unwrap();
        let driver = Driver::open(paths.clone()).unwrap();

        let refused = driver.save_credential(credential("d2")).await.unwrap_err();

        assert!(
            matches!(&refused, DriverError::AlreadyPaired { device_id } if device_id == "d1"),
            "{refused}"
        );
        assert_eq!(driver.status().folders.len(), 1);
        drop(driver);
        fs::remove_file(paths.credential()).unwrap();
        let driver = Driver::open(paths.clone()).unwrap();

        let account = driver.save_credential(credential("d2")).await.unwrap();

        assert_eq!(account.device_id, "d2");
        assert!(driver.status().folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"kept");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_credential_saved_during_a_stale_sign_out_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        let (held, folders_held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn({
            let driver = driver.clone();
            move || {
                let _folders = driver.inner.folders.lock().unwrap();
                held.send(()).unwrap();
                released.recv().unwrap();
            }
        });
        folders_held.recv().unwrap();
        let stale = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out_device("d1").await }
        });
        while driver.inner.running.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        assert!(driver.inner.lifecycle.try_lock().is_err());
        let save = tokio::spawn({
            let driver = driver.clone();
            async move { driver.save_credential(credential("d2")).await }
        });
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert!(!save.is_finished(), "the save waits for the sign-out");
        assert!(!paths.credential().exists());
        release.send(()).unwrap();
        holder.join().unwrap();
        stale.await.unwrap().unwrap();
        assert_eq!(save.await.unwrap().unwrap().device_id, "d2");
        assert!(paths.credential().exists());
        assert_eq!(driver.status().account.unwrap().device_id, "d2");
    }

    /// Every file and folder under `dir` with its modification time and contents, to show that nothing changed.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
        let mut found = Vec::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(next) = todo.pop() {
            for entry in fs::read_dir(&next).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                let bytes = if meta.is_file() {
                    fs::read(&path).unwrap()
                } else {
                    todo.push(path.clone());
                    Vec::new()
                };
                found.push((path, meta.modified().unwrap(), bytes));
            }
        }
        found.sort();
        found
    }

    #[test]
    fn linked_folders_are_read_without_writing_to_the_profile() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        drop(Driver::open(paths.clone()).unwrap());
        let before = snapshot(&base.join("profile"));

        let folders = Driver::linked_folders(&paths).unwrap();

        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].space_id, SPACE);
        assert_eq!(folders[0].path, root.display().to_string());
        assert_eq!(snapshot(&base.join("profile")), before);

        let held = Driver::open(paths.clone()).unwrap();
        assert!(matches!(
            Driver::linked_folders(&paths),
            Err(DriverError::Busy(_))
        ));
        drop(held);

        let fresh = Paths::under(&base.join("fresh"));
        assert!(Driver::linked_folders(&fresh).unwrap().is_empty());
        assert!(!base.join("fresh").exists());
    }

    #[tokio::test]
    async fn signing_out_a_refused_device_keeps_a_credential_saved_for_another() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d2");
        let driver = Driver::open(paths.clone()).unwrap();

        let error = driver.sign_out_device("d1").await.unwrap_err();

        assert!(
            matches!(&error, DriverError::CredentialReplaced { device_id } if device_id == "d2"),
            "{error}"
        );
        assert!(paths.credential().exists());
        let status = driver.status();
        assert_eq!(status.account.unwrap().device_id, "d2");
        assert_eq!(status.folders.len(), 1);
    }

    /// Serves one request per entry of `replies`, an HTTP status and a protobuf body, on a loopback port, in order,
    /// and returns each request's path with whether `credential` still existed when the request arrived.
    async fn serve_each(
        replies: Vec<(u16, Vec<u8>)>,
        credential: PathBuf,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, bool)>>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (path, _) = read_request(&mut socket).await;
                requests.push((path, credential.exists()));
                let head = format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            requests
        });
        (server, served)
    }

    fn access_token() -> (u16, Vec<u8>) {
        let token = koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        };
        (200, prost::Message::encode_to_vec(&token))
    }

    /// The server's refusal to revoke the user's last active device without a confirmation.
    fn last_device_refused() -> (u16, Vec<u8>) {
        let refusal = koidrive_proto::v1::Error {
            code: koidrive_proto::v1::error::Code::ConfirmationRequired.into(),
            message: "this is the user's last active device; confirm to revoke it".into(),
            ..Default::default()
        };
        (409, prost::Message::encode_to_vec(&refusal))
    }

    /// A profile in `base` paired as `d1` with the server at `server`, with a folder linked at `base/Koidrive`.
    fn linked_with_server(base: &Path, server: String) -> Paths {
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &base.join("Koidrive"), "d1");
        fs::remove_file(paths.credential()).unwrap();
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        paths
    }

    #[tokio::test]
    async fn a_refused_last_device_keeps_its_credential_and_folders_until_it_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let credential_path = Paths::under(&base.join("profile")).credential();
        let (server, served) = serve_each(
            vec![access_token(), last_device_refused(), (204, Vec::new())],
            credential_path,
        )
        .await;
        let paths = linked_with_server(&base, server);
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();

        assert!(matches!(
            driver.sign_out(false).await,
            Err(DriverError::LastDevice)
        ));
        let status = driver.status();
        assert_eq!(status.account.unwrap().device_id, "d1");
        assert_eq!(status.folders.len(), 1);
        assert!(status.running);
        assert!(paths.credential().exists());

        driver.sign_out(true).await.unwrap();
        let status = driver.status();
        assert_eq!(status.account, None);
        assert!(status.folders.is_empty());
        assert!(!paths.credential().exists());
        assert_eq!(
            served.await.unwrap(),
            [
                ("/v1/token".to_owned(), true),
                ("/v1/devices/d1".to_owned(), true),
                ("/v1/devices/d1?confirm=last-device".to_owned(), true),
            ]
        );
    }

    #[tokio::test]
    async fn a_device_that_is_not_the_last_revokes_itself_without_confirming() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let credential_path = Paths::under(&base.join("profile")).credential();
        let (server, served) =
            serve_each(vec![access_token(), (204, Vec::new())], credential_path).await;
        let paths = linked_with_server(&base, server);
        let driver = Driver::open(paths.clone()).unwrap();

        driver.sign_out(false).await.unwrap();
        assert_eq!(driver.status().account, None);
        assert!(!paths.credential().exists());
        let requested: Vec<String> = served
            .await
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(requested, ["/v1/token", "/v1/devices/d1"]);
    }

    #[tokio::test]
    async fn signing_out_without_the_server_waits_for_a_confirmation_and_then_finishes_on_the_device()
     {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();

        assert!(matches!(
            driver.sign_out(false).await,
            Err(DriverError::NotRevoked(_))
        ));
        assert_eq!(driver.status().account.unwrap().device_id, "d1");
        assert_eq!(driver.status().folders.len(), 1);
        assert!(paths.credential().exists());

        driver.sign_out(true).await.unwrap();
        assert_eq!(driver.status().account, None);
        assert!(driver.status().folders.is_empty());
        assert!(!paths.credential().exists());
    }

    /// Answers a token request, then answers the revocation with `raw`, the bytes of an HTTP response, and keeps that
    /// connection open until the test ends if `hold` is set.
    async fn revocation_answered_with(raw: &'static [u8], hold: bool) -> String {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            write_reply(&mut socket, &access_token().1).await;
            let (mut socket, _) = listener.accept().await.unwrap();
            read_request(&mut socket).await;
            socket.write_all(raw).await.unwrap();
            if hold {
                std::future::pending::<()>().await;
            }
        });
        server
    }

    #[tokio::test]
    async fn any_409_or_a_stalled_answer_keeps_the_credential_and_the_folders() {
        const EMPTY: &[u8] =
            b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
        const HTML: &[u8] = b"HTTP/1.1 409 Conflict\r\ncontent-type: text/html\r\ncontent-length: 13\r\nconnection: close\r\n\r\n<html></html>";
        const TRUNCATED: &[u8] = b"HTTP/1.1 409 Conflict\r\ncontent-type: application/x-protobuf\r\ncontent-length: 100\r\nconnection: close\r\n\r\n\x08\x10";
        // The status of a 409 whose body cannot be read may be lost with the body; either refusal keeps everything.
        let cases = [
            ("an empty 409", EMPTY, false, true),
            ("an HTML 409", HTML, false, true),
            ("a truncated 409", TRUNCATED, false, false),
            ("a stalled 409", TRUNCATED, true, false),
        ];
        for (case, raw, hold, status_kept) in cases {
            let dir = tempfile::tempdir().unwrap();
            let base = fs::canonicalize(dir.path()).unwrap();
            let server = revocation_answered_with(raw, hold).await;
            let paths = linked_with_server(&base, server);
            let driver = Driver::open(paths.clone()).unwrap();

            let refused = driver.sign_out(false).await.unwrap_err();
            let expected = matches!(refused, DriverError::LastDevice)
                || !status_kept && matches!(refused, DriverError::NotRevoked(_));
            assert!(expected, "{case}: {refused:?}");
            assert!(paths.credential().exists(), "{case}");
            assert_eq!(driver.status().folders.len(), 1, "{case}");
        }
    }

    #[tokio::test]
    async fn a_confirmed_sign_out_that_gets_a_409_finishes_on_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let server = revocation_answered_with(
            b"HTTP/1.1 409 Conflict\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            false,
        )
        .await;
        let paths = linked_with_server(&base, server);
        let driver = Driver::open(paths.clone()).unwrap();

        driver.sign_out(true).await.unwrap();
        assert_eq!(driver.status().account, None);
        assert!(driver.status().folders.is_empty());
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn signing_out_waits_for_a_claim_in_flight_and_then_undoes_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let (server, held) = serve_held(vec![issued("d1")]).await;
        let claim = tokio::spawn({
            let driver = driver.clone();
            async move { driver.claim(&server, "code", "Ada", "laptop").await }
        });
        held.arrived.await.unwrap();
        let sign_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(true).await }
        });
        assert!(still_waiting(&sign_out).await);

        held.release.send(()).unwrap();
        assert_eq!(claim.await.unwrap().unwrap().device_id, "d1");
        sign_out.await.unwrap().unwrap();
        assert_eq!(held.served.await.unwrap(), ["/v1/claim"]);
        assert_eq!(driver.status().account, None);
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn a_pair_during_a_claim_keeps_the_claimed_credential() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let (server, held) = serve_held(vec![issued("d1")]).await;
        let claim = tokio::spawn({
            let driver = driver.clone();
            async move { driver.claim(&server, "code", "Ada", "laptop").await }
        });
        held.arrived.await.unwrap();
        let (pair_server, pair_served) = serve_once(issued("d2")).await;
        let pair = tokio::spawn({
            let driver = driver.clone();
            async move { driver.pair(&pair_server, "code", "desktop").await }
        });
        assert!(still_waiting(&pair).await);

        held.release.send(()).unwrap();
        assert_eq!(claim.await.unwrap().unwrap().device_id, "d1");
        assert!(matches!(
            pair.await.unwrap(),
            Err(DriverError::AlreadyPaired { device_id }) if device_id == "d1"
        ));
        assert!(!pair_served.is_finished(), "the pair reached the server");
        pair_served.abort();
        held.served.await.unwrap();
        drop(driver);
        let reopened = Driver::open(paths).unwrap().status();
        assert_eq!(reopened.account.unwrap().device_id, "d1");
    }

    #[tokio::test]
    async fn signing_out_waits_for_a_link_in_flight_and_then_unlinks_it() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let token = prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        });
        let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let (server, held) = serve_held(vec![token, spaces]).await;
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let link = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.link(&root, "Projects").await }
        });
        held.arrived.await.unwrap();
        let sign_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(true).await }
        });
        assert!(still_waiting(&sign_out).await);

        held.release.send(()).unwrap();
        link.await.unwrap().unwrap();
        sign_out.await.unwrap().unwrap();
        assert_eq!(held.served.await.unwrap(), ["/v1/token", "/v1/spaces"]);
        let status = driver.status();
        assert_eq!(status.account, None);
        assert!(status.folders.is_empty());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn a_folder_whose_owner_record_stays_keeps_the_device_signed_in_until_a_retry_removes_it()
    {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        claim::set_owner(
            &root,
            &Owner {
                server: NOWHERE.into(),
                device_id: "d1".into(),
            },
        )
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        let mut watch = driver.watch();
        let private = root.join(koidrive_fs::PRIVATE_DIR);
        if !close_dir(&private) {
            return;
        }

        let failed = driver.sign_out(true).await;
        open_dir(&private);
        assert!(matches!(failed, Err(DriverError::Io { .. })), "{failed:?}");
        assert!(watch.has_changed().unwrap());
        watch.borrow_and_update();
        let status = driver.status();
        assert_eq!(status.account.unwrap().device_id, "d1");
        assert_eq!(status.folders[0].state, FolderState::Halted);
        let problem = status.folders[0].problem.clone().unwrap();
        assert!(problem.contains("could not be unlinked"), "{problem}");
        assert!(paths.credential().exists());
        assert!(paths.store(SPACE).unwrap().exists());
        assert!(claim::owner(&root).unwrap().is_some());

        driver.sign_out(true).await.unwrap();
        assert!(watch.has_changed().unwrap());
        assert!(driver.status().folders.is_empty());
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert!(!paths.credential().exists());
    }

    #[tokio::test]
    async fn a_failed_unlink_wakes_watchers_and_can_be_retried() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &root, "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        let mut watch = driver.watch();
        let store_dir = paths.store(SPACE).unwrap().parent().unwrap().to_path_buf();
        if !close_dir(&store_dir) {
            return;
        }

        let failed = driver.unlink(&root).await;
        open_dir(&store_dir);
        assert!(matches!(failed, Err(DriverError::Io { .. })), "{failed:?}");
        assert!(watch.has_changed().unwrap());
        watch.borrow_and_update();
        assert_eq!(driver.status().folders[0].state, FolderState::Halted);
        assert!(paths.store(SPACE).unwrap().exists());

        driver.unlink(&root).await.unwrap();
        assert!(watch.has_changed().unwrap());
        assert!(driver.status().folders.is_empty());
        assert!(!store_dir.exists());
    }

    /// Holds the observer of a folder inside its first matcher call until it is opened or dropped.
    struct Gate(Arc<(Mutex<bool>, std::sync::Condvar)>);

    impl Gate {
        fn open(&self) {
            *self.0.0.lock().unwrap() = true;
            self.0.1.notify_all();
        }
    }

    impl Drop for Gate {
        fn drop(&mut self) {
            self.open();
        }
    }

    struct Paused {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        gate: Arc<(Mutex<bool>, std::sync::Condvar)>,
    }

    impl Matcher for Paused {
        fn is_excluded(&self, _: &Path, _: bool) -> bool {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            let (open, opened) = &*self.gate;
            let _open = opened
                .wait_while(open.lock().unwrap(), |open| !*open)
                .unwrap();
            false
        }
    }

    /// A started driver whose one folder's observer is paused, holding the folder's lock, until the gate opens.
    async fn paused_driver(paths: &Paths, root: &Path) -> (Driver, Gate) {
        linked(paths, root, "d1");
        fs::write(root.join("file"), b"mine").unwrap();
        let mut driver = Driver::open(paths.clone()).unwrap();
        let (entered, observing) = tokio::sync::oneshot::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        Arc::get_mut(&mut driver.inner).unwrap().matcher = Arc::new(Paused {
            entered: Mutex::new(Some(entered)),
            gate: gate.clone(),
        });
        driver.start();
        observing.await.unwrap();
        (driver, Gate(gate))
    }

    /// Whether this process could lock `root` now, which it cannot while a folder task holds it.
    fn lockable(root: &Path) -> bool {
        claim::lock(root).unwrap().is_some()
    }

    /// Checks that signing out finished: no account, no folders, no running tasks, nothing left in the profile or
    /// the folder.
    fn signed_out(driver: &Driver, paths: &Paths, root: &Path) {
        let status = driver.status();
        assert_eq!(status.account, None);
        assert!(status.folders.is_empty());
        assert!(!status.running);
        assert!(!paths.credential().exists());
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        assert_eq!(claim::owner(root).unwrap(), None);
        assert!(lockable(root));
        assert_eq!(fs::read(root.join("file")).unwrap(), b"mine");
    }

    #[tokio::test]
    async fn signing_out_waits_for_a_stop_already_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let stop = tokio::spawn({
            let driver = driver.clone();
            async move { driver.stop().await }
        });
        eventually("the stop begins", || !driver.status().running).await;
        let sign_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(true).await }
        });
        assert!(still_waiting(&sign_out).await);
        assert!(!lockable(&root));

        gate.open();
        stop.await.unwrap();
        sign_out.await.unwrap().unwrap();
        signed_out(&driver, &paths, &root);
    }

    #[tokio::test]
    async fn signing_out_finishes_a_cancelled_unlink() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let unlink = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.unlink(&root).await }
        });
        assert!(still_waiting(&unlink).await);
        unlink.abort();
        assert!(unlink.await.unwrap_err().is_cancelled());
        assert_eq!(driver.status().folders.len(), 1);
        let sign_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(true).await }
        });
        assert!(still_waiting(&sign_out).await);

        gate.open();
        sign_out.await.unwrap().unwrap();
        signed_out(&driver, &paths, &root);
    }

    #[tokio::test]
    async fn no_task_starts_while_an_unlink_is_unfinished() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let unlink = tokio::spawn({
            let driver = driver.clone();
            let root = root.clone();
            async move { driver.unlink(&root).await }
        });
        assert!(still_waiting(&unlink).await);
        unlink.abort();
        assert!(unlink.await.unwrap_err().is_cancelled());
        gate.open();
        driver.stop().await;
        assert!(lockable(&root));

        driver.start();
        let folder = &driver.status().folders[0];
        assert_eq!(
            folder.state,
            FolderState::Halted,
            "a task started for a folder whose unlink is unfinished"
        );
        assert!(folder.problem.as_ref().unwrap().contains("unlink it again"));
        driver.sign_out(true).await.unwrap();
        signed_out(&driver, &paths, &root);
    }

    #[tokio::test]
    async fn a_start_during_a_sign_out_leaves_the_driver_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        let paths = Paths::under(&base.join("profile"));
        let (driver, gate) = paused_driver(&paths, &root).await;
        let sign_out = tokio::spawn({
            let driver = driver.clone();
            async move { driver.sign_out(true).await }
        });
        eventually("the sign-out stops the driver", || !driver.status().running).await;
        driver.start();

        gate.open();
        sign_out.await.unwrap().unwrap();
        signed_out(&driver, &paths, &root);
    }

    #[tokio::test]
    async fn a_profile_restored_without_its_credential_pairs_as_a_new_device_and_links_its_folder_again()
     {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("notes.txt"), b"kept").unwrap();
        let paths = Paths::under(&base.join("profile"));
        let token = || {
            prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
                token: "token".into(),
                expires_at_ms: i64::MAX,
                ..Default::default()
            })
        };
        let spaces = || {
            prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
                spaces: vec![SpaceInfo {
                    space_id: SPACE.into(),
                    name: "Projects".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        };
        let (server, held) = serve_held(vec![token(), spaces()]).await;
        held.release.send(()).unwrap();
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        driver.link(&root, "Projects").await.unwrap();
        drop(driver);
        fs::remove_file(paths.credential()).unwrap();

        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        let status = driver.status();
        assert_eq!(status.account, None);
        assert_eq!(status.folders[0].state, FolderState::Halted);
        let problem = status.folders[0].problem.clone().unwrap();
        assert!(problem.contains("signed out"), "{problem}");
        driver.stop().await;

        let (server, held) = serve_held(vec![issued("d2"), token(), spaces()]).await;
        held.release.send(()).unwrap();
        driver.pair(&server, "code", "Mac").await.unwrap();
        assert!(driver.status().folders.is_empty());
        assert_eq!(claim::owner(&root).unwrap(), None);
        assert!(!paths.store(SPACE).unwrap().parent().unwrap().exists());
        driver.link(&root, "Projects").await.unwrap();
        let status = driver.status();
        assert_eq!(status.account.unwrap().device_id, "d2");
        assert_eq!(status.folders.len(), 1);
        assert_eq!(fs::read(root.join("notes.txt")).unwrap(), b"kept");
        assert_eq!(
            held.served.await.unwrap(),
            ["/v1/pair/complete", "/v1/token", "/v1/spaces"]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn opening_the_profile_excludes_its_credential_from_time_machine_again() {
        use crate::credential::BACKUP_EXCLUSION;
        use crate::credential::tests::time_machine_excludes;

        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let paths = Paths::under(home.path());
        paired(&paths, "d1");
        assert!(time_machine_excludes(&paths.credential()));
        rustix::fs::removexattr(paths.credential(), BACKUP_EXCLUSION.0).unwrap();
        assert!(!time_machine_excludes(&paths.credential()));

        drop(Driver::open(paths.clone()).unwrap());
        assert!(time_machine_excludes(&paths.credential()));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn opening_refuses_a_credential_it_cannot_keep_out_of_backups() {
        use crate::credential::tests::time_machine_excludes;
        use crate::credential::{BACKUP_EXCLUSION, CredentialError};

        let home = tempfile::tempdir_in(std::env::var_os("HOME").unwrap()).unwrap();
        let paths = Paths::under(home.path());
        paired(&paths, "d1");
        rustix::fs::removexattr(paths.credential(), BACKUP_EXCLUSION.0).unwrap();
        struct Unlock(std::path::PathBuf);
        impl Drop for Unlock {
            fn drop(&mut self) {
                let status = std::process::Command::new("chflags")
                    .arg("nouchg")
                    .arg(&self.0)
                    .status();
                if !std::thread::panicking() {
                    assert!(status.unwrap().success());
                }
            }
        }
        let status = std::process::Command::new("chflags")
            .arg("uchg")
            .arg(paths.credential())
            .status()
            .unwrap();
        assert!(status.success());
        let unlock = Unlock(paths.credential());
        let refused = Driver::open(paths.clone()).err();
        drop(unlock);
        let error = refused.expect("the driver opened with a credential that backups would copy");
        assert!(
            matches!(
                error,
                DriverError::Credential(CredentialError::NotExcludedFromBackups { .. })
            ),
            "{error}"
        );
        assert!(error.to_string().contains("Time Machine"), "{error}");
        assert!(error.to_string().contains("restart Koidrive"), "{error}");

        drop(Driver::open(paths.clone()).unwrap());
        assert!(time_machine_excludes(&paths.credential()));
    }

    #[tokio::test]
    async fn a_credential_that_backups_would_copy_is_refused_before_the_server_uses_up_the_code() {
        use crate::credential::tests::REFUSE_EXCLUSION;

        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let (server, served) = serve_once(issued("d1")).await;
        REFUSE_EXCLUSION.set(true);
        let claimed = driver.claim(&server, "claim-code", "Ada", "laptop").await;
        let paired = driver.pair(&server, "pair-code", "laptop").await;
        REFUSE_EXCLUSION.set(false);
        for refused in [claimed.err(), paired.err()] {
            assert!(
                matches!(
                    refused,
                    Some(DriverError::Credential(
                        CredentialError::NotExcludedFromBackups { .. }
                    ))
                ),
                "{refused:?}"
            );
        }
        let state = fs::read_dir(paths.credential().parent().unwrap()).unwrap();
        let left: Vec<_> = state.map(|entry| entry.unwrap().file_name()).collect();
        assert!(
            !left
                .iter()
                .any(|name| name.to_string_lossy().contains("credential")),
            "{left:?}"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), served)
                .await
                .is_err(),
            "the server was asked for a credential"
        );
    }

    /// Serves one request per entry of `replies`, in order, and returns each request's path and body.
    async fn serve_recorded(
        replies: Vec<(u16, Vec<u8>)>,
    ) -> (String, tokio::task::JoinHandle<Vec<(String, Vec<u8>)>>) {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = format!("http://{}/", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                requests.push(read_request(&mut socket).await);
                let head = format!(
                    "HTTP/1.1 {status} Reply\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            requests
        });
        (server, served)
    }

    /// A token answer of a server with RFC 0011, at server time `server_time_ms`.
    fn rotation_answer(server_time_ms: i64, rotated: bool) -> (u16, Vec<u8>) {
        let token = koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            server_time_ms,
            rotated,
        };
        (200, prost::Message::encode_to_vec(&token))
    }

    fn token_request(body: &[u8]) -> koidrive_proto::v1::TokenRequest {
        prost::Message::decode(body).unwrap()
    }

    #[tokio::test]
    async fn a_rotation_reaches_linking_and_signing_out_through_the_one_auth() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![SpaceInfo {
                space_id: SPACE.into(),
                name: "Projects".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let (server, served) = serve_recorded(vec![
            rotation_answer(5_000, false),
            rotation_answer(5_000, true),
            (200, spaces),
            (204, Vec::new()),
        ])
        .await;
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let two_days_ago = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        OpenOptions::new()
            .write(true)
            .open(paths.credential())
            .unwrap()
            .set_modified(two_days_ago)
            .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();

        driver.link(&root, "Projects").await.unwrap();
        let rotated = Credential::load(&paths.credential())
            .unwrap()
            .unwrap()
            .refresh_credential;
        assert_ne!(rotated, "secret");
        assert!(!next_path(&paths.credential()).exists());
        driver.sign_out(true).await.unwrap();

        let requests = served.await.unwrap();
        let paths_requested: Vec<_> = requests.iter().map(|(path, _)| path.as_str()).collect();
        assert_eq!(
            paths_requested[..3],
            ["/v1/token", "/v1/token", "/v1/spaces"]
        );
        assert!(
            paths_requested[3].starts_with("/v1/devices/d1"),
            "{paths_requested:?}"
        );
        let proposal = token_request(&requests[1].1);
        assert_eq!(proposal.refresh_credential, "secret");
        assert_eq!(proposal.next_refresh_credential, rotated);
        assert_eq!(proposal.rotate_before_ms, 5_000 + 60_000);
    }

    #[tokio::test]
    async fn a_crash_between_the_two_writes_of_a_promotion_ends_with_one_file() {
        for stored_again in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let answer = if stored_again {
                rotation_answer(500, true)
            } else {
                rotation_answer(2_000, false)
            };
            let (server, served) = serve_recorded(vec![answer]).await;
            let promoted = Credential {
                server,
                refresh_credential: "promoted".into(),
                ..credential("d1")
            };
            promoted.save_new(&paths.credential()).unwrap();
            let staged = koidrive_net::Staged {
                refresh_credential: "promoted".into(),
                rotate_before_ms: 1_000,
            };
            koidrive_net::CredentialStore::stage(
                &CredentialFiles::new(&paths.credential(), promoted.clone(), Arc::default()),
                &staged,
            )
            .unwrap();
            let driver = Driver::open(paths.clone()).unwrap();

            driver
                .auth(&promoted)
                .unwrap()
                .access_token()
                .await
                .unwrap();
            let request = token_request(&served.await.unwrap()[0].1);
            assert_eq!(request.refresh_credential, "promoted");
            assert_eq!(request.next_refresh_credential, "promoted");
            assert!(!next_path(&paths.credential()).exists(), "{stored_again}");
            let stored = Credential::load_stored(&paths.credential())
                .unwrap()
                .unwrap();
            assert_eq!(stored.credential.refresh_credential, "promoted");
            assert_eq!(stored.staged, None);
        }
    }

    #[tokio::test]
    async fn an_auth_built_after_open_starts_from_the_stored_slots() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let (server, served) = serve_recorded(vec![rotation_answer(500, true)]).await;
        let current = Credential {
            server,
            refresh_credential: "current".into(),
            ..credential("d1")
        };
        current.save_new(&paths.credential()).unwrap();
        koidrive_net::CredentialStore::stage(
            &CredentialFiles::new(&paths.credential(), current.clone(), Arc::default()),
            &koidrive_net::Staged {
                refresh_credential: "staged".into(),
                rotate_before_ms: 1_000,
            },
        )
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        *driver.inner.auth.lock().unwrap() = None;

        driver.auth(&current).unwrap().access_token().await.unwrap();
        let request = token_request(&served.await.unwrap()[0].1);
        assert_eq!(request.refresh_credential, "current");
        assert_eq!(request.next_refresh_credential, "staged");
        let stored = Credential::load_stored(&paths.credential())
            .unwrap()
            .unwrap();
        assert_eq!(stored.credential.refresh_credential, "staged");
        assert_eq!(stored.staged, None);
    }

    #[tokio::test]
    async fn signing_out_removes_the_staged_credential_too() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        paired(&paths, "d1");
        let stored = Credential::load(&paths.credential()).unwrap().unwrap();
        koidrive_net::CredentialStore::stage(
            &CredentialFiles::new(&paths.credential(), stored, Arc::default()),
            &koidrive_net::Staged {
                refresh_credential: "next".into(),
                rotate_before_ms: 1,
            },
        )
        .unwrap();
        let driver = Driver::open(paths.clone()).unwrap();
        driver.sign_out(true).await.unwrap();
        assert!(!paths.credential().exists());
        assert!(!next_path(&paths.credential()).exists());
    }

    #[tokio::test]
    async fn linking_refuses_a_folder_of_another_device_or_another_process() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let root = base.join("Koidrive");
        fs::create_dir(&root).unwrap();
        let paths = Paths::under(&base.join("profile"));
        paired(&paths, "d2");
        let driver = Driver::open(paths).unwrap();
        fs::create_dir(root.join(koidrive_fs::PRIVATE_DIR)).unwrap();
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
    async fn linking_a_folder_to_the_space_it_syncs_changes_nothing_and_to_another_is_refused() {
        let (_dir, paths, root) = profile(&[]);
        let token = prost::Message::encode_to_vec(&koidrive_proto::v1::AccessToken {
            token: "token".into(),
            expires_at_ms: i64::MAX,
            ..Default::default()
        });
        let spaces = prost::Message::encode_to_vec(&koidrive_proto::v1::SpaceList {
            spaces: vec![
                SpaceInfo {
                    space_id: SPACE.into(),
                    name: "Projects".into(),
                    ..Default::default()
                },
                SpaceInfo {
                    space_id: "0190f0a0-0000-7000-8000-000000000002".into(),
                    name: "Notes".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        let (server, served) =
            serve_recorded(vec![(200, token), (200, spaces.clone()), (200, spaces)]).await;
        Credential {
            server,
            ..credential("d1")
        }
        .save_new(&paths.credential())
        .unwrap();
        let driver = Driver::open(paths).unwrap();
        let before = driver.status().folders;

        assert_eq!(
            driver.link(&root, SPACE).await.unwrap(),
            Linked::Already(before[0].clone())
        );
        assert_eq!(
            driver.link(&root, "Projects").await.unwrap(),
            Linked::Already(before[0].clone())
        );
        assert!(matches!(
            driver.link(&root, "Notes").await,
            Err(DriverError::FolderOverlaps(path)) if path == root
        ));
        assert_eq!(driver.status().folders, before);
        assert_eq!(read_marker(&root).unwrap(), None);
        let paths: Vec<String> = (served.await.unwrap().into_iter())
            .map(|(path, _)| path)
            .collect();
        assert_eq!(paths, ["/v1/token", "/v1/spaces", "/v1/spaces"]);
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

    /// A node or version ID as the server gives it.
    fn server_id(n: u128) -> String {
        koidrive_engine::ids::uuid_v7(1, n)
    }

    fn from_trash(node: u128) -> RestoreRequest {
        RestoreRequest::FromTrash {
            node_id: server_id(node),
            parent_id: None,
            name: None,
        }
    }

    #[test]
    fn a_restore_request_survives_a_reopen_and_reads_as_pending() {
        let (_dir, paths, _root) = profile(&[]);
        let driver = Driver::open(paths.clone()).unwrap();
        let mut watch = driver.watch();
        watch.mark_unchanged();
        let placed = RestoreRequest::FromTrash {
            node_id: server_id(1),
            parent_id: Some(server_id(2)),
            name: Some("back.txt".into()),
        };
        let version = RestoreRequest::Version {
            node_id: server_id(3),
            version_id: server_id(4),
        };
        assert_eq!(driver.restores(SPACE).unwrap(), []);
        let first = driver.restore(SPACE, placed.clone()).unwrap();
        assert!(watch.has_changed().unwrap());
        std::thread::sleep(Duration::from_millis(2));
        let second = driver.restore(SPACE, version.clone()).unwrap();
        assert!(first < second, "the IDs sort in request order");
        drop(driver);

        // Nothing ran the engine in between: the driver was stopped the whole time.
        let driver = Driver::open(paths).unwrap();
        let pending = |id: &str, request: &RestoreRequest| RestoreStatus {
            id: id.to_owned(),
            request: request.clone(),
            state: RestoreState::Pending { waits_for: None },
        };
        assert_eq!(
            driver.restores(SPACE).unwrap(),
            [pending(&first, &placed), pending(&second, &version)]
        );
    }

    #[test]
    fn restore_refuses_a_malformed_id_and_an_unlinked_space() {
        let (_dir, paths, _root) = profile(&[]);
        let driver = Driver::open(paths).unwrap();
        let other_space = "0190f0a0-0000-7000-8000-000000000002";
        assert!(matches!(
            driver.restore(other_space, from_trash(1)),
            Err(DriverError::SpaceNotLinked(space)) if space == other_space
        ));
        assert!(matches!(
            driver.restores(other_space),
            Err(DriverError::SpaceNotLinked(_))
        ));
        let version = |node_id: &str, version_id: &str| RestoreRequest::Version {
            node_id: node_id.into(),
            version_id: version_id.into(),
        };
        let placed = |parent_id: &str| RestoreRequest::FromTrash {
            node_id: server_id(1),
            parent_id: Some(parent_id.into()),
            name: None,
        };
        let cases = [
            (version("report.txt", &server_id(2)), "node_id"),
            (version(&server_id(1), ""), "version_id"),
            (version(&server_id(1), "v2"), "version_id"),
            (
                placed("0190F3A0-1234-7FFF-BFFF-FFFFFFFFFFFF"),
                "target_parent_id",
            ),
        ];
        for (request, field) in cases {
            assert!(matches!(
                driver.restore(SPACE, request),
                Err(DriverError::InvalidRestore(found)) if found == field
            ));
        }
        assert_eq!(
            driver.restores(SPACE).unwrap(),
            [],
            "a commit with a malformed ID would halt the space, so none is recorded"
        );
    }

    #[test]
    fn restores_lists_what_ended_and_then_what_waits() {
        use koidrive_engine::restore::{Outcome, Rejection, Resolved, RestoreIntent};

        let (_dir, paths, _root) = profile(&[]);
        let driver = Driver::open(paths.clone()).unwrap();
        let in_inbox = driver.restore(SPACE, from_trash(9)).unwrap();
        let intent = |node: u128| RestoreIntent::from_trash(server_id(node), None, None).unwrap();
        let outcomes = [
            (Outcome::Restored, RestoreState::Restored),
            (
                Outcome::Renamed { name: "b".into() },
                RestoreState::Renamed { name: "b".into() },
            ),
            (
                Outcome::Copied {
                    node: server_id(50),
                    name: "c".into(),
                },
                RestoreState::Copied {
                    node_id: server_id(50),
                    name: "c".into(),
                },
            ),
            (
                Outcome::Rejected(Rejection::NotFound),
                RestoreState::Rejected {
                    reason: RestoreRejection::NotFound,
                },
            ),
            (
                Outcome::Rejected(Rejection::ReadOnly),
                RestoreState::Rejected {
                    reason: RestoreRejection::ReadOnly,
                },
            ),
            (
                Outcome::Rejected(Rejection::Other("LATER".into())),
                RestoreState::Rejected {
                    reason: RestoreRejection::Other("LATER".into()),
                },
            ),
            (Outcome::DoesNotFit, RestoreState::DoesNotFit),
        ];
        // The engine's side of the store, as the folder task saves it: one restore queued, the rest ended, the
        // newest first by ID so that the listed order is the order they ended in.
        let mut state = koidrive_store::EngineState::default();
        state.commits.restores.insert("queued".into(), intent(8));
        for (n, (outcome, _)) in outcomes.iter().enumerate() {
            state.commits.resolved.push(Resolved {
                id: format!("ended-{}", 9 - n),
                intent: intent(n as u128),
                outcome: outcome.clone(),
            });
        }
        let path = paths.store(SPACE).unwrap();
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();

        let listed = driver.restores(SPACE).unwrap();
        let ids: Vec<&str> = listed.iter().map(|restore| restore.id.as_str()).collect();
        let ended = (0..outcomes.len()).map(|n| format!("ended-{}", 9 - n));
        let expected: Vec<String> = ended.chain([in_inbox, "queued".into()]).collect();
        assert_eq!(ids, expected);
        for (restore, (_, state)) in listed.iter().zip(&outcomes) {
            assert_eq!(&restore.state, state);
        }
        assert_eq!(listed[1].request, from_trash(1));
        for waiting in &listed[outcomes.len()..] {
            assert_eq!(waiting.state, RestoreState::Pending { waits_for: None });
        }
    }

    /// A running folder takes a request when it is recorded, without a restart. Nothing listens at the server's
    /// address, so the folder stays offline, and the request is then queued in the engine and no longer in the
    /// inbox.
    #[tokio::test]
    async fn a_restore_asked_for_while_the_folder_runs_reaches_its_engine() {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        linked(&paths, &base.join("Koidrive"), "d1");
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        eventually("the folder's task runs its loop", || {
            driver.status().folders[0].state == FolderState::Offline
        })
        .await;

        let id = driver.restore(SPACE, from_trash(1)).unwrap();
        let path = paths.store(SPACE).unwrap();
        eventually("the engine holds the request", || {
            let (inbox, (queued, _)) = Store::open(&path)
                .unwrap()
                .transaction(|tx| {
                    Ok::<_, koidrive_store::Error>((tx.restore_requests()?, tx.restores()?))
                })
                .unwrap();
            inbox.is_empty() && queued.contains_key(&id)
        })
        .await;
        let listed = driver.restores(SPACE).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].state, RestoreState::Pending { waits_for: None });
        driver.stop().await;
    }

    #[test]
    fn a_restore_held_by_a_problem_node_reads_as_pending_with_the_reason() {
        use koidrive_engine::restore::RestoreIntent;

        let (_dir, paths, _root) = profile(&[]);
        let driver = Driver::open(paths.clone()).unwrap();
        // The engine's side of the store: the server refused a change to node 1, which is a problem since, and
        // the engine holds a restore of an earlier version of it and a restore of node 2.
        let mut state = koidrive_store::EngineState::default();
        state.commits.problems.insert(server_id(1));
        let held = RestoreIntent::version(server_id(1), server_id(40)).unwrap();
        let free = RestoreIntent::from_trash(server_id(2), None, None).unwrap();
        state.commits.restores.insert("a-held".into(), held);
        state.commits.restores.insert("b-free".into(), free);
        let path = paths.store(SPACE).unwrap();
        Store::open(&path)
            .unwrap()
            .transaction(|tx| tx.set_engine_state(&state))
            .unwrap();
        // A request the engine has not taken yet waits for the same reason.
        let asked = driver.restore(SPACE, from_trash(1)).unwrap();

        let listed = driver.restores(SPACE).unwrap();
        let states: Vec<_> = listed.iter().map(|r| (r.id.as_str(), &r.state)).collect();
        let problem = RestoreState::Pending {
            waits_for: Some(RestoreWait::NodeProblem),
        };
        let free = RestoreState::Pending { waits_for: None };
        assert_eq!(
            states,
            [
                (asked.as_str(), &problem),
                ("a-held", &problem),
                ("b-free", &free)
            ]
        );
        assert_eq!(
            serde_json::to_value(&problem).unwrap(),
            serde_json::json!({ "type": "pending", "waits_for": "node_problem" })
        );
    }

    /// A mass-delete question stored before questions kept the limits they crossed names the
    /// limits its counts cross, or the deletions limit when they cross none.
    #[test]
    fn a_question_stored_without_its_limits_names_them() {
        let decoded = |deletions, affected_files, space_files| {
            let question = Question {
                id: "q1".into(),
                space_id: SPACE.into(),
                asked_at_ms: 1,
                kind: QuestionKind::MassDelete {
                    deletions,
                    affected_files,
                    space_files,
                    exceeded: Vec::new(),
                },
                items: Vec::new(),
                lineage: "q1".into(),
            };
            let body = serde_json::to_string(&question).unwrap();
            let stored = StoredQuestion {
                question_id: "q1".into(),
                asked_at_ms: 1,
                body: body.replace(r#","exceeded":[]"#, "").into_bytes(),
                answer: None,
            };
            match decode_question(SPACE, &stored).unwrap().kind {
                QuestionKind::MassDelete { exceeded, .. } => exceeded,
                kind => panic!("a mass-delete question: {kind:?}"),
            }
        };
        assert_eq!(
            decoded(1_200, 1_500, 2_000),
            [Limit::Deletions, Limit::Share]
        );
        assert_eq!(decoded(3, 30, 40), [Limit::Share]);
        assert_eq!(decoded(3, 3, 40), [Limit::Deletions]);
    }
}
