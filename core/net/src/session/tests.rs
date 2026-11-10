// SPDX-License-Identifier: Apache-2.0

//! Session behaviour against a scripted WebSocket server. The real Worker is covered by `tests/server.rs`.

use std::time::{Duration, UNIX_EPOCH};

use flarebox_proto::v1::{
    CaughtUp, Changes, CommitRecord, Error as ProtoError, Goodbye, Notice, Resume, ServerMessage,
    TreePage, client_message, server_message,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;

use super::*;
use crate::{Api, Credentials};
use flarebox_proto::v1::error::Code;

type Server = WebSocketStream<TcpStream>;

async fn listen() -> (TcpListener, Arc<Auth>) {
    listen_on("127.0.0.1:0", Duration::from_secs(3600), None).await
}

/// A scripted server at `address` and an `Auth` for it whose cached token expires after `lifetime`.
async fn listen_on(
    address: &str,
    lifetime: Duration,
    refresh_margin: Option<Duration>,
) -> (TcpListener, Arc<Auth>) {
    let listener = TcpListener::bind(address).await.unwrap();
    let base: Url = format!("http://{}", listener.local_addr().unwrap())
        .parse()
        .unwrap();
    let credentials = Credentials {
        user_id: "u".into(),
        device_id: "d".into(),
        refresh_credential: "r".into(),
    };
    let expires = SystemTime::now() + lifetime;
    let token = AccessToken {
        token: "token".into(),
        expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
    };
    let mut auth = Auth::new(Api::local_development(base).unwrap(), credentials).with_token(token);
    if let Some(margin) = refresh_margin {
        auth = auth.with_refresh_margin(margin);
    }
    (listener, Arc::new(auth))
}

fn config() -> SessionConfig {
    let mut config = SessionConfig::new("0190a0b0-0000-7000-8000-000000000001", "test", "0", 1);
    config.backoff = Backoff {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(50),
    };
    config
}

/// Accepts one connection, reads `Hello`, and answers with this `Welcome`.
async fn accept(listener: &TcpListener, welcome: Welcome) -> Server {
    let (tcp, _) = listener.accept().await.unwrap();
    accept_upgrade(tcp, welcome).await
}

/// Completes the upgrade on an accepted connection, reads `Hello`, and answers with this `Welcome`.
async fn accept_upgrade(tcp: TcpStream, welcome: Welcome) -> Server {
    let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
    assert!(matches!(recv(&mut ws).await, client_message::Msg::Hello(_)));
    push(&mut ws, server_message::Msg::Welcome(welcome)).await;
    ws
}

async fn recv(ws: &mut Server) -> client_message::Msg {
    loop {
        match ws.next().await.expect("the client hung up").unwrap() {
            WsMessage::Binary(bytes) => return ClientMessage::decode(bytes).unwrap().msg.unwrap(),
            _ => continue,
        }
    }
}

async fn recv_rpc(ws: &mut Server) -> RpcRequest {
    match recv(ws).await {
        client_message::Msg::Rpc(request) => request,
        other => panic!("expected an RpcRequest, got {other:?}"),
    }
}

async fn push(ws: &mut Server, msg: server_message::Msg) {
    ws.send(WsMessage::Binary(
        ServerMessage { msg: Some(msg) }.encode_to_vec().into(),
    ))
    .await
    .unwrap();
}

async fn reply(ws: &mut Server, req_id: u64, result: rpc_response::Result) {
    push(
        ws,
        server_message::Msg::Rpc(RpcResponse {
            req_id,
            result: Some(result),
        }),
    )
    .await;
}

fn welcome(history_id: &str, device_seq: u64) -> Welcome {
    Welcome {
        proto: 1,
        history_id: history_id.into(),
        device_seq,
        ..Default::default()
    }
}

fn commit(device_seq: u64) -> Commit {
    Commit {
        commit_id: "c1".into(),
        base_seq: 3,
        history_id: "h".into(),
        device_seq,
        ..Default::default()
    }
}

async fn next(events: &mut Events) -> Event {
    tokio::time::timeout(Duration::from_secs(5), events.next())
        .await
        .expect("no event within 5 s")
        .expect("events ended")
}

/// A `ServerMessage` carrying a `Notice` whose encoding is exactly `size` bytes.
fn notice_of_size(size: usize) -> Notice {
    let mut notice = Notice {
        message: String::new(),
        ..Default::default()
    };
    let encoded = |n: &Notice| {
        ServerMessage {
            msg: Some(server_message::Msg::Notice(n.clone())),
        }
        .encoded_len()
    };
    notice.message = "x".repeat(size - 16);
    while encoded(&notice) < size {
        notice.message.push('x');
    }
    while encoded(&notice) > size {
        notice.message.pop();
    }
    assert_eq!(encoded(&notice), size);
    notice
}

#[test]
fn a_commit_is_resent_only_when_welcome_accounts_for_its_slot() {
    let slot = CommitSlot {
        commit_id: "c".into(),
        history_id: "h".into(),
        device_seq: 5,
    };
    assert_eq!(may_resend(&slot, &welcome("h", 4)), Ok(()), "not accepted");
    assert_eq!(
        may_resend(&slot, &welcome("h", 5)),
        Ok(()),
        "accepted, response lost"
    );
    assert_eq!(
        may_resend(&slot, &welcome("h", 3)),
        Err(NotResent::DeviceSeqMismatch {
            slot: 5,
            welcome: 3
        })
    );
    assert_eq!(
        may_resend(&slot, &welcome("h", 6)),
        Err(NotResent::DeviceSeqMismatch {
            slot: 5,
            welcome: 6
        })
    );
    assert_eq!(
        may_resend(&slot, &welcome("other", 4)),
        Err(NotResent::HistoryChanged {
            commit: "h".into(),
            welcome: "other".into()
        })
    );
    let first = CommitSlot {
        device_seq: 1,
        ..slot
    };
    assert_eq!(may_resend(&first, &welcome("h", 0)), Ok(()));
}

#[test]
fn the_websocket_url_follows_the_base_scheme_and_path() {
    let url = |base: &str| {
        websocket_url(
            &Api::local_development(base.parse().unwrap())
                .unwrap()
                .base()
                .clone(),
            "0190-ab",
        )
        .unwrap()
        .to_string()
    };
    assert_eq!(
        url("http://127.0.0.1:8787"),
        "ws://127.0.0.1:8787/v1/spaces/0190-ab/ws"
    );
    assert_eq!(
        url("https://files.example.com/fb"),
        "wss://files.example.com/fb/v1/spaces/0190-ab/ws"
    );
    let base = Api::local_development("http://localhost".parse().unwrap())
        .unwrap()
        .base()
        .clone();
    assert!(websocket_url(&base, "../x").is_err());
}

#[tokio::test]
async fn responses_are_matched_to_calls_by_req_id() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let second = recv_rpc(&mut ws).await;
        assert_ne!(first.req_id, second.req_id);
        let page = |cursor: &str| {
            rpc_response::Result::Tree(TreePage {
                next_cursor: cursor.into(),
                ..Default::default()
            })
        };
        reply(&mut ws, second.req_id, page("second")).await;
        reply(&mut ws, first.req_id, page("first")).await;
        ws
    });
    let (a, b) = tokio::join!(session.list_tree("a", 1), async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        session.list_tree("b", 1).await
    });
    assert_eq!(a.unwrap().next_cursor, "first");
    assert_eq!(b.unwrap().next_cursor, "second");
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_message_of_1_mib_is_read_and_a_larger_one_drops_the_connection() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let largest = notice_of_size(MAX_SERVER_MESSAGE);
    let too_large = notice_of_size(MAX_SERVER_MESSAGE + 1);
    let server = tokio::spawn({
        let largest = largest.clone();
        async move {
            let mut ws = accept(&listener, welcome("h", 0)).await;
            push(&mut ws, server_message::Msg::Notice(largest)).await;
            let frame = ServerMessage {
                msg: Some(server_message::Msg::Notice(too_large)),
            }
            .encode_to_vec();
            let _ = ws.send(WsMessage::Binary(frame.into())).await;
            (listener, ws)
        }
    });
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert_eq!(next(&mut events).await, Event::Notice(largest));
    match next(&mut events).await {
        Event::Disconnected {
            reason: DisconnectReason::ProtocolViolation(what),
            ..
        } => assert!(what.contains("1 MiB"), "{what}"),
        other => panic!("expected a protocol violation, got {other:?}"),
    }
    drop(server.await.unwrap());
}

