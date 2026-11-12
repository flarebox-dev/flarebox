// SPDX-License-Identifier: Apache-2.0

//! The daemon's local socket. Every command is a [`Request`] that [`handle`] runs against a [`Driver`]: the daemon's
//! driver through the socket when the daemon runs, or a driver the command opens itself when it does not. One JSON
//! line goes each way per connection. The socket is the owner's only: its directory is 0700, the socket 0600, and
//! the daemon also refuses a peer running as another user.

use std::collections::BTreeSet;
use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use koidrive_driver::{
    Account, Answer, Answered, Credential, Driver, DriverError, FolderState, FolderStatus, Linked,
    Paths, Question, RestoreRequest, RestoreStatus, Status,
};
use koidrive_engine::commit::Lift;
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::error::{CliError, Code};
use crate::report::Next;
use crate::request::{self, Begun, Key, RequestId};
use crate::service::Blocked;

/// The longest request or response line the socket accepts.
const MAX_LINE: u64 = 16 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    Status,
    Pair {
        server: String,
        code: String,
        name: String,
    },
    /// Claims a new server with the one-time code from its log.
    Claim {
        server: String,
        code: String,
        user_name: String,
        name: String,
    },
    /// Stores the credential that `koi setup` claimed.
    SaveCredential {
        credential: Credential,
    },
    /// Removes the credential of `device_id`, which the server no longer accepts, unlinking every folder; their files
    /// stay. Revoking the device on the server is best effort, since the server already refuses the credential. A
    /// credential saved for another device meanwhile is kept.
    SignOut {
        device_id: String,
    },
    Link {
        folder: PathBuf,
        space: String,
    },
    Unlink {
        folder: PathBuf,
    },
    Restart {
        folder: PathBuf,
    },
    /// `Driver::lift`: `Lift::Replan` when `replan`, else `Lift::Resend`.
    Lift {
        folder: PathBuf,
        replan: bool,
    },
    Questions,
    /// `Driver::answered_question`.
    AnsweredQuestion {
        question_id: String,
    },
    Answer {
        question_id: String,
        answer: Answer,
    },
    /// Records a restore in the folder of `space_id`, for its engine to send. `space_id`, `folder` and `path`, the folder's
    /// root and the path in it, come back in the response, so that the record of a request ID can report the restore again
    /// when the folder is no longer linked.
    Restore {
        space_id: String,
        restore: RestoreRequest,
        folder: String,
        path: String,
    },
    Restores {
        space_id: String,
    },
    /// The linked folders that this process may not read.
    Access,
    /// Runs `run` once for `request_id`: a later request with the same ID and key gets the first response, and one
    /// with another key fails as stale (see [`request`]).
    Once {
        request_id: RequestId,
        key: Key,
        run: Box<Request>,
    },
}

