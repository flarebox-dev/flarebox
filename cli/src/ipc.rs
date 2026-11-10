// SPDX-License-Identifier: Apache-2.0

//! The daemon's local socket. Every command is a [`Request`] that [`handle`] runs against a [`Driver`]: the daemon's
//! driver through the socket when the daemon runs, or a driver the command opens itself when it does not. One JSON
//! line goes each way per connection. The socket is the owner's only: its directory is 0700, the socket 0600, and
//! the daemon also refuses a peer running as another user.

use std::fs::{self, DirBuilder};
use std::io;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

use flarebox_driver::{
    Account, Answer, Credential, Driver, DriverError, FolderStatus, Paths, Question, Status,
};
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
    Questions,
    Answer {
        question_id: String,
        answer: Answer,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Status { status: Status },
    Paired { account: Account },
    Linked { folder: FolderStatus },
    Unlinked { space_id: String, path: PathBuf },
    Questions { questions: Vec<Question> },
    Answered,
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
        Request::Questions => driver
            .questions()
            .map(|questions| Response::Questions { questions }),
        Request::Answer {
            question_id,
            answer,
        } => driver
            .answer(&question_id, answer)
            .map(|()| Response::Answered),
    };
    outcome.unwrap_or_else(|error| Response::Error {
        message: error.to_string(),
    })
}

/// Binds the daemon's socket. The caller holds the profile lock, so a socket file left behind is stale.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
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

/// Runs a request in the daemon when one runs, and in a driver of its own otherwise.
pub async fn run(paths: &Paths, request: Request) -> Result<Response, String> {
    let socket = paths.socket();
    if let Some(response) = send(&socket, &request)
        .await
        .map_err(|error| format!("the daemon's socket {} failed: {error}", socket.display()))?
    {
        return Ok(response);
    }
    let driver = Driver::open(paths.clone()).map_err(|error| match error {
        DriverError::Busy(_) => format!(
            "a Flarebox process holds this profile, but its socket {} does not answer",
            socket.display()
        ),
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
    }
}