#[tokio::test]
async fn an_oversized_request_is_refused_before_it_is_sent() {
    let (_listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let chunks = vec![
        ChunkRef {
            hash: vec![7; 32],
            size: 1,
            encoding: 0
        };
        MAX_CLIENT_MESSAGE / 30
    ];
    match session.prepare_upload(chunks).await {
        Err(CallError::TooLarge { bytes }) => assert!(bytes > MAX_CLIENT_MESSAGE),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn a_revoked_device_stops_the_session_and_fails_pending_calls() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        recv_rpc(&mut ws).await;
        push(
            &mut ws,
            server_message::Msg::Goodbye(Goodbye {
                code: 4003,
                reason: "device revoked".into(),
            }),
        )
        .await;
        let _ = ws.close(None).await;
    });
    assert_eq!(
        session.list_trash("", 10).await,
        Err(CallError::Stopped(StopReason::DeviceRevoked))
    );
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert_eq!(
        next(&mut events).await,
        Event::Stopped(StopReason::DeviceRevoked)
    );
    assert_eq!(
        session.list_trash("", 10).await,
        Err(CallError::Stopped(StopReason::DeviceRevoked))
    );
    server.await.unwrap();
}

#[tokio::test]
async fn retry_later_is_sent_again_after_the_requested_wait() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let busy = ProtoError {
            code: Code::RetryLater as i32,
            retry_after_ms: 20,
            ..Default::default()
        };
        reply(&mut ws, first.req_id, rpc_response::Result::Error(busy)).await;
        let second = recv_rpc(&mut ws).await;
        assert_eq!(first.call, second.call);
        reply(
            &mut ws,
            second.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    session.list_trash("", 10).await.unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_dropped_commit_is_resent_unchanged_when_welcome_shows_it_was_accepted() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 1)).await;
        let sent = recv_rpc(&mut ws).await;
        drop(ws);
        let mut ws = accept(&listener, welcome("h", 2)).await;
        let resent = recv_rpc(&mut ws).await;
        assert_eq!(sent, resent, "the same request, req_id included");
        let result = CommitResult {
            seq: 4,
            ..Default::default()
        };
        reply(&mut ws, resent.req_id, rpc_response::Result::Commit(result)).await;
        ws
    });
    let result = session.commit(commit(2)).await.unwrap();
    assert_eq!(result.seq, 4);
    let mut seen = Vec::new();
    while !matches!(seen.last(), Some(Event::Committed(_))) {
        seen.push(next(&mut events).await);
    }
    assert!(
        matches!(
            seen[..],
            [
                Event::Connected(_),
                Event::Disconnected { .. },
                Event::Connected(_),
                Event::Committed(_)
            ]
        ),
        "{seen:?}"
    );
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_dropped_commit_is_not_resent_when_welcome_cannot_account_for_it() {
    for (later, expected) in [
        (
            welcome("h", 7),
            NotResent::DeviceSeqMismatch {
                slot: 2,
                welcome: 7,
            },
        ),
        (
            welcome("new", 1),
            NotResent::HistoryChanged {
                commit: "h".into(),
                welcome: "new".into(),
            },
        ),
    ] {
        let (listener, auth) = listen().await;
        let (session, _events) = Session::start(auth, config()).unwrap();
        let server = tokio::spawn(async move {
            let mut ws = accept(&listener, welcome("h", 1)).await;
            recv_rpc(&mut ws).await;
            drop(ws);
            let mut ws = accept(&listener, later).await;
            let after = recv_rpc(&mut ws).await;
            assert!(
                matches!(after.call, Some(Call::ListTree(_))),
                "the commit was not resent: {after:?}"
            );
            reply(
                &mut ws,
                after.req_id,
                rpc_response::Result::Tree(Default::default()),
            )
            .await;
            ws
        });
        assert_eq!(
            session.commit(commit(2)).await,
            Err(CallError::NotResent(expected))
        );
        session.list_tree("", 1).await.unwrap();
        drop(server.await.unwrap());
    }
}