impl Request {
    /// This request run once for `request_id`, or as it is without one.
    pub fn once(self, request_id: Option<RequestId>, key: Key) -> Request {
        match request_id {
            Some(request_id) => Request::Once {
                request_id,
                key,
                run: Box::new(self),
            },
            None => self,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Status {
        status: Status,
    },
    Paired {
        account: Account,
    },
    Linked {
        folder: FolderStatus,
    },
    /// The folder was already linked to the space.
    AlreadyLinked {
        folder: FolderStatus,
    },
    Unlinked {
        space_id: String,
        path: PathBuf,
    },
    /// The folder to unlink was not linked.
    NotLinked {
        path: PathBuf,
    },
    Restarted {
        folder: FolderStatus,
    },
    Questions {
        questions: Vec<Question>,
    },
    AnsweredQuestion {
        question: Option<Question>,
    },
    Answered,
    /// The decision was answered before with the same answer.
    AlreadyAnswered,
    Restore {
        id: String,
        space_id: String,
        folder: String,
        path: String,
    },
    Restores {
        restores: Vec<RestoreStatus>,
    },
    SignedOut,
    Access {
        blocked: Vec<Blocked>,
    },
    /// A failed request. An older daemon sends only `message`, which reads as [`Code::Failed`].
    Error {
        message: String,
        #[serde(default)]
        code: Code,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        next: Vec<Next>,
    },
}

impl Response {
    fn failed(error: CliError) -> Self {
        Response::Error {
            message: error.message,
            code: error.code,
            next: error.next,
        }
    }
}

/// Runs `request` unless an earlier run of `request_id` recorded its response, and records the response. A response
/// the CLI reports as a failure removes the record instead, so the next run with the ID tries again: an error, where the
/// driver fails before its change or rolls it back, and a restart or lift that left the folder halted, which is safe
/// to repeat once the cause is fixed. A run cancelled midway leaves the record without an outcome. The record is written in the
/// process that runs the driver, so a client that stops waiting does not keep the outcome from being recorded.
async fn once(driver: &Driver, request_id: RequestId, key: Key, request: Request) -> Response {
    let claim = match request::begin(driver.paths(), &request_id, &key) {
        Ok(Begun::Claimed(claim)) => claim,
        Ok(Begun::Done(recorded)) => {
            return serde_json::from_value(recorded).unwrap_or_else(|error| {
                Response::failed(CliError::from(format!(
                    "the record of the request {request_id} cannot be read: {error}"
                )))
            });
        }
        Err(error) => return Response::failed(error),
    };
    let response = Box::pin(handle(driver, request)).await;
    let failed = match &response {
        Response::Error { .. } => true,
        Response::Restarted { folder } => folder.state == FolderState::Halted,
        _ => false,
    };
    if failed {
        claim.abandon();
    } else if let Err(error) = claim.finish(&response) {
        tracing::warn!(%request_id, error = error.message, "could not record the outcome of a request");
    }
    response
}

/// Runs one request.
pub async fn handle(driver: &Driver, request: Request) -> Response {
    let outcome: Result<Response, DriverError> = match request {
        Request::Status => Ok(Response::Status {
            status: driver.status(),
        }),
        Request::Pair { server, code, name } => driver
            .pair(&server, &code, &name)
            .await
            .map(|account| Response::Paired { account }),
        Request::Claim {
            server,
            code,
            user_name,
            name,
        } => driver
            .claim(&server, &code, &user_name, &name)
            .await
            .map(|account| Response::Paired { account }),
        Request::SaveCredential { credential } => driver
            .save_credential(credential)
            .await
            .map(|account| Response::Paired { account }),
        Request::SignOut { device_id } => driver
            .sign_out_device(&device_id)
            .await
            .map(|()| Response::SignedOut),
        Request::Link { folder, space } => {
            driver
                .link(&folder, &space)
                .await
                .map(|linked| match linked {
                    Linked::New(folder) => Response::Linked { folder },
                    Linked::Already(folder) => Response::AlreadyLinked { folder },
                })
        }
        Request::Unlink { folder } => match driver.unlink(&folder).await {
            Ok(link) => Ok(Response::Unlinked {
                space_id: link.space_id,
                path: link.root,
            }),
            Err(DriverError::NotLinked(path)) => Ok(Response::NotLinked { path }),
            Err(error) => Err(error),
        },
        Request::Restart { folder } => driver
            .restart(&folder)
            .await
            .map(|folder| Response::Restarted { folder }),
        Request::Lift { folder, replan } => {
            let lift = if replan { Lift::Replan } else { Lift::Resend };
            (driver.lift(&folder, lift).await).map(|folder| Response::Restarted { folder })
        }
        Request::Questions => driver
            .questions()
            .map(|questions| Response::Questions { questions }),
        Request::AnsweredQuestion { question_id } => driver
            .answered_question(&question_id)
            .map(|question| Response::AnsweredQuestion { question }),
        Request::Answer {
            question_id,
            answer,
        } => driver
            .answer(&question_id, answer)
            .map(|answered| match answered {
                Answered::Now => Response::Answered,
                Answered::Already => Response::AlreadyAnswered,
            }),
        Request::Restore {
            space_id,
            restore,
            folder,
            path,
        } => driver
            .restore(&space_id, restore)
            .map(|id| Response::Restore {
                id,
                space_id,
                folder,
                path,
            }),
        Request::Restores { space_id } => driver
            .restores(&space_id)
            .map(|restores| Response::Restores { restores }),
        Request::Access => Ok(Response::Access {
            blocked: unreadable(driver.status().folders).await,
        }),
        Request::Once {
            request_id,
            key,
            run,
        } => return once(driver, request_id, key, *run).await,
    };
    outcome.unwrap_or_else(|error| Response::failed(error.into()))
}

/// The folders whose read by [`unreadable`] has not ended.
static READING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The linked folders this process may not read. macOS keeps a launch agent out of the folders that its privacy
/// settings guard, such as Documents, until the person allows it, and asks while the first read waits. Each read
/// runs on a thread of its own, which a read waiting for the person keeps until the answer, so the runtime never waits
/// for it. A folder whose earlier read still waits gets no second thread, however often the status is asked for.
async fn unreadable(folders: Vec<FolderStatus>) -> Vec<Blocked> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let reads: Vec<_> = (folders.into_iter())
        .map(|folder| {
            let (send, receive) = tokio::sync::oneshot::channel();
            if READING.lock().unwrap().insert(folder.path.clone()) {
                let path = folder.path.clone();
                std::thread::spawn(move || {
                    let read = fs::read_dir(&path).map(drop);
                    READING.lock().unwrap().remove(&path);
                    send.send(read)
                });
            }
            (folder.path, receive)
        })
        .collect();
    let mut blocked = Vec::new();
    for (folder, read) in reads {
        match tokio::time::timeout_at(deadline, read).await {
            Err(_) | Ok(Err(_)) => blocked.push(Blocked {
                folder,
                waiting: true,
            }),
            Ok(Ok(Err(error))) if error.raw_os_error() == Some(Errno::PERM.raw_os_error()) => {
                blocked.push(Blocked {
                    folder,
                    waiting: false,
                })
            }
            Ok(Ok(_)) => {}
        }
    }
    blocked
}

/// Binds the daemon's socket. The caller holds the profile lock, so a socket file left behind is stale.
///
/// The directory must belong to this user and be closed to everyone else, so no other user can reach the socket even
/// before its mode is set. The socket is bound under a temporary name and renamed once it is 0600, so it never
/// appears at `path` with another mode. Setting the umask instead would change it for every thread of the process.
/// The temporary name is `.tmp` and the process id, at most 11 bytes like `daemon.sock`, so it fits the socket address
/// limit wherever the socket path does. A socket path too long for that limit is refused, since no client could
/// connect to it.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    std::os::unix::net::SocketAddr::from_pathname(path)?;
    let dir = path.parent().unwrap_or(Path::new("."));
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let metadata = fs::metadata(dir)?;
    let refuse = |reason: String| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {reason}", dir.display()),
        )
    };
    let uid = rustix::process::getuid().as_raw();
    if metadata.uid() != uid {
        return Err(refuse("belongs to another user".to_owned()));
    }
    let mode = metadata.mode() & 0o7777;
    if mode & 0o077 != 0 {
        return Err(refuse(format!(
            "is open to other users (mode {mode:o}); make it 0700"
        )));
    }
    #[cfg(target_os = "macos")]
    if acl::allows_others(dir, uid)? {
        return Err(refuse(
            "lets other users in through an ACL; remove it with chmod -N".to_owned(),
        ));
    }
    let temp = dir.join(format!(".tmp{}", std::process::id()));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // A socket left by an earlier version can carry an inherited deny-delete entry, which blocks the rename.
    #[cfg(target_os = "macos")]
    let _ = acl::clear(path);
    let listener = bind_socket(&temp)
        .and_then(|listener| fs::rename(&temp, path).map(|()| listener))
        .inspect_err(|_| {
            let _ = fs::remove_file(&temp);
        })?;
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// Creates the socket 0600 before binding it, so it is closed to other users from its first moment even if the
/// directory was swapped after it was checked. Linux takes the new file's mode from the socket, so no chmod by path is
/// needed. One could follow a symlink planted in a swapped directory.
#[cfg(target_os = "linux")]
fn bind_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    rustix::fs::fchmod(&socket, rustix::fs::Mode::from_raw_mode(0o600))?;
    rustix::net::bind(&socket, &SocketAddrUnix::new(path)?)?;
    rustix::net::listen(&socket, 1024)?;
    Ok(socket.into())
}

