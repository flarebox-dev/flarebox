// SPDX-License-Identifier: Apache-2.0

//! One-request HTTP servers that answer chunk transfers in scripted ways.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use koidrive_proto::v1::Error as ProtoError;
use koidrive_proto::v1::error::Code;
use koidrive_proto::v1::{ChunkPlan, PrepareUploadResult, rpc_request, rpc_response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use super::*;
use crate::session::Events;
use crate::session::tests::{accept, config, listen, recv_rpc, reply, welcome};

fn chunk(bytes: &[u8]) -> ChunkRef {
    ChunkRef {
        hash: Sha256::digest(bytes).to_vec(),
        size: bytes.len() as u32,
        encoding: 0,
    }
}

fn transfer() -> Transfer {
    let api = Api::local_development("http://127.0.0.1:9".parse().unwrap()).unwrap();
    Transfer {
        backoff: Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(5),
        },
        ..Transfer::new(&api)
    }
}

/// Serves each scripted response to one request, in order, and returns the target URL and the request bodies.
async fn serve(
    responses: Vec<(u16, &'static str, Vec<u8>)>,
) -> (Target, tokio::task::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/v1/spaces/s/chunks/x?ticket=secret",
        listener.local_addr().unwrap()
    );
    let server = tokio::spawn(async move {
        let mut bodies = Vec::new();
        for (status, content_type, body) in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            bodies.push(read_request(&mut socket).await.1);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&body).await.unwrap();
        }
        bodies
    });
    (
        Target {
            url,
            method: String::new(),
            headers: Default::default(),
            expires_at_ms: 0,
        },
        server,
    )
}

/// Reads one request and returns its head and body.
async fn read_request(socket: &mut TcpStream) -> (String, Vec<u8>) {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let (head_end, length) = loop {
        let n = socket.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        let n = socket.read(&mut buffer).await.unwrap();
        request.extend_from_slice(&buffer[..n]);
    }
    (
        String::from_utf8_lossy(&request[..head_end]).into_owned(),
        request[head_end..].to_vec(),
    )
}

/// Answers one request with a body that never ends: `part` again and again, `every` apart, until the client leaves.
async fn endless(
    status: u16,
    content_type: &'static str,
    part: Vec<u8>,
    every: Duration,
) -> (Target, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chunk", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        let head = format!(
            "HTTP/1.1 {status} X\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\n\r\n"
        );
        let mut framed = format!("{:x}\r\n", part.len()).into_bytes();
        framed.extend_from_slice(&part);
        framed.extend_from_slice(b"\r\n");
        let _ = socket.write_all(head.as_bytes()).await;
        while socket.write_all(&framed).await.is_ok() {
            tokio::time::sleep(every).await;
        }
    });
    let target = Target {
        url,
        ..Default::default()
    };
    (target, server)
}

#[test]
fn matches_checks_hash_and_size() {
    let good = chunk(b"hello");
    assert!(matches(&good, b"hello"));
    assert!(!matches(&good, b"hellp"));
    assert!(!matches(&ChunkRef { size: 6, ..good }, b"hello"));
}

#[test]
fn distinct_keeps_the_first_of_each_hash_in_order() {
    let (a, b) = (chunk(b"a"), chunk(b"b"));
    assert_eq!(distinct(&[a.clone(), b.clone(), a.clone()]), vec![a, b]);
}

#[test]
fn a_target_near_its_expiry_is_renewed() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let target = |expires_at_ms| Target {
        expires_at_ms,
        ..Default::default()
    };
    assert!(expiring(&target(now + 30_000)));
    assert!(!expiring(&target(now + 600_000)));
    assert!(!expiring(&target(0)), "no expiry given");
}