#[tokio::test]
async fn the_stream_resumes_from_the_last_seq_after_a_reconnect() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    session.subscribe("h", 2);
    let record = |seq| CommitRecord {
        seq,
        ..Default::default()
    };
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        assert_eq!(
            recv(&mut ws).await,
            client_message::Msg::Resume(Resume {
                history_id: "h".into(),
                since_seq: 2
            })
        );
        push(
            &mut ws,
            server_message::Msg::Changes(Changes {
                commits: vec![record(3), record(4)],
            }),
        )
        .await;
        push(
            &mut ws,
            server_message::Msg::CaughtUp(CaughtUp { head_seq: 4 }),
        )
        .await;
        drop(ws);
        let mut ws = accept(&listener, welcome("h", 0)).await;
        assert_eq!(
            recv(&mut ws).await,
            client_message::Msg::Resume(Resume {
                history_id: "h".into(),
                since_seq: 4
            })
        );
        push(
            &mut ws,
            server_message::Msg::Changes(Changes {
                commits: vec![record(4), record(5)],
            }),
        )
        .await;
        ws
    });
    let mut changes = Vec::new();
    while changes.last() != Some(&5) {
        if let Event::Changes(records) = next(&mut events).await {
            changes.extend(records.iter().map(|r| r.seq));
        }
    }
    assert_eq!(changes, [3, 4, 5]);
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_welcome_with_a_protocol_not_offered_stops_before_anything_is_sent() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    session.subscribe("h", 0);
    let server = tokio::spawn(async move {
        let mut ws = accept(
            &listener,
            Welcome {
                proto: 999,
                ..welcome("h", 0)
            },
        )
        .await;
        let after = tokio::time::timeout(Duration::from_secs(1), ws.next()).await;
        assert!(
            !matches!(after, Ok(Some(Ok(WsMessage::Binary(_))))),
            "no Resume or RPC after an unsupported Welcome: {after:?}"
        );
    });
    let call = session.list_tree("", 1).await;
    let Err(CallError::Stopped(StopReason::UnsupportedVersion(why))) = call else {
        panic!("expected UnsupportedVersion, got {call:?}");
    };
    assert!(why.contains("1.999"), "{why}");
    match next(&mut events).await {
        Event::Stopped(StopReason::UnsupportedVersion(_)) => {}
        other => panic!("no Connected for an unsupported Welcome, got {other:?}"),
    }
    server.await.unwrap();
}

