// SPDX-License-Identifier: Apache-2.0

//! One-request HTTP servers that answer chunk transfers in scripted ways.

use koidrive_proto::v1::Error as ProtoError;
use koidrive_proto::v1::error::Code;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

use super::*;

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
            bodies.push(read_request(&mut socket).await);
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

/// Reads one request and returns its body.
async fn read_request(socket: &mut TcpStream) -> Vec<u8> {
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
    request[head_end..].to_vec()
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

/// Accepts one upload and returns its target, the socket and the bytes of the body already read with the head, and
/// the body's length.
async fn accept_upload() -> (Target, tokio::task::JoinHandle<(TcpStream, usize, usize)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/chunk", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        let mut buffer = [0u8; 4096];
        loop {
            let n = socket.read(&mut buffer).await.unwrap();
            head.extend_from_slice(&buffer[..n]);
            if let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") {
                let text = String::from_utf8_lossy(&head[..end]).to_lowercase();
                let length = text
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse::<usize>().unwrap());
                return (socket, head.len() - end - 4, length);
            }
        }
    });
    let target = Target {
        url,
        ..Default::default()
    };
    (target, server)
}

fn outcome<T>(attempt: &Attempt<T>) -> String {
    match attempt {
        Attempt::Done(_) => "done".into(),
        Attempt::Expired => "expired".into(),
        Attempt::Retry(error) => format!("retry: {error}"),
        Attempt::Fail(error) => format!("fail: {error}"),
    }
}

#[tokio::test]
async fn an_upload_that_takes_longer_than_the_read_timeout_completes_while_it_moves() {
    let body = vec![7; 3_500_000];
    let sent = chunk(&body);
    let (target, server) = accept_upload().await;
    let server = tokio::spawn(async move {
        let (mut socket, mut received, length) = server.await.unwrap();
        let mut buffer = [0u8; 10_000];
        while received < length {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let n = socket.read(&mut buffer).await.unwrap();
            assert!(n > 0, "the client left after {received} bytes");
            received += n;
        }
        socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await
            .unwrap();
        received
    });
    let started = Instant::now();
    let put = transfer().put(&target, &sent, Bytes::from(body)).await;
    assert!(matches!(put, Attempt::Done(None)), "{}", outcome(&put));
    assert_eq!(server.await.unwrap(), 3_500_000);
    assert!(
        started.elapsed() > Duration::from_secs(30),
        "the body is read at 0.8 Mbit/s, so it takes longer than the client's 30 s read timeout: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_upload_whose_body_stops_moving_is_retried_after_the_stall_timeout() {
    let body = vec![7; 32 << 20];
    let sent = chunk(&body);
    let transfer = Transfer {
        stall_timeout: Duration::from_millis(300),
        ..transfer()
    };
    let (target, server) = accept_upload().await;
    let started = Instant::now();
    let put = transfer.put(&target, &sent, Bytes::from(body)).await;
    assert!(
        matches!(&put, Attempt::Retry(ChunkError::Unavailable { message, .. }) if message.contains("did not move")),
        "{}",
        outcome(&put)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    server.abort();
}
