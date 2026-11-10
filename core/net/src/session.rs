// SPDX-License-Identifier: Apache-2.0

//! The WebSocket session with one space (spec v0 section 7): `Hello` and `Welcome`, RPCs matched by `req_id`, the
//! change stream, `Reauth`, and reconnects with backoff that resend only what RFC 0002 section 2 allows.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use flarebox_proto::v1::{
    AccessToken, ChunkRef, ClientMessage, Commit, CommitResult, DownloadTargets,
    GetDownloadTargets, GetVersionChunks, Hello, ListTrash, ListTree, ListVersions, PrepareUpload,
    PrepareUploadResult, Reauth, RpcRequest, RpcResponse, ServerMessage, TrashPage, TreePage,
    VersionChunks, VersionList, Welcome, client_message, error::Code, rpc_request::Call,
    rpc_response, server_message::Msg,
};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use rustls::pki_types::ServerName;
use rustls_platform_verifier::BuilderVerifierExt;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message as WsMessage};
use url::Url;

use crate::auth::Auth;
use crate::backoff::Backoff;
use crate::error::{ApiError, CallError, NotResent, ServerError, StopReason};
use crate::event::{Committed, DisconnectReason, Event};
use crate::stream::{Fed, Stream};

/// The protocol minor versions under `/v1/` this client speaks (spec v0 section 9.1).
pub const PROTO_MIN: u32 = 1;
pub const PROTO_MAX: u32 = 1;
/// A `ServerMessage` is at most 1 MiB (spec v0 section 7.2, RFC 0001 section 7). A larger one breaks the protocol.
pub const MAX_SERVER_MESSAGE: usize = 1 << 20;
/// A `ClientMessage` is at most 8 MiB; the server closes the connection on a larger one.
pub const MAX_CLIENT_MESSAGE: usize = 8 << 20;

/// WebSocket close codes the server uses (spec v0 section 7.1, RFC 0003 section 3).
const CLOSE_TOKEN_EXPIRED: u16 = 4001;
const CLOSE_UNSUPPORTED_VERSION: u16 = 4002;
const CLOSE_DEVICE_REVOKED: u16 = 4003;
/// Checks the token at least this often, whatever expiry the server sent.
const MAX_REAUTH_WAIT: Duration = Duration::from_secs(3600);
/// Waits at least this long between token refreshes, even when a new token is already due because the refresh
/// margin is longer than the token's lifetime or the clocks disagree.
const MIN_REAUTH_WAIT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub space_id: String,
    /// Sent first on every connection.
    pub hello: Hello,
    pub backoff: Backoff,
    /// WebSocket ping interval. The connection counts as lost when nothing arrives for two intervals.
    pub ping_interval: Duration,
    /// Bounds getting an access token, the TCP connection, TLS and the WebSocket upgrade together, and each token
    /// refresh for `Reauth`, which runs while the connection keeps serving.
    pub connect_timeout: Duration,
    /// Bounds the wait for `Welcome` after `Hello`.
    pub welcome_timeout: Duration,
    /// Bounds sending one message. A server that stops reading for this long counts as lost.
    pub write_timeout: Duration,
    /// How many times a call refused with `CODE_RETRY_LATER` is sent again before the error is returned.
    pub retry_later_attempts: u32,
}

impl SessionConfig {
    pub fn new(
        space_id: impl Into<String>,
        client_name: impl Into<String>,
        client_version: impl Into<String>,
        max_format: u32,
    ) -> Self {
        SessionConfig {
            space_id: space_id.into(),
            hello: Hello {
                client_name: client_name.into(),
                client_version: client_version.into(),
                proto_min: PROTO_MIN,
                proto_max: PROTO_MAX,
                max_format,
                features: Vec::new(),
            },
            backoff: Backoff::default(),
            ping_interval: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(30),
            welcome_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(60),
            retry_later_attempts: 5,
        }
    }
}

/// The events of a session, in order. Ends after `Event::Stopped`.
#[derive(Debug)]
pub struct Events(mpsc::UnboundedReceiver<Event>);