#[tokio::test]
async fn a_withheld_upgrade_times_out_and_close_does_not_wait_for_it() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.connect_timeout = Duration::from_millis(30);
    config.backoff = Backoff {
        initial: Duration::from_secs(5),
        max: Duration::from_secs(5),
    };
    let (session, mut events) = Session::start(auth.clone(), config).unwrap();
    let (mut tcp, _) = listener.accept().await.unwrap();
    let mut buffer = [0; 4096];
    assert!(
        tcp.read(&mut buffer).await.unwrap() > 0,
        "the upgrade request arrived"
    );
    let event = tokio::time::timeout(Duration::from_millis(500), events.next()).await;
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Timeout,
                ..
            }))
        ),
        "{event:?}"
    );
    let closed = tokio::time::timeout(Duration::from_millis(150), session.close()).await;
    assert!(
        closed.is_ok(),
        "close interrupts the wait before the next attempt"
    );

    let mut config = self::config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, _events) = Session::start(auth, config).unwrap();
    let (mut tcp, _) = listener.accept().await.unwrap();
    assert!(tcp.read(&mut buffer).await.unwrap() > 0);
    let closed = tokio::time::timeout(Duration::from_millis(150), session.close()).await;
    assert!(closed.is_ok(), "close interrupts a withheld upgrade");
}

#[tokio::test]
async fn responses_to_unknown_or_answered_req_ids_are_ignored() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let page = |cursor: &str| {
            rpc_response::Result::Tree(TreePage {
                next_cursor: cursor.into(),
                ..Default::default()
            })
        };
        let first = recv_rpc(&mut ws).await;
        reply(&mut ws, first.req_id + 999, page("unknown")).await;
        reply(&mut ws, first.req_id, page("first")).await;
        reply(&mut ws, first.req_id, page("duplicate")).await;
        let second = recv_rpc(&mut ws).await;
        reply(&mut ws, second.req_id, page("second")).await;
        ws
    });
    assert_eq!(session.list_tree("", 1).await.unwrap().next_cursor, "first");
    assert_eq!(
        session.list_tree("", 1).await.unwrap().next_cursor,
        "second"
    );
    drop(server.await.unwrap());
}

#[tokio::test]
async fn a_malformed_server_message_drops_the_connection_as_a_protocol_violation() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        ws.send(WsMessage::Binary(vec![0xff].into())).await.unwrap();
        (listener, ws)
    });
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    assert!(matches!(
        next(&mut events).await,
        Event::Disconnected {
            reason: DisconnectReason::ProtocolViolation(_),
            ..
        }
    ));
    drop(server.await.unwrap());
}

/// Queues about 10 MB of protocol-sized requests on a connection whose server has stopped reading, so the session
/// blocks in a socket write.
async fn block_a_write(session: &Arc<Session>) {
    let calls = tokio::spawn({
        let session = session.clone();
        async move {
            let chunks = vec![
                ChunkRef {
                    hash: vec![7; 32],
                    size: 1,
                    ..Default::default()
                };
                1000
            ];
            futures_util::future::join_all((0..250).map(|_| session.prepare_upload(chunks.clone())))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!calls.is_finished(), "the server answers nothing");
    calls.abort();
    let _ = calls.await;
}

#[tokio::test]
async fn close_interrupts_a_write_the_server_does_not_read() {
    let (listener, auth) = listen().await;
    let (session, mut events) = Session::start(auth, config()).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let session = Arc::new(session);
    block_a_write(&session).await;
    let session = Arc::try_unwrap(session).unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the blocked write");
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Closed)
    ));
}

#[tokio::test]
async fn a_write_the_server_does_not_read_times_out() {
    let (listener, auth) = listen().await;
    let mut config = config();
    config.write_timeout = Duration::from_millis(200);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let session = Arc::new(session);
    let disconnected =
        tokio::spawn(
            async move { tokio::time::timeout(Duration::from_secs(2), events.next()).await },
        );
    block_a_write(&session).await;
    let event = disconnected.await.unwrap();
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Timeout,
                ..
            }))
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn an_ipv6_loopback_base_connects() {
    let (listener, auth) = listen_on("[::1]:0", Duration::from_secs(3600), None).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _server = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
}