/// macOS takes the new socket file's mode from the umask, so the file is set to 0600 right after binding, without
/// following a symlink, and any ACL it inherited from the directory is cleared, since an inherited deny-delete entry
/// would block replacing and removing it. Until then the directory check keeps other users out.
#[cfg(target_os = "macos")]
fn bind_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    use rustix::fs::{AtFlags, CWD, Mode};
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    rustix::fs::chmodat(
        CWD,
        path,
        Mode::from_raw_mode(0o600),
        AtFlags::SYMLINK_NOFOLLOW,
    )?;
    acl::clear(path)?;
    Ok(listener)
}

#[cfg(target_os = "macos")]
mod acl {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    const ID_TYPE_UID: c_int = 0;
    unsafe extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_get_file(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_set_link_np(path: *const c_char, kind: c_int, acl: *mut c_void) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> c_int;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
    }

    /// Removes every extended ACL entry from `path`, without following a symlink.
    pub fn clear(path: &Path) -> io::Result<()> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid C string, and the empty ACL from acl_init is freed once after use.
        unsafe {
            let empty = acl_init(0);
            if empty.is_null() {
                return Err(io::Error::last_os_error());
            }
            let result = acl_set_link_np(path.as_ptr(), ACL_TYPE_EXTENDED, empty);
            let error = io::Error::last_os_error();
            acl_free(empty);
            if result != 0 {
                return Err(error);
            }
        }
        Ok(())
    }

    /// Whether the extended ACL of `dir` has an allow entry for anyone but `owner`. Such an entry grants access the
    /// mode bits do not show. Deny entries only take access away.
    pub fn allows_others(dir: &Path, owner: u32) -> io::Result<bool> {
        let path = CString::new(dir.as_os_str().as_bytes())?;
        // SAFETY: `path` is a valid C string for the duration of the call.
        let acl = unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let error = io::Error::last_os_error();
            return match error.kind() {
                io::ErrorKind::NotFound => Ok(false),
                _ => Err(error),
            };
        }
        let mut allows_others = false;
        let mut entry = std::ptr::null_mut();
        let mut which = ACL_FIRST_ENTRY;
        // SAFETY: `acl` is a live ACL from acl_get_file, `entry` points into it, and each qualifier is freed once.
        unsafe {
            while acl_get_entry(acl, which, &mut entry) == 0 {
                which = ACL_NEXT_ENTRY;
                let mut tag = 0;
                if acl_get_tag_type(entry, &mut tag) == 0 && tag != ACL_EXTENDED_ALLOW {
                    continue;
                }
                let qualifier = acl_get_qualifier(entry);
                let (mut id, mut id_type) = (0, -1);
                let owner_only = !qualifier.is_null()
                    && mbr_uuid_to_id(qualifier.cast(), &mut id, &mut id_type) == 0
                    && id_type == ID_TYPE_UID
                    && id == owner;
                if !qualifier.is_null() {
                    acl_free(qualifier);
                }
                if !owner_only {
                    allows_others = true;
                    break;
                }
            }
            acl_free(acl);
        }
        Ok(allows_others)
    }
}