impl Events {
    pub async fn next(&mut self) -> Option<Event> {
        self.0.recv().await
    }
}

/// A session with one space. It keeps reconnecting until it is closed or stopped. Dropping it closes it.
#[derive(Debug)]
pub struct Session {
    commands: mpsc::UnboundedSender<Command>,
    /// Set by `close`; the task drops the connection and whatever it is waiting on, a blocked write included.
    /// Dropping the session has the same effect.
    closing: watch::Sender<bool>,
    stopped: Arc<OnceLock<StopReason>>,
    commit_lock: Mutex<()>,
    backoff: Backoff,
    retry_later_attempts: u32,
    task: JoinHandle<()>,
}

impl Session {
    /// Starts the session task on the current Tokio runtime.
    pub fn start(auth: Arc<Auth>, config: SessionConfig) -> Result<(Session, Events), StopReason> {
        let url = websocket_url(auth.api().base(), &config.space_id)?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let stopped = Arc::new(OnceLock::new());
        let (closing, closing_rx) = watch::channel(false);
        let session = Session {
            commands,
            closing,
            stopped: stopped.clone(),
            commit_lock: Mutex::new(()),
            backoff: config.backoff,
            retry_later_attempts: config.retry_later_attempts,
            task: tokio::spawn(
                Task {
                    auth,
                    config,
                    url,
                    tls: None,
                    commands: command_rx,
                    closing: closing_rx,
                    events,
                    stopped,
                    next_req: 1,
                    pending: BTreeMap::new(),
                    stream: Stream::default(),
                    attempt: 0,
                    token: None,
                    expired: None,
                }
                .run(),
            ),
        };
        Ok((session, Events(event_rx)))
    }

    /// Streams the log after `since_seq` of `history_id`: the last seq the driver applied completely, or the first
    /// `TreePage.head_seq` of a bootstrap (spec v0 section 7.5). The session resumes from the last seq it passed on
    /// after every reconnect, until a `ResyncRequired` stops the stream.
    pub fn subscribe(&self, history_id: impl Into<String>, since_seq: u64) {
        let _ = self
            .commands
            .send(Command::Subscribe(history_id.into(), since_seq));
    }