#[tokio::test]
async fn retry_later_waits_as_long_as_the_server_asks_beyond_the_backoff_limit() {
    let (listener, auth) = listen().await;
    let config = config();
    assert!(config.backoff.max < Duration::from_millis(300));
    let (session, _events) = Session::start(auth, config).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        let first = recv_rpc(&mut ws).await;
        let busy = ProtoError {
            code: Code::RetryLater as i32,
            retry_after_ms: 300,
            ..Default::default()
        };
        reply(&mut ws, first.req_id, rpc_response::Result::Error(busy)).await;
        let asked = Instant::now();
        let second = recv_rpc(&mut ws).await;
        assert!(
            asked.elapsed() >= Duration::from_millis(300),
            "{:?}",
            asked.elapsed()
        );
        reply(
            &mut ws,
            second.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    session.list_trash("", 10).await.unwrap();
    drop(server.await.unwrap());
}

#[tokio::test]
async fn the_connection_keeps_serving_while_a_token_refresh_is_withheld() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let mut config = config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let mut ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut token_request, _) = listener.accept().await.unwrap();
    assert!(
        token_request.read(&mut [0; 4096]).await.unwrap() > 0,
        "the refresh is under way"
    );
    let server = tokio::spawn(async move {
        let request = recv_rpc(&mut ws).await;
        reply(
            &mut ws,
            request.req_id,
            rpc_response::Result::Tree(Default::default()),
        )
        .await;
        ws
    });
    let listed = tokio::time::timeout(Duration::from_millis(500), session.list_tree("", 1)).await;
    assert!(
        matches!(listed, Ok(Ok(_))),
        "an RPC is served while the refresh waits: {listed:?}"
    );
    let _ws = server.await.unwrap();
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the refresh");
}

#[tokio::test]
async fn tokens_that_are_due_at_once_are_not_refreshed_in_a_loop() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_secs(3600),
        Some(Duration::from_secs(7200)),
    )
    .await;
    let issued = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let server = tokio::spawn({
        let issued = issued.clone();
        async move {
            let mut sockets = Vec::new();
            loop {
                let (mut tcp, _) = listener.accept().await.unwrap();
                let mut start = [0; 4];
                tcp.peek(&mut start).await.unwrap();
                if &start != b"POST" {
                    sockets.push(accept_upgrade(tcp, welcome("h", 0)).await);
                    continue;
                }
                let n = issued.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut request = [0; 4096];
                let _ = tcp.read(&mut request).await.unwrap();
                let expires = SystemTime::now() + Duration::from_secs(3600);
                let body = AccessToken {
                    token: format!("token-{n}"),
                    expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
                }
                .encode_to_vec();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                tcp.write_all(head.as_bytes()).await.unwrap();
                tcp.write_all(&body).await.unwrap();
            }
        }
    });
    let (session, mut events) = Session::start(auth, config()).unwrap();
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let issued = issued.load(std::sync::atomic::Ordering::SeqCst);
    assert!(issued <= 3, "{issued} tokens in 1.5 s");
    session.close().await;
    server.abort();
}

#[tokio::test]
async fn a_token_expiry_close_during_a_refresh_reconnects_with_a_new_token() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let mut config = config();
    config.connect_timeout = Duration::from_secs(60);
    let (session, mut events) = Session::start(auth, config).unwrap();
    let mut ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    assert!(
        refresh.read(&mut [0; 4096]).await.unwrap() > 0,
        "a refresh is in flight and gets no answer"
    );
    let goodbye = Goodbye {
        code: u32::from(CLOSE_TOKEN_EXPIRED),
        reason: "expired".into(),
    };
    push(&mut ws, server_message::Msg::Goodbye(goodbye)).await;
    let event = tokio::time::timeout(Duration::from_millis(500), events.next()).await;
    assert!(
        matches!(
            event,
            Ok(Some(Event::Disconnected {
                reason: DisconnectReason::Closed {
                    code: Some(CLOSE_TOKEN_EXPIRED),
                    ..
                },
                ..
            }))
        ),
        "the expiry is handled while the refresh waits: {event:?}"
    );
    let (mut token_request, _) =
        tokio::time::timeout(Duration::from_millis(500), listener.accept())
            .await
            .expect("the session asks for a new token to reconnect")
            .unwrap();
    assert!(token_request.read(&mut [0; 4096]).await.unwrap() > 0);
    let closed = tokio::time::timeout(Duration::from_millis(500), session.close()).await;
    assert!(closed.is_ok(), "close does not wait for the token request");
    drop(refresh);
}

/// Answers one HTTP request on an accepted connection with this status and a protobuf `Error`.
async fn answer_with_error(tcp: &mut TcpStream, status: u16, error: ProtoError) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let body = error.encode_to_vec();
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    tcp.write_all(&[head.as_bytes(), &body].concat())
        .await
        .unwrap();
}