/// Serves requests until the task is dropped.
pub async fn serve(listener: UnixListener, driver: Driver) {
    let uid = rustix::process::getuid().as_raw();
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                tracing::warn!(%error, "accepting a connection on the socket failed");
                continue;
            }
        };
        match stream.peer_cred() {
            Ok(peer) if peer.uid() == uid => {}
            peer => {
                tracing::warn!(?peer, "refused a connection from another user");
                continue;
            }
        }
        let driver = driver.clone();
        tokio::spawn(async move {
            if let Err(error) = answer(stream, &driver).await {
                tracing::debug!(%error, "a socket connection failed");
            }
        });
    }
}

async fn answer(stream: UnixStream, driver: &Driver) -> io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let line = read_line(reader).await?;
    let response = match serde_json::from_str::<Request>(&line) {
        Ok(request) => handle(driver, request).await,
        Err(error) => Response::failed(CliError::usage(format!(
            "the request is not valid: {error}"
        ))),
    };
    write_line(&mut writer, &response).await
}

/// Sends a request to the daemon at `socket`, or returns `None` when no daemon listens there. A socket served by
/// anyone but the user `owner` gets nothing: requests carry pairing codes and the credential.
pub async fn send(socket: &Path, request: &Request, owner: u32) -> io::Result<Option<Response>> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let peer = stream.peer_cred()?.uid();
    if peer != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("it is served by another user (uid {peer})"),
        ));
    }
    let (reader, mut writer) = stream.into_split();
    write_line(&mut writer, request).await?;
    let line = read_line(reader).await?;
    serde_json::from_str(&line)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Runs a request in the daemon, or returns `None` when no daemon answers on the profile's socket.
pub async fn ask(paths: &Paths, request: &Request) -> Result<Option<Response>, String> {
    let socket = paths.socket();
    send(&socket, request, rustix::process::getuid().as_raw())
        .await
        .map_err(|error| format!("the daemon's socket {} failed: {error}", socket.display()))
}

/// The error for a profile that a process holds while its socket does not answer.
pub fn busy(paths: &Paths) -> String {
    format!(
        "a Koidrive process holds this profile, but its socket {} does not answer",
        paths.socket().display()
    )
}