    /// Sends a commit and waits for its result. One commit is in flight at a time (spec v0 section 8). If the
    /// connection drops, the identical commit is resent only when the next `Welcome` accounts for its slot
    /// (RFC 0002 section 2); otherwise the call fails with `CallError::NotResent`. The result also arrives as
    /// `Event::Committed` in seq order.
    pub async fn commit(&self, commit: Commit) -> Result<CommitResult, CallError> {
        let _one_at_a_time = self.commit_lock.lock().await;
        let slot = CommitSlot {
            commit_id: commit.commit_id.clone(),
            history_id: commit.history_id.clone(),
            device_seq: commit.device_seq,
        };
        match self.call(Call::Commit(commit), Some(slot)).await? {
            rpc_response::Result::Commit(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    /// At most 1,000 chunks per call (spec v0 section 13).
    pub async fn prepare_upload(
        &self,
        chunks: Vec<ChunkRef>,
    ) -> Result<PrepareUploadResult, CallError> {
        match self
            .call(Call::PrepareUpload(PrepareUpload { chunks }), None)
            .await?
        {
            rpc_response::Result::PrepareUpload(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    /// At most 1,000 hashes per call.
    pub async fn get_download_targets(
        &self,
        hashes: Vec<Vec<u8>>,
    ) -> Result<DownloadTargets, CallError> {
        match self
            .call(
                Call::GetDownloadTargets(GetDownloadTargets { hashes }),
                None,
            )
            .await?
        {
            rpc_response::Result::DownloadTargets(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn get_version_chunks(
        &self,
        version_id: impl Into<String>,
        page: u32,
    ) -> Result<VersionChunks, CallError> {
        let request = GetVersionChunks {
            version_id: version_id.into(),
            page,
        };
        match self.call(Call::GetVersionChunks(request), None).await? {
            rpc_response::Result::VersionChunks(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_versions(&self, request: ListVersions) -> Result<VersionList, CallError> {
        match self.call(Call::ListVersions(request), None).await? {
            rpc_response::Result::Versions(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_tree(
        &self,
        cursor: impl Into<String>,
        limit: u32,
    ) -> Result<TreePage, CallError> {
        match self
            .call(
                Call::ListTree(ListTree {
                    cursor: cursor.into(),
                    limit,
                }),
                None,
            )
            .await?
        {
            rpc_response::Result::Tree(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    pub async fn list_trash(
        &self,
        cursor: impl Into<String>,
        limit: u32,
    ) -> Result<TrashPage, CallError> {
        match self
            .call(
                Call::ListTrash(ListTrash {
                    cursor: cursor.into(),
                    limit,
                }),
                None,
            )
            .await?
        {
            rpc_response::Result::Trash(result) => Ok(result),
            _ => Err(CallError::UnexpectedResult),
        }
    }

    /// Closes the connection and waits for the session task to end. Pending calls fail with `StopReason::Closed`.
    pub async fn close(self) {
        let _ = self.commands.send(Command::Close);
        let _ = self.closing.send(true);
        let _ = self.task.await;
    }

    /// Sends one call, again after `CODE_RETRY_LATER` up to the configured number of times. A refused call changed
    /// nothing, so sending it unchanged is safe, a commit included.
    async fn call(
        &self,
        call: Call,
        commit: Option<CommitSlot>,
    ) -> Result<rpc_response::Result, CallError> {
        let bytes = ClientMessage {
            msg: Some(client_message::Msg::Rpc(RpcRequest {
                req_id: u64::MAX,
                call: Some(call.clone()),
            })),
        }
        .encoded_len();
        if bytes > MAX_CLIENT_MESSAGE {
            return Err(CallError::TooLarge { bytes });
        }
        let mut attempt = 0;
        loop {
            let (reply, answer) = oneshot::channel();
            let command = Command::Call {
                call: call.clone(),
                commit: commit.clone(),
                reply,
            };
            self.commands.send(command).map_err(|_| self.stopped())?;
            match answer.await.map_err(|_| self.stopped())? {
                Err(CallError::Server(error))
                    if error.code() == Code::RetryLater && attempt < self.retry_later_attempts =>
                {
                    let wait = error
                        .retry_after()
                        .unwrap_or_else(|| self.backoff.delay(attempt));
                    sleep(wait).await;
                    attempt += 1;
                }
                outcome => return outcome,
            }
        }
    }

    fn stopped(&self) -> CallError {
        CallError::Stopped(self.stopped.get().cloned().unwrap_or(StopReason::Closed))
    }
}

fn websocket_url(base: &Url, space_id: &str) -> Result<Url, StopReason> {
    if space_id.is_empty() || !space_id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        return Err(StopReason::BadUrl(format!(
            "{space_id:?} is not a space id"
        )));
    }
    let mut url = base
        .join(&format!("v1/spaces/{space_id}/ws"))
        .map_err(|e| StopReason::BadUrl(e.to_string()))?;
    let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
    url.set_scheme(scheme)
        .map_err(|()| StopReason::BadUrl(base.to_string()))?;
    Ok(url)
}

/// What identifies the commit in flight for the resend rule.
#[derive(Clone, Debug)]
struct CommitSlot {
    commit_id: String,
    history_id: String,
    device_seq: u64,
}

/// RFC 0002 section 2: after a reconnect the commit in flight, with slot `a + 1`, is resent unchanged only when
/// `Welcome.device_seq` is `a` (not accepted) or `a + 1` (accepted, response lost), in the same history.
fn may_resend(slot: &CommitSlot, welcome: &Welcome) -> Result<(), NotResent> {
    if welcome.history_id != slot.history_id {
        return Err(NotResent::HistoryChanged {
            commit: slot.history_id.clone(),
            welcome: welcome.history_id.clone(),
        });
    }
    if welcome.device_seq == slot.device_seq
        || Some(welcome.device_seq) == slot.device_seq.checked_sub(1)
    {
        return Ok(());
    }
    Err(NotResent::DeviceSeqMismatch {
        slot: slot.device_seq,
        welcome: welcome.device_seq,
    })
}

type Reply = oneshot::Sender<Result<rpc_response::Result, CallError>>;

enum Command {
    Call {
        call: Call,
        commit: Option<CommitSlot>,
        reply: Reply,
    },
    Subscribe(String, u64),
    Close,
}

struct PendingCall {
    call: Call,
    commit: Option<CommitSlot>,
    reply: Reply,
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Socket = WebSocketStream<Box<dyn Io>>;

/// How one connection ended.
enum Exit {
    Reconnect { reason: DisconnectReason, now: bool },
    Stop(StopReason),
    Closed,
}

impl Exit {
    fn timeout() -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::Timeout,
            now: false,
        }
    }

    fn transport(error: impl std::fmt::Display) -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::Transport(error.to_string()),
            now: false,
        }
    }

    fn violation(what: &str) -> Exit {
        Exit::Reconnect {
            reason: DisconnectReason::ProtocolViolation(what.to_owned()),
            now: false,
        }
    }
}

struct Task {
    auth: Arc<Auth>,
    config: SessionConfig,
    url: Url,
    tls: Option<Arc<rustls::ClientConfig>>,
    commands: mpsc::UnboundedReceiver<Command>,
    closing: watch::Receiver<bool>,
    events: mpsc::UnboundedSender<Event>,
    stopped: Arc<OnceLock<StopReason>>,
    next_req: u64,
    /// Calls not yet answered, by `req_id`. A call keeps its `req_id` when it is resent on a new connection.
    pending: BTreeMap<u64, PendingCall>,
    stream: Stream,
    /// Failed connection attempts since the last `Welcome`.
    attempt: u32,
    /// The access token of the current connection.
    token: Option<String>,
    /// A token the server closed the connection over, rejected before the next connection gets a token. Not
    /// rejected at once, because a refresh in flight may hold the token lock until the connection is dropped.
    expired: Option<String>,
}

impl Task {
    async fn run(mut self) {
        let reason = loop {
            let mut closing = self.closing.clone();
            let exit = tokio::select! {
                biased;
                exit = self.connection() => exit,
                () = closed(&mut closing) => Exit::Closed,
            };
            let (reason, now) = match exit {
                Exit::Closed => break StopReason::Closed,
                Exit::Stop(reason) => break reason,
                Exit::Reconnect { reason, now } => (reason, now),
            };
            let retry_in = if now && self.attempt == 0 {
                Duration::ZERO
            } else {
                self.config.backoff.delay(self.attempt)
            };
            self.attempt = self.attempt.saturating_add(1);
            tracing::debug!(?reason, ?retry_in, "flarebox session disconnected");
            self.emit(Event::Disconnected { reason, retry_in });
            if !self.idle(retry_in).await {
                break StopReason::Closed;
            }
        };
        tracing::debug!(%reason, "flarebox session stopped");
        let _ = self.stopped.set(reason.clone());
        for (_, call) in std::mem::take(&mut self.pending) {
            let _ = call.reply.send(Err(CallError::Stopped(reason.clone())));
        }
        self.emit(Event::Stopped(reason));
    }

    /// Waits between connections while taking calls for the next one. False when the session was closed.
    async fn idle(&mut self, delay: Duration) -> bool {
        let deadline = sleep(delay);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return true,
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => return false,
                    Some(Command::Call { call, commit, reply }) => {
                        let req_id = self.req_id();
                        self.pending.insert(req_id, PendingCall { call, commit, reply });
                    }
                    Some(Command::Subscribe(history_id, since_seq)) => {
                        self.stream.subscribe(history_id, since_seq);
                    }
                },
            }
        }
    }

    async fn connection(&mut self) -> Exit {
        let (mut socket, token, welcome) = match self.establish().await {
            Ok(established) => established,
            Err(exit) => return exit,
        };
        self.attempt = 0;
        self.emit(Event::Connected(welcome.clone()));
        if let Some(resume) = self.stream.reconnected()
            && let Err(exit) = self
                .send(&mut socket, client_message::Msg::Resume(resume))
                .await
        {
            return exit;
        }
        if let Err(exit) = self.resend(&welcome, &mut socket).await {
            return exit;
        }
        self.serve(socket, token).await
    }

    /// Opens the WebSocket within `connect_timeout` and waits for an acceptable `Welcome`.
    async fn establish(&mut self) -> Result<(Socket, AccessToken, Welcome), Exit> {
        let (mut socket, token) = timeout(self.config.connect_timeout, self.open())
            .await
            .map_err(|_| Exit::timeout())??;
        let welcome = self.handshake(&mut socket).await?;
        Ok((socket, token, welcome))
    }

    /// Gets a token and opens the WebSocket: TCP, TLS for `wss`, and the upgrade.
    async fn open(&mut self) -> Result<(Socket, AccessToken), Exit> {
        if let Some(expired) = self.expired.take() {
            self.auth.reject(&expired).await;
        }
        let token = match self.auth.access_token().await {
            Ok(token) => token,
            Err(error) if error.is_unauthenticated() => {
                return Err(Exit::Stop(StopReason::CredentialsRefused));
            }
            Err(error) => return Err(Exit::transport(error)),
        };
        self.token = Some(token.token.clone());
        let host = self
            .url
            .host_str()
            .unwrap_or_default()
            .trim_matches(['[', ']'])
            .to_owned();
        let port = self.url.port_or_known_default().unwrap_or(80);
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .map_err(Exit::transport)?;
        let _ = tcp.set_nodelay(true);
        let io: Box<dyn Io> = if self.url.scheme() == "wss" {
            let name = ServerName::try_from(host.clone())
                .map_err(|e| Exit::Stop(StopReason::BadUrl(e.to_string())))?;
            let tls = TlsConnector::from(self.tls_config()?);
            Box::new(tls.connect(name, tcp).await.map_err(Exit::transport)?)
        } else {
            Box::new(tcp)
        };
        let mut request = self
            .url
            .as_str()
            .into_client_request()
            .map_err(|e| Exit::Stop(StopReason::BadUrl(e.to_string())))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", token.token))
            .map_err(|_| Exit::violation("an access token that is not a header value"))?;
        request.headers_mut().insert(AUTHORIZATION, bearer);
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SERVER_MESSAGE))
            .max_frame_size(Some(MAX_SERVER_MESSAGE));
        match tokio_tungstenite::client_async_with_config(request, io, Some(config)).await {
            Ok((socket, _)) => Ok((socket, token)),
            Err(tungstenite::Error::Http(response)) => {
                Err(self.refused(response.status().as_u16(), &token.token).await)
            }
            Err(error) => Err(Exit::transport(error)),
        }
    }

    /// The WebSocket upgrade was answered with an HTTP error.
    async fn refused(&mut self, status: u16, token: &str) -> Exit {
        match status {
            401 => {
                self.auth.reject(token).await;
                Exit::Reconnect {
                    reason: DisconnectReason::Refused { status },
                    now: true,
                }
            }
            403 => Exit::Stop(StopReason::Forbidden),
            404 => Exit::Stop(StopReason::NotFound),
            _ => Exit::Reconnect {
                reason: DisconnectReason::Refused { status },
                now: false,
            },
        }
    }

    /// rustls with the platform verifier, so the operating system's trust store decides, as for HTTP.
    fn tls_config(&mut self) -> Result<Arc<rustls::ClientConfig>, Exit> {
        if let Some(config) = &self.tls {
            return Ok(config.clone());
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .and_then(|builder| builder.with_platform_verifier())
            .map_err(|e| Exit::Stop(StopReason::Tls(e.to_string())))?
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        self.tls = Some(config.clone());
        Ok(config)
    }

    async fn handshake(&mut self, socket: &mut Socket) -> Result<Welcome, Exit> {
        self.send(
            socket,
            client_message::Msg::Hello(self.config.hello.clone()),
        )
        .await?;
        let deadline = Instant::now() + self.config.welcome_timeout;
        loop {
            let item = timeout_at(deadline, socket.next())
                .await
                .map_err(|_| Exit::Reconnect {
                    reason: DisconnectReason::Timeout,
                    now: false,
                })?;
            match self.frame(item).await? {
                Some(Msg::Welcome(welcome)) => return self.negotiated(welcome),
                Some(Msg::Goodbye(goodbye)) => {
                    return Err(self.closed(Some(goodbye.code as u16), goodbye.reason));
                }
                Some(Msg::Notice(notice)) => self.emit(Event::Notice(notice)),
                Some(_) => return Err(Exit::violation("a message before Welcome")),
                None => {}
            }
        }
    }

    /// Resends the calls of a dropped connection on this one. A commit goes only where `may_resend` allows it.
    async fn resend(&mut self, welcome: &Welcome, socket: &mut Socket) -> Result<(), Exit> {
        let refused: Vec<(u64, NotResent)> = self
            .pending
            .iter()
            .filter_map(|(req_id, call)| {
                Some((*req_id, may_resend(call.commit.as_ref()?, welcome).err()?))
            })
            .collect();
        for (req_id, reason) in refused {
            if let Some(call) = self.pending.remove(&req_id) {
                tracing::debug!(%reason, "flarebox session did not resend a commit");
                let _ = call.reply.send(Err(CallError::NotResent(reason)));
            }
        }
        let calls: Vec<RpcRequest> = self
            .pending
            .iter()
            .map(|(req_id, call)| RpcRequest {
                req_id: *req_id,
                call: Some(call.call.clone()),
            })
            .collect();
        for request in calls {
            self.send(socket, client_message::Msg::Rpc(request)).await?;
        }
        Ok(())
    }

    async fn serve(&mut self, mut socket: Socket, mut token: AccessToken) -> Exit {
        let interval = self.config.ping_interval;
        let mut ping = tokio::time::interval_at(Instant::now() + interval, interval);
        let mut last_heard = Instant::now();
        let reauth = sleep_until(self.reauth_at(&token));
        tokio::pin!(reauth);
        let mut refreshing: Option<Refresh> = None;
        loop {
            tokio::select! {
                item = socket.next() => {
                    last_heard = Instant::now();
                    let msg = match self.frame(item).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => continue,
                        Err(exit) => return exit,
                    };
                    if let Err(exit) = self.dispatch(msg, &mut socket).await {
                        return exit;
                    }
                }
                command = self.commands.recv() => {
                    let sent = match command {
                        None | Some(Command::Close) => {
                            let _ = socket.close(None).await;
                            return Exit::Closed;
                        }
                        Some(Command::Subscribe(history_id, since_seq)) => {
                            let resume = self.stream.subscribe(history_id, since_seq);
                            self.send(&mut socket, client_message::Msg::Resume(resume)).await
                        }
                        Some(Command::Call { call, commit, reply }) => {
                            let req_id = self.req_id();
                            let request = RpcRequest { req_id, call: Some(call.clone()) };
                            self.pending.insert(req_id, PendingCall { call, commit, reply });
                            self.send(&mut socket, client_message::Msg::Rpc(request)).await
                        }
                    };
                    if let Err(exit) = sent {
                        return exit;
                    }
                }
                _ = ping.tick() => {
                    if last_heard.elapsed() > 2 * interval {
                        return Exit::Reconnect { reason: DisconnectReason::Timeout, now: false };
                    }
                    if let Err(exit) = self.write(&mut socket, WsMessage::Ping(Default::default())).await {
                        return exit;
                    }
                }
                () = &mut reauth, if refreshing.is_none() => {
                    let auth = self.auth.clone();
                    let limit = self.config.connect_timeout;
                    refreshing = Some(Box::pin(async move { timeout(limit, auth.access_token()).await }));
                }
                refreshed = refreshed(&mut refreshing) => {
                    refreshing = None;
                    let Ok(refreshed) = refreshed else {
                        tracing::debug!("flarebox session timed out refreshing its access token");
                        reauth.as_mut().reset(Instant::now() + self.config.backoff.delay(0));
                        continue;
                    };
                    match refreshed {
                        Ok(fresh) if fresh.token != token.token => {
                            let message = client_message::Msg::Reauth(Reauth { access_token: fresh.token.clone() });
                            if let Err(exit) = self.send(&mut socket, message).await {
                                return exit;
                            }
                            self.token = Some(fresh.token.clone());
                            token = fresh;
                            reauth.as_mut().reset(self.reauth_at(&token));
                        }
                        Ok(_) => reauth.as_mut().reset(Instant::now() + MIN_REAUTH_WAIT),
                        Err(error) if error.is_unauthenticated() => return Exit::Stop(StopReason::CredentialsRefused),
                        Err(error) => {
                            tracing::debug!(%error, "flarebox session could not refresh its access token");
                            reauth.as_mut().reset(Instant::now() + self.config.backoff.delay(0));
                        }
                    }
                }
            }
        }
    }

    /// Accepts the `Welcome` only if it chose a protocol version this client offered in `Hello`.
    fn negotiated(&self, welcome: Welcome) -> Result<Welcome, Exit> {
        let hello = &self.config.hello;
        if (hello.proto_min..=hello.proto_max).contains(&welcome.proto) {
            return Ok(welcome);
        }
        Err(Exit::Stop(StopReason::UnsupportedVersion(format!(
            "the server chose protocol 1.{}, but this client offered 1.{} to 1.{}",
            welcome.proto, hello.proto_min, hello.proto_max
        ))))
    }

    fn reauth_at(&self, token: &AccessToken) -> Instant {
        let wait = self
            .auth
            .refresh_at(token)
            .duration_since(SystemTime::now())
            .unwrap_or_default();
        Instant::now() + wait.clamp(MIN_REAUTH_WAIT, MAX_REAUTH_WAIT)
    }

    /// Reads one WebSocket item: a server message, nothing to act on, or the end of the connection.
    async fn frame(
        &mut self,
        item: Option<Result<WsMessage, tungstenite::Error>>,
    ) -> Result<Option<Msg>, Exit> {
        match item {
            None => Err(Exit::Reconnect {
                reason: DisconnectReason::Closed {
                    code: None,
                    reason: String::new(),
                },
                now: false,
            }),
            Some(Err(tungstenite::Error::Capacity(_))) => {
                Err(Exit::violation("a message larger than 1 MiB"))
            }
            Some(Err(error)) => Err(Exit::transport(error)),
            Some(Ok(WsMessage::Binary(bytes))) => match ServerMessage::decode(bytes) {
                Ok(message) => Ok(message.msg),
                Err(_) => Err(Exit::violation("a malformed ServerMessage")),
            },
            Some(Ok(WsMessage::Close(frame))) => {
                let (code, reason) = frame
                    .map(|f| (Some(u16::from(f.code)), f.reason.to_string()))
                    .unwrap_or_default();
                Err(self.closed(code, reason))
            }
            Some(Ok(_)) => Ok(None),
        }
    }

    /// The server is closing the connection with this code.
    fn closed(&mut self, code: Option<u16>, reason: String) -> Exit {
        match code {
            Some(CLOSE_UNSUPPORTED_VERSION) => Exit::Stop(StopReason::UnsupportedVersion(reason)),
            Some(CLOSE_DEVICE_REVOKED) => Exit::Stop(StopReason::DeviceRevoked),
            Some(CLOSE_TOKEN_EXPIRED) => {
                self.expired = self.token.clone();
                Exit::Reconnect {
                    reason: DisconnectReason::Closed { code, reason },
                    now: true,
                }
            }
            _ => Exit::Reconnect {
                reason: DisconnectReason::Closed { code, reason },
                now: false,
            },
        }
    }

    async fn dispatch(&mut self, msg: Msg, socket: &mut Socket) -> Result<(), Exit> {
        match msg {
            Msg::Changes(changes) => {
                let fed = self.stream.changes(changes.commits);
                self.feed(fed, socket).await
            }
            Msg::CaughtUp(caught_up) => {
                let fed = self.stream.caught_up(caught_up.head_seq);
                self.feed(fed, socket).await
            }
            Msg::ResyncRequired(resync) => {
                let fed = self.stream.resync(resync);
                self.feed(fed, socket).await
            }
            Msg::Rpc(response) => self.answer(response, socket).await,
            Msg::Notice(notice) => {
                self.emit(Event::Notice(notice));
                Ok(())
            }
            Msg::Goodbye(goodbye) => Err(self.closed(Some(goodbye.code as u16), goodbye.reason)),
            Msg::Welcome(_) => Err(Exit::violation("a second Welcome")),
        }
    }

    async fn answer(&mut self, response: RpcResponse, socket: &mut Socket) -> Result<(), Exit> {
        let Some(call) = self.pending.remove(&response.req_id) else {
            tracing::debug!(
                req_id = response.req_id,
                "flarebox session got a response to no pending call"
            );
            return Ok(());
        };
        let outcome = match response.result {
            None => Err(CallError::UnexpectedResult),
            Some(rpc_response::Result::Error(error)) => Err(CallError::Server(ServerError(error))),
            Some(rpc_response::Result::Commit(result)) if let Some(slot) = &call.commit => {
                let fed = self.stream.committed(Committed {
                    commit_id: slot.commit_id.clone(),
                    result: result.clone(),
                });
                self.feed(fed, socket).await?;
                Ok(rpc_response::Result::Commit(result))
            }
            Some(result) => Ok(result),
        };
        let _ = call.reply.send(outcome);
        Ok(())
    }

    async fn feed(&mut self, fed: Fed, socket: &mut Socket) -> Result<(), Exit> {
        for event in fed.events {
            self.emit(event);
        }
        match fed.resume {
            Some(resume) => self.send(socket, client_message::Msg::Resume(resume)).await,
            None => Ok(()),
        }
    }

    async fn send(&self, socket: &mut Socket, msg: client_message::Msg) -> Result<(), Exit> {
        let bytes = ClientMessage { msg: Some(msg) }.encode_to_vec();
        self.write(socket, WsMessage::Binary(bytes.into())).await
    }

    /// Sends one WebSocket message within `write_timeout`.
    async fn write(&self, socket: &mut Socket, message: WsMessage) -> Result<(), Exit> {
        match timeout(self.config.write_timeout, socket.send(message)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => Err(Exit::transport(error)),
            Err(_) => Err(Exit::timeout()),
        }
    }

    fn req_id(&mut self) -> u64 {
        let req_id = self.next_req;
        self.next_req += 1;
        req_id
    }

    fn emit(&self, event: Event) {
        let _ = self.events.send(event);
    }
}

/// A token refresh for `Reauth`, run alongside the connection so a slow `/v1/token` does not hold up the stream.
type Refresh = Pin<Box<dyn Future<Output = Result<Result<AccessToken, ApiError>, Elapsed>> + Send>>;

/// The result of the refresh in progress; never resolves when there is none.
async fn refreshed(
    refreshing: &mut Option<Refresh>,
) -> Result<Result<AccessToken, ApiError>, Elapsed> {
    match refreshing {
        Some(refresh) => refresh.await,
        None => std::future::pending().await,
    }
}

/// Resolves once the session is closed or dropped.
async fn closed(closing: &mut watch::Receiver<bool>) {
    let _ = closing.wait_for(|closed| *closed).await;
}

#[cfg(test)]
mod tests;