fn refusal(code: i32, retry_after_ms: u64) -> ProtoError {
    ProtoError {
        code,
        retry_after_ms,
        message: "refused".into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn only_retry_later_and_quota_with_a_wait_are_resent() {
    let (listener, auth) = listen().await;
    let (session, _events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        for code in [999, Code::Unspecified as i32, Code::QuotaExceeded as i32] {
            let request = recv_rpc(&mut ws).await;
            assert!(
                matches!(request.call, Some(Call::ListTree(_))),
                "a new call, not a resend"
            );
            reply(
                &mut ws,
                request.req_id,
                rpc_response::Result::Error(refusal(code, 0)),
            )
            .await;
        }
        let request = recv_rpc(&mut ws).await;
        assert!(
            matches!(request.call, Some(Call::ListTrash(_))),
            "the refused calls were not resent"
        );
        let quota = refusal(Code::QuotaExceeded as i32, 200);
        reply(&mut ws, request.req_id, rpc_response::Result::Error(quota)).await;
        let asked = Instant::now();
        let resent = recv_rpc(&mut ws).await;
        assert!(
            asked.elapsed() >= Duration::from_millis(200),
            "{:?}",
            asked.elapsed()
        );
        assert_eq!(
            request.call, resent.call,
            "a quota with a wait is resent after it"
        );
        reply(
            &mut ws,
            resent.req_id,
            rpc_response::Result::Trash(Default::default()),
        )
        .await;
        ws
    });
    for code in [999, Code::Unspecified as i32, Code::QuotaExceeded as i32] {
        let refused = session.list_tree("", 1).await;
        assert!(
            matches!(&refused, Err(CallError::Server(error)) if error.0.code == code),
            "{refused:?}"
        );
    }
    session.list_trash("", 1).await.unwrap();
    drop(server.await.unwrap());
}
#[tokio::test]
async fn a_token_refused_with_an_unknown_code_stops_the_session_without_asking_again() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut token_request, _) = listener.accept().await.unwrap();
    answer_with_error(&mut token_request, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));
    let again = tokio::time::timeout(Duration::from_millis(300), listener.accept()).await;
    assert!(again.is_err(), "no second token request");
}

#[tokio::test]
async fn an_upgrade_refused_with_an_unknown_code_stops_the_session_and_a_known_one_reconnects() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_with_error(&mut upgrade, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_with_error(&mut upgrade, 500, refusal(Code::Internal as i32, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 500 },
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn a_refresh_refused_with_an_unknown_code_stops_the_session() {
    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    answer_with_error(&mut refresh, 500, refusal(999, 0)).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));
}

/// Answers one HTTP request with this status and a protobuf body, sending the body 30 ms after the head.
async fn answer_in_two_parts(tcp: &mut TcpStream, status: u16, body: Vec<u8>) {
    let mut request = [0; 4096];
    assert!(tcp.read(&mut request).await.unwrap() > 0);
    let head = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\n\r\n",
        body.len()
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
    tcp.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;
    tcp.write_all(&body).await.unwrap();
}

#[tokio::test]
async fn an_upgrade_refusal_whose_body_arrives_after_its_head_is_read_whole() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_two_parts(&mut upgrade, 500, refusal(999, 0).encode_to_vec()).await;
    assert!(matches!(
        next(&mut events).await,
        Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")
    ));

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let quota = refusal(Code::QuotaExceeded as i32, 300);
    answer_in_two_parts(&mut upgrade, 429, quota.encode_to_vec()).await;
    match next(&mut events).await {
        Event::Disconnected {
            reason: DisconnectReason::Refused { status: 429 },
            retry_in,
        } => assert!(retry_in >= Duration::from_millis(300), "{retry_in:?}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn an_empty_upgrade_refusal_is_left_to_its_status() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_two_parts(&mut upgrade, 503, Vec::new()).await;
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 503 },
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn a_401_with_an_unknown_code_stops_the_session_with_its_message() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut token_request, _) = listener.accept().await.unwrap();
    answer_with_error(&mut token_request, 401, refusal(999, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "not CredentialsRefused: {event:?}"
    );

    let (listener, auth) = listen_on(
        "127.0.0.1:0",
        Duration::from_millis(300),
        Some(Duration::ZERO),
    )
    .await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let _ws = accept(&listener, welcome("h", 0)).await;
    assert!(matches!(next(&mut events).await, Event::Connected(_)));
    let (mut refresh, _) = listener.accept().await.unwrap();
    answer_with_error(&mut refresh, 401, refusal(999, 0)).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "{event:?}"
    );
}