#[tokio::test]
async fn a_download_that_does_not_match_its_hash_is_retried_and_then_refused() {
    let wanted = chunk(b"the right bytes");
    let (target, server) = serve(vec![
        (200, "application/octet-stream", b"the wrong bytes".to_vec()),
        (
            200,
            "application/octet-stream",
            b"the right bytes!".to_vec(),
        ),
        (200, "application/octet-stream", b"the right bytes".to_vec()),
    ])
    .await;
    let transfer = transfer();
    for _ in 0..2 {
        assert!(matches!(
            transfer.get(&target, &wanted).await,
            Attempt::Retry(ChunkError::Corrupt { .. })
        ));
    }
    assert!(
        matches!(transfer.get(&target, &wanted).await, Attempt::Done(bytes) if bytes == b"the right bytes"[..])
    );
    server.await.unwrap();

    let (target, server) =
        serve(vec![(200, "application/octet-stream", b"nope".to_vec()); 3]).await;
    let transfer = Transfer {
        attempts: 3,
        ..transfer
    };
    let outcome = transfer
        .retrying(async || transfer.get(&target, &wanted).await)
        .await;
    assert!(
        matches!(outcome, Err(ChunkError::Corrupt { .. })),
        "{outcome:?}"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn an_upload_returns_the_relay_receipt_and_sends_the_bytes_unchanged() {
    let sent = chunk(b"payload");
    let receipt = UploadReceipt {
        hash: sent.hash.clone(),
        size: sent.size,
        token: "v1.1.mac".into(),
    };
    let (target, server) = serve(vec![(
        200,
        "application/x-protobuf",
        receipt.encode_to_vec(),
    )])
    .await;
    let outcome = transfer()
        .put(&target, &sent, Bytes::from_static(b"payload"))
        .await;
    assert!(matches!(outcome, Attempt::Done(Some(r)) if r == receipt));
    assert_eq!(server.await.unwrap(), [b"payload".to_vec()]);
}

#[tokio::test]
async fn upload_responses_are_classified() {
    let sent = chunk(b"payload");
    let mismatch = ProtoError {
        code: 3,
        message: "the body does not match the chunk hash".into(),
        ..Default::default()
    };
    let other = UploadReceipt {
        hash: vec![0; 32],
        size: 7,
        token: "t".into(),
    };
    let (target, server) = serve(vec![
        (
            403,
            "application/x-protobuf",
            ProtoError {
                code: 2,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (400, "application/x-protobuf", mismatch.encode_to_vec()),
        (503, "text/plain", Vec::new()),
        (
            413,
            "application/x-protobuf",
            ProtoError {
                code: 5,
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (200, "application/x-protobuf", other.encode_to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let transfer = transfer();
    let put = async || {
        transfer
            .put(&target, &sent, Bytes::from_static(b"payload"))
            .await
    };
    assert!(
        matches!(put().await, Attempt::Expired),
        "a refused ticket is renewed"
    );
    match put().await {
        Attempt::Retry(ChunkError::Refused {
            status: 400,
            error: Some(error),
            ..
        }) => assert_eq!(error.message(), mismatch.message),
        _ => panic!("a hash mismatch on the way is retried"),
    }
    assert!(matches!(
        put().await,
        Attempt::Retry(ChunkError::Refused { status: 503, .. })
    ));
    assert!(matches!(
        put().await,
        Attempt::Fail(ChunkError::Refused { status: 413, .. })
    ));
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "a receipt for another chunk is refused"
    );
    assert!(
        matches!(put().await, Attempt::Done(None)),
        "a presigned upload has no receipt"
    );
    server.await.unwrap();
}

#[test]
fn errors_do_not_show_tickets() {
    let error = ChunkError::Unavailable {
        hash: vec![0xab; 2],
        message: "connection refused".into(),
    };
    assert_eq!(
        error.to_string(),
        "the transfer of chunk abab kept failing: connection refused"
    );
}

#[tokio::test]
async fn an_endless_receipt_or_error_is_not_read_past_a_small_limit() {
    let sent = chunk(b"payload");
    let transfer = transfer();
    let quickly = Duration::from_secs(5);

    let (target, server) = endless(
        200,
        "application/x-protobuf",
        vec![0; 16 << 10],
        Duration::ZERO,
    )
    .await;
    let put = tokio::time::timeout(
        quickly,
        transfer.put(&target, &sent, Bytes::from_static(b"payload")),
    )
    .await;
    assert!(
        matches!(
            put,
            Ok(Attempt::Fail(ChunkError::Refused {
                status: 200,
                error: None,
                ..
            }))
        ),
        "an oversized receipt is refused without reading it all"
    );
    server.abort();

    let (target, server) = endless(
        500,
        "application/x-protobuf",
        vec![0; 16 << 10],
        Duration::ZERO,
    )
    .await;
    let get = tokio::time::timeout(quickly, transfer.get(&target, &sent)).await;
    assert!(
        matches!(
            get,
            Ok(Attempt::Retry(ChunkError::Refused {
                status: 500,
                error: None,
                ..
            }))
        ),
        "an oversized error is dropped without reading it all"
    );
    server.abort();

    let (target, server) = endless(200, "text/plain", vec![0; 16 << 10], Duration::ZERO).await;
    let put = tokio::time::timeout(
        quickly,
        transfer.put(&target, &sent, Bytes::from_static(b"payload")),
    )
    .await;
    assert!(
        matches!(put, Ok(Attempt::Done(None))),
        "the body of a presigned answer is not read"
    );
    server.abort();
}

#[tokio::test]
async fn an_answer_that_trickles_is_cut_off_by_the_attempt_timeout() {
    let wanted = chunk(&[7; 1000]);
    let transfer = Transfer {
        attempt_timeout: Duration::from_millis(300),
        ..transfer()
    };
    let every = Duration::from_millis(50);

    let (target, server) = endless(200, "application/octet-stream", vec![7], every).await;
    let started = Instant::now();
    let get = transfer.get(&target, &wanted).await;
    assert!(matches!(
        get,
        Attempt::Retry(ChunkError::Unavailable { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    server.abort();

    let (target, server) = endless(200, "application/x-protobuf", vec![0], every).await;
    let started = Instant::now();
    let put = transfer
        .put(&target, &wanted, Bytes::from(vec![7; 1000]))
        .await;
    assert!(matches!(
        put,
        Attempt::Retry(ChunkError::Unavailable { .. })
    ));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    server.abort();
}

#[tokio::test]
async fn a_429_is_retried_after_the_wait_the_server_asks() {
    let sent = chunk(b"payload");
    let busy = ProtoError {
        code: Code::RetryLater as i32,
        retry_after_ms: 300,
        ..Default::default()
    };
    let (target, server) = serve(vec![
        (429, "application/x-protobuf", busy.encode_to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let transfer = transfer();
    assert!(transfer.backoff.max < Duration::from_millis(300));
    let started = Instant::now();
    let outcome = transfer
        .retrying(async || {
            transfer
                .put(&target, &sent, Bytes::from_static(b"payload"))
                .await
        })
        .await;
    assert!(matches!(outcome, Ok(Some(None))), "{outcome:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    server.await.unwrap();
}

#[tokio::test]
async fn a_target_inside_the_expiry_margin_is_still_tried() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let wanted = chunk(b"hello");
    let (target, server) = serve(vec![
        (200, "application/octet-stream", b"hello".to_vec()),
        (200, "application/xml", Vec::new()),
    ])
    .await;
    let target = Target {
        expires_at_ms: now + 30_000,
        ..target
    };
    assert!(expiring(&target));
    let transfer = transfer();
    assert!(
        matches!(transfer.get(&target, &wanted).await, Attempt::Done(bytes) if bytes == b"hello"[..])
    );
    assert!(matches!(
        transfer
            .put(&target, &wanted, Bytes::from_static(b"hello"))
            .await,
        Attempt::Done(None)
    ));
    server.await.unwrap();
}

/// An `UPLOAD` plan for `chunk` through the relay at `relay`, with a ticket that expires `lifetime` from now.
fn upload_plan(relay: SocketAddr, chunk: &ChunkRef, ticket: &str, lifetime: Duration) -> ChunkPlan {
    let expires = SystemTime::now() + lifetime;
    ChunkPlan {
        hash: chunk.hash.clone(),
        action: Action::Upload as i32,
        target: Some(Target {
            url: format!("http://{relay}/v1/spaces/s/chunks/x?ticket={ticket}"),
            expires_at_ms: expires.duration_since(UNIX_EPOCH).unwrap().as_millis() as i64,
            ..Default::default()
        }),
        retry_after_ms: 0,
    }
}

/// A relay that refuses the ticket `stale` with 403, as the Worker refuses an expired ticket, and answers any other
/// ticket with a receipt. Returns its address and the tickets it saw, in order.
async fn relay() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tickets = Arc::new(Mutex::new(Vec::new()));
    let seen = tickets.clone();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request(&mut socket).await;
            let (_, query) = head.split_once("ticket=").unwrap();
            let ticket = query.split(' ').next().unwrap().to_owned();
            let (status, answer) = if ticket == "stale" {
                (403, Vec::new())
            } else {
                let receipt = UploadReceipt {
                    hash: Sha256::digest(&body).to_vec(),
                    size: body.len() as u32,
                    token: ticket.clone(),
                };
                (200, receipt.encode_to_vec())
            };
            seen.lock().unwrap().push(ticket);
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/x-protobuf\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                answer.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            socket.write_all(&answer).await.unwrap();
        }
    });
    (address, tickets)
}

/// A session whose server answers its n-th `PrepareUpload` with `plans(n)`, and the chunks each call asked for.
async fn scripted_session(
    mut plans: impl FnMut(usize) -> Vec<ChunkPlan> + Send + 'static,
) -> (Session, Events, Arc<Mutex<Vec<Vec<ChunkRef>>>>) {
    let (listener, auth) = listen().await;
    let (session, events) = Session::start(auth, config()).unwrap();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let calls = asked.clone();
    tokio::spawn(async move {
        let mut ws = accept(&listener, welcome("h", 0)).await;
        loop {
            let request = recv_rpc(&mut ws).await;
            let Some(rpc_request::Call::PrepareUpload(prepare)) = request.call else {
                panic!("expected PrepareUpload, got {:?}", request.call);
            };
            let round = {
                let mut calls = calls.lock().unwrap();
                calls.push(prepare.chunks);
                calls.len() - 1
            };
            let result = rpc_response::Result::PrepareUpload(PrepareUploadResult {
                chunks: plans(round),
            });
            reply(&mut ws, request.req_id, result).await;
        }
    });
    (session, events, asked)
}

#[tokio::test]
async fn an_upload_target_that_expires_during_its_batch_is_prepared_again_before_the_chunk_is_read()
{
    let a = chunk(b"chunk a");
    let b = chunk(b"chunk b");
    let (relay, tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let (a, b) = (a.clone(), b.clone());
        move |round| match round {
            0 => vec![
                upload_plan(relay, &b, "b", Duration::from_secs(3600)),
                upload_plan(relay, &a, "stale", Duration::from_millis(61_000)),
            ],
            _ => vec![upload_plan(relay, &a, "fresh", Duration::from_secs(3600))],
        }
    })
    .await;

    let mut reads = Vec::new();
    let receipts = transfer()
        .upload(&session, &[b.clone(), a.clone()], |chunk| {
            reads.push(chunk.hash.clone());
            let (bytes, delay): (&'static [u8], _) = if chunk.hash == b.hash {
                (b"chunk b", Duration::from_millis(1500))
            } else {
                (b"chunk a", Duration::ZERO)
            };
            async move {
                tokio::time::sleep(delay).await;
                Ok(Bytes::from_static(bytes))
            }
        })
        .await
        .unwrap();

    assert_eq!(*tickets.lock().unwrap(), ["b", "fresh"]);
    assert_eq!(reads, [b.hash.clone(), a.hash.clone()]);
    assert_eq!(
        *asked.lock().unwrap(),
        [vec![b.clone(), a.clone()], vec![a.clone()]]
    );
    let receipts: Vec<_> = receipts
        .into_iter()
        .map(|receipt| (receipt.hash, receipt.token))
        .collect();
    assert_eq!(
        receipts,
        [(b.hash, "b".to_owned()), (a.hash, "fresh".to_owned())]
    );
}

#[tokio::test]
async fn an_upload_target_already_inside_the_expiry_margin_when_prepared_is_sent_as_given() {
    let a = chunk(b"chunk a");
    let (relay, tickets) = relay().await;
    let (session, _events, asked) = scripted_session({
        let a = a.clone();
        move |_| vec![upload_plan(relay, &a, "skewed", Duration::from_secs(30))]
    })
    .await;

    let mut reads = 0;
    let receipts = transfer()
        .upload(&session, std::slice::from_ref(&a), |_| {
            reads += 1;
            async { Ok(Bytes::from_static(b"chunk a")) }
        })
        .await
        .unwrap();

    assert_eq!(*asked.lock().unwrap(), [vec![a.clone()]]);
    assert_eq!(*tickets.lock().unwrap(), ["skewed"]);
    assert_eq!(reads, 1);
    let receipts: Vec<_> = receipts
        .into_iter()
        .map(|receipt| (receipt.hash, receipt.token))
        .collect();
    assert_eq!(receipts, [(a.hash, "skewed".to_owned())]);
}

#[tokio::test]
async fn refusals_follow_the_server_error_code_before_the_http_status() {
    let sent = chunk(b"payload");
    let error = |code: i32, retry_after_ms: u64| {
        ProtoError {
            code,
            retry_after_ms,
            ..Default::default()
        }
        .encode_to_vec()
    };
    let protobuf = "application/x-protobuf";
    let (target, server) = serve(vec![
        (503, protobuf, error(999, 0)),
        (429, protobuf, error(Code::QuotaExceeded as i32, 0)),
        (429, protobuf, error(Code::QuotaExceeded as i32, 100)),
        (503, protobuf, error(Code::Internal as i32, 0)),
        (500, "text/plain", Vec::new()),
        (
            500,
            protobuf,
            ProtoError {
                message: "no code".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        ),
        (500, protobuf, Vec::new()),
    ])
    .await;
    let transfer = transfer();
    let put = async || {
        transfer
            .put(&target, &sent, Bytes::from_static(b"payload"))
            .await
    };
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "an unknown code is a refusal"
    );
    assert!(
        matches!(put().await, Attempt::Fail(_)),
        "a quota without a wait is a refusal"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "a quota with a wait is retried"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "a known code keeps its status mapping"
    );
    assert!(
        matches!(put().await, Attempt::Retry(_)),
        "no error: the status decides"
    );
    assert!(
        matches!(transfer.get(&target, &sent).await, Attempt::Fail(_)),
        "UNSPECIFIED is a refusal"
    );
    assert!(
        matches!(
            transfer.get(&target, &sent).await,
            Attempt::Retry(ChunkError::Refused {
                status: 500,
                error: None,
                ..
            })
        ),
        "an empty body is no refusal: the status decides"
    );
    server.await.unwrap();
}