/// Runs `request` in the daemon, or in a driver of its own when no daemon runs. A failed request is an error. Without
/// a daemon and without a state directory, the status and the questions, answered or not, are read as a profile that is not set up,
/// without opening a driver, so reading them creates nothing.
pub async fn run(paths: &Paths, request: Request) -> Result<Response, CliError> {
    let response = match ask(paths, &request).await? {
        Some(response) => response,
        None if matches!(request, Request::Status) && !paths.state_dir.exists() => {
            Response::Status {
                status: Status::default(),
            }
        }
        None if matches!(request, Request::Questions) && !paths.state_dir.exists() => {
            Response::Questions {
                questions: Vec::new(),
            }
        }
        None if matches!(request, Request::AnsweredQuestion { .. })
            && !paths.state_dir.exists() =>
        {
            Response::AnsweredQuestion { question: None }
        }
        None => {
            let driver = Driver::open(paths.clone()).map_err(|error| match error {
                DriverError::Busy(_) => CliError::from(busy(paths)),
                other => CliError::from(other),
            })?;
            handle(&driver, request).await
        }
    };
    match response {
        Response::Error {
            message,
            code,
            next,
        } => Err(CliError {
            next,
            ..CliError::new(code, message)
        }),
        response => Ok(response),
    }
}