/// Reads one request, then writes `parts` 30 ms apart, and returns the request's head.
async fn answer_in_parts(tcp: &mut TcpStream, parts: Vec<Vec<u8>>) -> String {
    let mut request = [0; 4096];
    let n = tcp.read(&mut request).await.unwrap();
    for part in parts {
        tcp.write_all(&part).await.unwrap();
        tcp.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    String::from_utf8_lossy(&request[..n]).into_owned()
}

#[tokio::test]
async fn chunked_and_close_delimited_upgrade_refusals_are_read_whole() {
    let (listener, auth) = listen().await;
    let body = refusal(999, 0).encode_to_vec();
    let (first, rest) = body.split_at(3);
    let chunked = vec![
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ntransfer-encoding: chunked\r\n\r\n".to_vec(),
        [format!("{:x}\r\n", first.len()).as_bytes(), first, b"\r\n"].concat(),
        [format!("{:x}\r\n", rest.len()).as_bytes(), rest, b"\r\n0\r\n\r\n"].concat(),
    ];
    let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    answer_in_parts(&mut upgrade, chunked).await;
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "chunked: {event:?}"
    );

    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let head =
        b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\nconnection: close\r\n\r\n"
            .to_vec();
    answer_in_parts(&mut upgrade, vec![head, first.to_vec(), rest.to_vec()]).await;
    drop(upgrade);
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Stopped(StopReason::Refused(why)) if why.ends_with(": refused")),
        "close-delimited: {event:?}"
    );
}

#[tokio::test]
async fn a_refusal_whose_error_cannot_be_read_is_retried_a_bounded_number_of_times_in_a_row() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let server = tokio::spawn(async move {
        let mut attempts = 0;
        while let Ok(Ok((mut upgrade, _))) =
            tokio::time::timeout(Duration::from_secs(2), listener.accept()).await
        {
            attempts += 1;
            let answer = if attempts == MAX_UNREADABLE_REFUSALS {
                // A refusal that is read ends the run.
                vec![b"HTTP/1.1 500 X\r\ncontent-length: 0\r\n\r\n".to_vec()]
            } else if attempts % 2 == 0 {
                // A whole body that is not an `Error`.
                vec![
                    b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 3\r\n\r\n".to_vec(),
                    vec![0xff; 3],
                ]
            } else {
                // A body cut short.
                vec![
                    b"HTTP/1.1 500 X\r\ncontent-type: application/x-protobuf\r\ncontent-length: 1000\r\n\r\n".to_vec(),
                    vec![0x08; 10],
                ]
            };
            answer_in_parts(&mut upgrade, answer).await;
        }
        attempts
    });
    let stopped = loop {
        match next(&mut events).await {
            Event::Disconnected {
                reason: DisconnectReason::Refused { status: 500 },
                ..
            } => {}
            other => break other,
        }
    };
    assert!(
        matches!(&stopped, Event::Stopped(StopReason::Refused(why)) if why.contains("could not be read")),
        "{stopped:?}"
    );
    assert_eq!(server.await.unwrap(), 2 * MAX_UNREADABLE_REFUSALS);
}

#[tokio::test]
async fn a_101_without_upgrade_and_connection_headers_is_not_a_websocket() {
    let (listener, auth) = listen().await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut upgrade, _) = listener.accept().await.unwrap();
    let mut request = [0; 4096];
    let n = upgrade.read(&mut request).await.unwrap();
    let request = String::from_utf8_lossy(&request[..n]).into_owned();
    let key = request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-key")
                .then(|| value.trim().to_owned())
        })
        .unwrap();
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    let answer =
        format!("HTTP/1.1 101 Switching Protocols\r\nsec-websocket-accept: {accept}\r\n\r\n");
    upgrade.write_all(answer.as_bytes()).await.unwrap();
    let event = next(&mut events).await;
    assert!(
        matches!(
            event,
            Event::Disconnected {
                reason: DisconnectReason::Transport(_),
                ..
            }
        ),
        "{event:?}"
    );
}

#[tokio::test]
async fn malformed_chunked_refusals_are_left_to_their_status() {
    let (listener, auth) = listen().await;
    let body = refusal(999, 0).encode_to_vec();
    let head = b"HTTP/1.1 503 X\r\ncontent-type: application/x-protobuf\r\ntransfer-encoding: chunked\r\n\r\n";
    let oversized = [b"ffffffffffffffff\r\n".as_slice(), &body].concat();
    let unterminated = [
        format!("{:x}\r\n", body.len()).as_bytes(),
        &body,
        b"\r\n0\r\n",
    ]
    .concat();
    for (case, rest) in [("oversized", oversized), ("unterminated", unterminated)] {
        let (_session, mut events) = Session::start(auth.clone(), config()).unwrap();
        let (mut upgrade, _) = listener.accept().await.unwrap();
        answer_in_parts(&mut upgrade, vec![head.to_vec(), rest]).await;
        drop(upgrade);
        let event = next(&mut events).await;
        assert!(
            matches!(
                event,
                Event::Disconnected {
                    reason: DisconnectReason::Refused { status: 503 },
                    ..
                }
            ),
            "{case}: {event:?}"
        );
    }
}

