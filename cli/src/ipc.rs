// SPDX-License-Identifier: Apache-2.0

//! The daemon's local socket. Every command is a [`Request`] that [`handle`] runs against a [`Driver`]: the daemon's
//! driver through the socket when the daemon runs, or a driver the command opens itself when it does not. One JSON
//! line goes each way per connection. The socket is the owner's only: its directory is 0700, the socket 0600, and
//! the daemon also refuses a peer running as another user.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use flarebox_driver::{
    Account, Answer, Credential, Driver, DriverError, FolderStatus, Paths, Question,
    RestoreRequest, RestoreStatus, Status,
};
use flarebox_engine::commit::Lift;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

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
    /// Stores the credential that `flarebox setup` claimed.
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
    Answer {
        question_id: String,
        answer: Answer,
    },
    /// Records a restore in the folder of `space_id`, for its engine to send.
    Restore {
        space_id: String,
        restore: RestoreRequest,
    },
    Restores {
        space_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Status { status: Status },
    Paired { account: Account },
    Linked { folder: FolderStatus },
    Unlinked { space_id: String, path: PathBuf },
    Restarted { folder: FolderStatus },
    Questions { questions: Vec<Question> },
    Answered,
    Restore { id: String },
    Restores { restores: Vec<RestoreStatus> },
    SignedOut,
    Error { message: String },
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
        Request::Link { folder, space } => driver
            .link(&folder, &space)
            .await
            .map(|folder| Response::Linked { folder }),
        Request::Unlink { folder } => driver.unlink(&folder).await.map(|link| Response::Unlinked {
            space_id: link.space_id,
            path: link.root,
        }),
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
        Request::Answer {
            question_id,
            answer,
        } => driver
            .answer(&question_id, answer)
            .map(|()| Response::Answered),
        Request::Restore { space_id, restore } => driver
            .restore(&space_id, restore)
            .map(|id| Response::Restore { id }),
        Request::Restores { space_id } => driver
            .restores(&space_id)
            .map(|restores| Response::Restores { restores }),
    };
    outcome.unwrap_or_else(|error| Response::Error {
        message: error.to_string(),
    })
}

/// Binds the daemon's socket. The caller holds the profile lock, so a socket file left behind is stale.
///
/// The directory must belong to this user and be closed to everyone else, so no other user can reach the socket even
/// before its mode is set. The socket is bound under a temporary name and renamed once it is 0600, so it never
/// appears at `path` with another mode. Setting the umask instead would change it for every thread of the process.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
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
    if acl_allows_others(dir, uid)? {
        return Err(refuse(
            "lets other users in through an ACL; remove it with chmod -N".to_owned(),
        ));
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temp = dir.join(format!(".{name}.new"));
    match fs::remove_file(&temp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = bind_socket(&temp)?;
    fs::set_permissions(&temp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temp, path)?;
    listener.set_nonblocking(true)?;
    UnixListener::from_std(listener)
}

/// Creates the socket 0600 before binding it, so it is closed to other users from its first moment even if the
/// directory was swapped after it was checked. Linux takes the new file's mode from the socket; macOS ignores it.
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

#[cfg(not(target_os = "linux"))]
fn bind_socket(path: &Path) -> io::Result<std::os::unix::net::UnixListener> {
    std::os::unix::net::UnixListener::bind(path)
}

/// Whether the extended ACL of `dir` has an allow entry for anyone but `owner`. Such an entry grants access the mode
/// bits do not show, and an inheritable one is copied onto the socket. Deny entries only take access away.
#[cfg(target_os = "macos")]
fn acl_allows_others(dir: &Path, owner: u32) -> io::Result<bool> {
    use std::ffi::{CString, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;

    const ACL_TYPE_EXTENDED: c_int = 0x100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ACL_NEXT_ENTRY: c_int = -1;
    const ACL_EXTENDED_ALLOW: c_int = 1;
    const ID_TYPE_UID: c_int = 0;
    unsafe extern "C" {
        fn acl_get_file(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn acl_get_tag_type(entry: *mut c_void, tag: *mut c_int) -> c_int;
        fn acl_get_qualifier(entry: *mut c_void) -> *mut c_void;
        fn acl_free(object: *mut c_void) -> c_int;
        fn mbr_uuid_to_id(uuid: *const u8, id: *mut u32, id_type: *mut c_int) -> c_int;
    }

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
        Err(error) => Response::Error {
            message: format!("the request is not valid: {error}"),
        },
    };
    write_line(&mut writer, &response).await
}

/// Sends a request to the daemon at `socket`, or returns `None` when no daemon listens there.
pub async fn send(socket: &Path, request: &Request) -> io::Result<Option<Response>> {
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
    send(&socket, request)
        .await
        .map_err(|error| format!("the daemon's socket {} failed: {error}", socket.display()))
}

/// The error for a profile that a process holds while its socket does not answer.
pub fn busy(paths: &Paths) -> String {
    format!(
        "a Flarebox process holds this profile, but its socket {} does not answer",
        paths.socket().display()
    )
}

/// Runs a request in the daemon when one runs, and in a driver of its own otherwise.
pub async fn run(paths: &Paths, request: Request) -> Result<Response, String> {
    if let Some(response) = ask(paths, &request).await? {
        return Ok(response);
    }
    let driver = Driver::open(paths.clone()).map_err(|error| match error {
        DriverError::Busy(_) => busy(paths),
        other => other.to_string(),
    })?;
    Ok(handle(&driver, request).await)
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
        let response = run(
            &paths,
            Request::Answer {
                question_id: "q1".into(),
                answer: Answer::Delete,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            response,
            Response::Error {
                message: "no unanswered question has the id \"q1\"".into()
            }
        );
        server.abort();
    }

    #[tokio::test]
    async fn without_a_daemon_a_command_opens_the_profile_itself() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let response = run(&paths, Request::Questions).await.unwrap();
        assert_eq!(response, Response::Questions { questions: vec![] });

        let _held = Driver::open(paths.clone()).unwrap();
        let error = run(&paths, Request::Status).await.unwrap_err();
        assert!(error.contains("does not answer"), "{error}");
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
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"request":"restore","space_id":"s1","restore":{"type":"version","node_id":"n1","version_id":"v1"}}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
    }
}