async fn read_line(reader: impl tokio::io::AsyncRead + Unpin) -> io::Result<String> {
    let mut line = String::new();
    BufReader::new(reader.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    Ok(line)
}

async fn write_line(
    writer: &mut (impl tokio::io::AsyncWrite + Unpin),
    value: &impl Serialize,
) -> io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.shutdown().await
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[tokio::test]
    async fn requests_reach_the_daemon_through_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let driver = Driver::open(paths.clone()).unwrap();
        let server = tokio::spawn(serve(bind(&paths.socket()).unwrap(), driver.clone()));
        let mode = fs::metadata(paths.socket()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let response = run(&paths, Request::Status).await.unwrap();
        assert_eq!(
            response,
            Response::Status {
                status: driver.status()
            }
        );
        let error = run(
            &paths,
            Request::Answer {
                question_id: "q1".into(),
                answer: Answer::Delete,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message,
            "no unanswered question has the id \"q1\"; koi resolve lists the decisions waiting now"
        );
        assert_eq!(error.code, Code::StaleDecision);
        assert_eq!(error.next, [Next::of(&["koi", "resolve", "--json"])]);
        server.abort();
    }

    #[test]
    fn an_error_from_an_older_daemon_reads_as_unclassified() {
        let response: Response =
            serde_json::from_str(r#"{"response":"error","message":"it failed"}"#).unwrap();
        assert_eq!(
            response,
            Response::Error {
                message: "it failed".into(),
                code: Code::Failed,
                next: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn without_a_daemon_a_command_opens_the_profile_itself() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let response = run(&paths, Request::Questions).await.unwrap();
        assert_eq!(response, Response::Questions { questions: vec![] });

        let _held = Driver::open(paths.clone()).unwrap();
        let error = run(&paths, Request::Status).await.unwrap_err();
        assert!(error.message.contains("does not answer"), "{error:?}");
    }

    #[tokio::test]
    async fn a_socket_served_by_another_user_gets_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("daemon.sock");
        let daemon = UnixListener::bind(&socket).unwrap();
        let received = tokio::spawn(async move {
            let (stream, _) = daemon.accept().await.unwrap();
            read_line(stream).await.unwrap()
        });
        let someone_else = rustix::process::getuid().as_raw() + 1;
        let request = Request::Pair {
            server: "https://example.com".into(),
            code: "AAAA-BBBB".into(),
            name: "laptop".into(),
        };
        let error = send(&socket, &request, someone_else).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("another user"), "{error}");
        assert_eq!(received.await.unwrap(), "");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_socket_appears_with_mode_0600_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run/daemon.sock");
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = std::thread::spawn({
            let run = dir.path().join("run");
            let stop = stop.clone();
            move || {
                let (mut at_socket, mut anywhere) = (BTreeSet::new(), BTreeSet::new());
                while !stop.load(Ordering::Relaxed) {
                    for entry in fs::read_dir(&run).into_iter().flatten().flatten() {
                        if let Ok(metadata) = entry.metadata() {
                            let mode = metadata.permissions().mode() & 0o777;
                            if entry.file_name() == "daemon.sock" {
                                at_socket.insert(mode);
                            }
                            anywhere.insert(mode);
                        }
                    }
                }
                (at_socket, anywhere)
            }
        });
        for _ in 0..200 {
            drop(bind(&socket).unwrap());
        }
        stop.store(true, Ordering::Relaxed);
        let (at_socket, anywhere) = watcher.join().unwrap();
        assert_eq!(at_socket, BTreeSet::from([0o600]));
        if cfg!(target_os = "linux") {
            assert_eq!(anywhere, BTreeSet::from([0o600]));
        }
        let names: Vec<_> = fs::read_dir(dir.path().join("run"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["daemon.sock"]);
        let mode = fs::metadata(dir.path().join("run"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[tokio::test]
    async fn a_directory_other_users_can_enter_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        fs::create_dir(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o755)).unwrap();
        let error = bind(&run.join("daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("open to other users"), "{error}");
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_socket_path_too_long_to_connect_to_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let limit = if cfg!(target_os = "macos") { 104 } else { 108 };
        let fixed = dir.path().as_os_str().len() + "/".len() + "/daemon.sock".len();
        let run = dir.path().join("r".repeat(limit - fixed));
        let socket = run.join("daemon.sock");
        assert_eq!(socket.as_os_str().len(), limit);
        let error = bind(&socket).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!run.exists());
    }

    #[tokio::test]
    async fn a_directory_of_another_user_gets_no_socket() {
        if rustix::process::getuid().is_root() {
            return;
        }
        let error = bind(Path::new("/daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error.to_string().contains("belongs to another user"),
            "{error}"
        );
    }

    #[cfg(target_os = "macos")]
    fn add_acl_entries(dir: &Path, entries: &[&str]) {
        for entry in entries {
            let status = std::process::Command::new("/bin/chmod")
                .arg("+a")
                .arg(entry)
                .arg(dir)
                .status()
                .unwrap();
            assert!(status.success(), "chmod +a {entry:?}");
        }
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_lets_everyone_in_gets_no_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        add_acl_entries(
            &run,
            &[
                "everyone allow list,search",
                "everyone allow read,write,append,file_inherit,only_inherit",
            ],
        );
        let error = bind(&run.join("daemon.sock")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("through an ACL"), "{error}");
        assert_eq!(fs::read_dir(&run).unwrap().count(), 0);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_only_denies_or_allows_the_owner_gets_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        let user = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        let user = String::from_utf8(user).unwrap();
        add_acl_entries(
            &run,
            &[
                "everyone deny delete",
                &format!("user:{} allow list,search", user.trim()),
            ],
        );
        let socket = run.join("daemon.sock");
        let listener = bind(&socket).unwrap();
        let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _client = UnixStream::connect(&socket).await.unwrap();
        listener.accept().await.unwrap();
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_directory_whose_acl_denies_deleting_new_files_still_gets_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        DirBuilder::new().mode(0o700).create(&run).unwrap();
        add_acl_entries(&run, &["everyone deny delete,file_inherit"]);
        let socket = run.join("daemon.sock");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        drop(bind(&socket).unwrap());
        let listener = bind(&socket).unwrap();
        let names: Vec<_> = fs::read_dir(&run)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["daemon.sock"]);
        let _client = UnixStream::connect(&socket).await.unwrap();
        listener.accept().await.unwrap();
    }

    #[test]
    fn requests_are_tagged_json() {
        let request = Request::Answer {
            question_id: "q1".into(),
            answer: Answer::Restore,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"answer","question_id":"q1","answer":"restore"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);

        let request = Request::Restore {
            space_id: "s1".into(),
            restore: RestoreRequest::Version {
                node_id: "n1".into(),
                version_id: "v1".into(),
            },
            folder: "/home/ada/Docs".into(),
            path: "a.txt".into(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"restore","space_id":"s1","restore":{"type":"version","node_id":"n1","version_id":"v1"},"folder":"/home/ada/Docs","path":"a.txt"}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }

    const SPACE: &str = "0190f0a0-0000-7000-8000-000000000001";

    /// A profile whose folder `Koidrive` syncs one space, which waits on the decision `q1`.
    fn linked_profile() -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(dir.path()).unwrap();
        let paths = Paths::under(&base.join("profile"));
        let root = base.join("Koidrive");
        fs::create_dir_all(&root).unwrap();
        let store_path = paths.store(SPACE).unwrap();
        fs::create_dir_all(store_path.parent().unwrap()).unwrap();
        let question = Question {
            id: "q1".into(),
            lineage: "q1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 10,
            kind: koidrive_driver::QuestionKind::AmbiguousDisappearance,
            items: Vec::new(),
        };
        koidrive_store::Store::open(&store_path)
            .unwrap()
            .transaction(|tx| {
                tx.set_link(&koidrive_store::Link {
                    space_id: SPACE.into(),
                    space_name: "Projects".into(),
                    root: root.clone(),
                    root_dev: 1,
                    server_url: "https://files.example.com/".into(),
                    device_id: "d1".into(),
                    linked_at_ms: 1,
                })?;
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: question.id.clone(),
                    asked_at_ms: question.asked_at_ms,
                    body: serde_json::to_vec(&question).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        (dir, paths, root)
    }

    #[tokio::test]
    async fn resolve_reads_a_decision_sync_took_so_the_same_word_is_unchanged_and_another_is_stale()
    {
        use koidrive_engine::question::{ConflictFile, Revisions};

        let (_dir, paths, _root) = linked_profile();
        let file = |node_id: &str| ConflictFile {
            node_id: node_id.into(),
            revisions: Revisions {
                content_rev: 2,
                meta_rev: 1,
            },
        };
        let conflict = Question {
            id: "c1".into(),
            lineage: "c1".into(),
            space_id: SPACE.into(),
            asked_at_ms: 20,
            kind: koidrive_driver::QuestionKind::Conflict {
                original: file("n-original"),
                copy: file("n-copy"),
                sent_in: None,
            },
            items: Vec::new(),
        };
        let store = || koidrive_store::Store::open(paths.store(SPACE).unwrap()).unwrap();
        store()
            .transaction(|tx| {
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: conflict.id.clone(),
                    asked_at_ms: conflict.asked_at_ms,
                    body: serde_json::to_vec(&conflict).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        let resolve =
            |id: &str, word: &str| crate::resolve(&paths, Some(id.into()), Some(word.into()));
        let outcome = |done: Result<crate::report::Done, CliError>| match done {
            Ok(done) => Ok(done.outcome),
            Err(error) => Err(error.code),
        };
        use crate::report::Outcome;

        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Accepted)
        );
        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Unchanged)
        );
        assert!(store().transaction(|tx| tx.take_answer("c1")).unwrap());
        assert_eq!(
            outcome(resolve("c1", "keep-node:n-original").await),
            Ok(Outcome::Unchanged)
        );
        for other in ["keep-node:n-copy", "keep-both"] {
            assert_eq!(
                outcome(resolve("c1", other).await),
                Err(Code::StaleDecision),
                "{other}"
            );
        }

        assert_eq!(
            outcome(resolve("q1", "delete").await),
            Ok(Outcome::Accepted)
        );
        store().transaction(|tx| tx.remove_question("q1")).unwrap();
        assert_eq!(
            outcome(resolve("q1", "delete").await),
            Err(Code::StaleDecision)
        );
    }

    #[tokio::test]
    async fn a_repeated_answer_is_unchanged_after_sync_took_it_and_another_answer_is_stale() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = |question_id: &str, answer| {
            handle(
                &driver,
                Request::Answer {
                    question_id: question_id.into(),
                    answer,
                },
            )
        };
        assert_eq!(answer("q1", Answer::Delete).await, Response::Answered);
        assert_eq!(
            answer("q1", Answer::Delete).await,
            Response::AlreadyAnswered
        );
        assert!(
            koidrive_store::Store::open(paths.store(SPACE).unwrap())
                .unwrap()
                .transaction(|tx| tx.take_answer("q1"))
                .unwrap()
        );
        assert!(driver.questions().unwrap().is_empty());
        assert_eq!(
            answer("q1", Answer::Delete).await,
            Response::AlreadyAnswered
        );

        let Response::Error {
            code,
            message,
            next,
        } = answer("q1", Answer::Restore).await
        else {
            panic!("another answer was taken");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(
            message,
            "the question was answered with delete; koi resolve lists the decisions waiting now"
        );
        assert_eq!(next, [Next::of(&["koi", "resolve", "--json"])]);
    }

    #[tokio::test]
    async fn an_answer_the_daemon_acknowledged_outlives_the_daemon_and_is_unchanged_after_sync_took_it()
     {
        let (_dir, paths, _root) = linked_profile();
        let answer = || Request::Answer {
            question_id: "q1".into(),
            answer: Answer::Delete,
        };
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, answer()).await, Response::Answered);
        drop(driver);
        let mut store = koidrive_store::Store::open(paths.store(SPACE).unwrap()).unwrap();
        assert!(store.transaction(|tx| tx.take_answer("q1")).unwrap());
        drop(store);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, answer()).await, Response::AlreadyAnswered);
    }

    #[tokio::test]
    async fn question_ids_that_differ_in_case_are_different_questions() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = |question_id: &str| {
            handle(
                &driver,
                Request::Answer {
                    question_id: question_id.into(),
                    answer: Answer::Delete,
                },
            )
        };
        assert_eq!(answer("q1").await, Response::Answered);
        koidrive_store::Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| tx.take_answer("q1"))
            .unwrap();
        let Response::Error { code, .. } = answer("Q1").await else {
            panic!("the answer to q1 was taken as one to Q1");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(answer("q1").await, Response::AlreadyAnswered);
    }

    #[tokio::test]
    async fn an_answer_to_a_decision_replaced_before_sync_took_it_is_stale() {
        let (_dir, paths, _root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let answer = || {
            handle(
                &driver,
                Request::Answer {
                    question_id: "q1".into(),
                    answer: Answer::Delete,
                },
            )
        };
        assert_eq!(answer().await, Response::Answered);
        let replacement = Question {
            id: "q2".into(),
            lineage: "q2".into(),
            space_id: SPACE.into(),
            asked_at_ms: 20,
            kind: koidrive_driver::QuestionKind::AmbiguousDisappearance,
            items: Vec::new(),
        };
        koidrive_store::Store::open(paths.store(SPACE).unwrap())
            .unwrap()
            .transaction(|tx| {
                tx.remove_question("q1")?;
                tx.put_question(&koidrive_store::StoredQuestion {
                    question_id: "q2".into(),
                    asked_at_ms: 20,
                    body: serde_json::to_vec(&replacement).unwrap(),
                    answer: None,
                })
            })
            .unwrap();
        assert_eq!(driver.questions().unwrap(), [replacement]);
        let Response::Error { code, next, .. } = answer().await else {
            panic!("the dropped answer was reported as taken");
        };
        assert_eq!(code, Code::StaleDecision);
        assert_eq!(next, [Next::of(&["koi", "resolve", "--json"])]);
    }

    #[tokio::test]
    async fn a_restart_that_leaves_the_folder_halted_records_nothing_so_its_id_runs_again() {
        let (_dir, paths, root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        driver.start();
        let request_id = RequestId::parse("r1").unwrap();
        let restart = || {
            let key = Key::new(
                "folder restart",
                serde_json::json!({ "folder": root.display().to_string() }),
            );
            Request::Restart {
                folder: root.clone(),
            }
            .once(Some(request_id.clone()), key)
        };
        for _ in 0..2 {
            let Response::Restarted { folder } = handle(&driver, restart()).await else {
                panic!("the restart failed");
            };
            assert_eq!(folder.state, FolderState::Halted, "{folder:?}");
            assert!(!crate::request::request_file(&paths, &request_id).exists());
        }
        driver.stop().await;
    }

    #[tokio::test]
    async fn a_request_with_an_id_runs_once_and_its_record_outlives_the_folder_and_the_daemon() {
        let (_dir, paths, root) = linked_profile();
        let driver = Driver::open(paths.clone()).unwrap();
        let id = |text: &str| Some(RequestId::parse(text).unwrap());
        let restart = |folder: &Path, request_id| {
            let key = Key::new(
                "folder restart",
                serde_json::json!({ "folder": folder.display().to_string() }),
            );
            Request::Restart {
                folder: folder.to_path_buf(),
            }
            .once(request_id, key)
        };
        let first = handle(&driver, restart(&root, id("r1"))).await;
        assert!(matches!(first, Response::Restarted { .. }), "{first:?}");
        let unlinked = handle(
            &driver,
            Request::Unlink {
                folder: root.clone(),
            },
        )
        .await;
        assert!(
            matches!(unlinked, Response::Unlinked { .. }),
            "{unlinked:?}"
        );
        assert_eq!(
            handle(
                &driver,
                Request::Unlink {
                    folder: root.clone()
                }
            )
            .await,
            Response::NotLinked { path: root.clone() }
        );
        assert_eq!(handle(&driver, restart(&root, id("r1"))).await, first);
        drop(driver);
        let driver = Driver::open(paths.clone()).unwrap();
        assert_eq!(handle(&driver, restart(&root, id("r1"))).await, first);

        let other = handle(&driver, restart(&root.join("Other"), id("r1"))).await;
        assert!(
            matches!(
                &other,
                Response::Error {
                    code: Code::StaleDecision,
                    ..
                }
            ),
            "{other:?}"
        );
        for _ in 0..2 {
            let failed = handle(&driver, restart(&root, id("r2"))).await;
            assert!(
                matches!(
                    &failed,
                    Response::Error {
                        code: Code::Usage,
                        ..
                    }
                ),
                "{failed:?}"
            );
            assert!(!crate::request::request_file(&paths, &id("r2").unwrap()).exists());
        }
    }
}