#[tokio::test]
async fn an_oversized_token_answer_is_a_transport_failure_and_is_asked_again() {
    let (listener, auth) = listen_on("127.0.0.1:0", Duration::ZERO, Some(Duration::ZERO)).await;
    let (_session, mut events) = Session::start(auth, config()).unwrap();
    let (mut token_request, _) = listener.accept().await.unwrap();
    let mut request = [0; 4096];
    assert!(token_request.read(&mut request).await.unwrap() > 0);
    tokio::spawn(async move {
        let head = b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 1000000\r\n\r\n";
        let _ = token_request
            .write_all(&[head.as_slice(), &[0x0a; 1_000_000]].concat())
            .await;
    });
    let event = next(&mut events).await;
    assert!(
        matches!(&event, Event::Disconnected { reason: DisconnectReason::Transport(why), .. } if why.contains("more than 65536 bytes")),
        "{event:?}"
    );
    let again = tokio::time::timeout(Duration::from_secs(2), listener.accept()).await;
    assert!(again.is_ok(), "the token is asked for again");
}

/// `refusal_body` fed arbitrary and truncated answers.
mod refusal_reader {
    use proptest::prelude::*;

    use super::*;

    type Framing = Option<(&'static str, String)>;

    fn head(framing: Framing) -> tungstenite::http::response::Parts {
        let mut response = tungstenite::http::Response::builder()
            .status(503)
            .header(CONTENT_TYPE, PROTOBUF);
        if let Some((name, value)) = framing {
            response = response.header(name, value);
        }
        response.body(()).unwrap().into_parts().0
    }

    fn chunked() -> Framing {
        Some(("transfer-encoding", "chunked".into()))
    }

    fn length(length: usize) -> Framing {
        Some(("content-length", length.to_string()))
    }

    /// Reads an answer whose first `split` bytes tungstenite already read, and whose connection then ends.
    fn read(framing: Framing, answer: &[u8], split: usize) -> Option<Bytes> {
        let (tail, rest) = answer.split_at(split.min(answer.len()));
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(refusal_body(&head(framing), tail.to_vec(), rest))
    }

    fn encode_chunked(body: &[u8], sizes: &[usize]) -> Vec<u8> {
        let mut encoded = Vec::new();
        let mut rest = body;
        for size in sizes.iter().cycle() {
            if rest.is_empty() {
                break;
            }
            let (chunk, after) = rest.split_at((*size).min(rest.len()));
            encoded.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
            encoded.extend_from_slice(chunk);
            encoded.extend_from_slice(b"\r\n");
            rest = after;
        }
        encoded.extend_from_slice(b"0\r\n\r\n");
        encoded
    }

    #[test]
    fn a_body_past_the_bound_is_not_read() {
        let body = vec![0; MAX_REFUSAL + 1];
        assert_eq!(read(length(body.len()), &body, 0), None);
        assert_eq!(read(chunked(), &encode_chunked(&body, &[4096]), 0), None);
        assert_eq!(read(None, &body, 0), None);
    }

    proptest! {
        #[test]
        fn arbitrary_answers_are_bounded_and_never_panic(
            framing in prop_oneof![Just(None), Just(chunked()), (0usize..100_000).prop_map(length)],
            size_line in "([0-9a-fA-F]{1,20}(;[a-z]{0,3})?\r\n)?",
            bytes in proptest::collection::vec(any::<u8>(), 0..512),
            split in any::<usize>(),
        ) {
            let answer = [size_line.as_bytes(), &bytes].concat();
            if let Some(body) = read(framing, &answer, split) {
                prop_assert!(body.len() <= MAX_REFUSAL);
            }
        }

        #[test]
        fn complete_bodies_are_read_whole_and_cut_ones_are_not(
            body in proptest::collection::vec(any::<u8>(), 0..2048),
            sizes in proptest::collection::vec(1usize..600, 1..8),
            is_chunked in any::<bool>(),
            split in any::<usize>(),
            cut in any::<prop::sample::Index>(),
        ) {
            let (framing, encoded) = if is_chunked {
                (chunked(), encode_chunked(&body, &sizes))
            } else {
                (length(body.len()), body.clone())
            };
            let read_whole = read(framing.clone(), &encoded, split);
            prop_assert_eq!(read_whole.as_deref(), Some(body.as_slice()));
            if !encoded.is_empty() {
                let cut = cut.index(encoded.len());
                prop_assert_eq!(read(framing, &encoded[..cut], split), None);
            }
        }
    }
}
