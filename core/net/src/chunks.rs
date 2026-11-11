// SPDX-License-Identifier: Apache-2.0

//! Chunk upload and download (spec v0 sections 8 and 10). Targets come from the server, through the Worker relay or
//! as presigned URLs, and are used as given: the client never builds a storage key. Every chunk is checked against
//! its SHA-256 and size before it is sent and after it is received.

use std::collections::{HashMap, HashSet};
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use koidrive_proto::v1::{ChunkRef, Target, UploadReceipt, chunk_plan::Action};
use prost::Message;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

use crate::api::Api;
use crate::backoff::Backoff;
use crate::error::{CallError, Resend, ServerError};
use crate::session::Session;

/// The most chunks one `PrepareUpload` or `GetDownloadTargets` takes (spec v0 section 13).
pub const MAX_CHUNKS_PER_CALL: usize = 1000;
/// A target this close to its expiry is fetched again before use.
const TARGET_MARGIN: Duration = Duration::from_secs(60);
/// The most bytes reserved up front for one downloaded chunk. A larger chunk's buffer grows as its bytes arrive, so a
/// size the server or another device claims does not reserve memory by itself.
const MAX_PREALLOCATION: usize = 4 << 20;
/// The most bytes read of an answer other than chunk bytes: a receipt or an error. Anything longer is not read.
const SMALL_ANSWER: usize = 64 << 10;

/// True when `bytes` are the chunk: their SHA-256 is its hash and their length its size.
pub fn matches(chunk: &ChunkRef, bytes: &[u8]) -> bool {
    bytes.len() as u64 == u64::from(chunk.size)
        && Sha256::digest(bytes).as_slice() == chunk.hash.as_slice()
}

fn hex(hash: &[u8]) -> String {
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error(transparent)]
    Call(#[from] CallError),
    /// The bytes read for an upload are not the chunk, so the file changed after it was chunked. Nothing was sent;
    /// the caller plans the commit again (RFC 0002 section 11).
    #[error("the bytes read for chunk {} do not match its hash and size", hex(.hash))]
    SourceChanged { hash: Vec<u8> },
    /// The bytes downloaded for a chunk did not match its hash and size on every attempt. None reached the caller.
    #[error("the bytes received for chunk {} do not match its hash and size", hex(.hash))]
    Corrupt { hash: Vec<u8> },
    #[error("the transfer of chunk {} was refused with HTTP {status}", hex(.hash))]
    Refused {
        hash: Vec<u8>,
        status: u16,
        error: Option<ServerError>,
    },
    #[error("the transfer of chunk {} kept failing: {message}", hex(.hash))]
    Unavailable { hash: Vec<u8>, message: String },
    #[error("the server gave no usable plan or target for chunk {}", hex(.hash))]
    NoTarget { hash: Vec<u8> },
    #[error("reading or writing chunk {}", hex(.hash))]
    Io {
        hash: Vec<u8>,
        #[source]
        source: io::Error,
    },
}

/// How one transfer attempt ended.
enum Attempt<T> {
    Done(T),
    /// The ticket or URL was refused or expired: ask the server for a new target.
    Expired,
    /// Worth trying again with the same target after a backoff.
    Retry(ChunkError),
    Fail(ChunkError),
}

/// Moves chunk bytes between the client and the targets the server hands out.
#[derive(Clone, Debug)]
pub struct Transfer {
    http: reqwest::Client,
    pub backoff: Backoff,
    /// Attempts per chunk, and rounds of asking the server again for chunks it deferred or whose target expired.
    pub attempts: u32,
    /// Bounds one attempt, from sending the request to the last byte of the answer.
    pub attempt_timeout: Duration,
}

impl Transfer {
    pub fn new(api: &Api) -> Self {
        Transfer {
            http: api.http().clone(),
            backoff: Backoff::default(),
            attempts: 5,
            attempt_timeout: Duration::from_secs(120),
        }
    }

    /// Uploads the chunks the server does not hold yet and returns their receipts for the commit. `read` gives a
    /// chunk's bytes; it is called only for chunks the server asks for. Chunks are handled in order, each distinct
    /// hash once. Presigned targets give no receipt; the server checks those chunks itself at commit time.
    ///
    /// The future `read` returns borrows neither the chunk nor `read` itself. That keeps this transfer `Send`, so a
    /// spawned task can run it.
    pub async fn upload<R: Future<Output = io::Result<Bytes>>>(
        &self,
        session: &Session,
        chunks: &[ChunkRef],
        mut read: impl FnMut(&ChunkRef) -> R,
    ) -> Result<Vec<UploadReceipt>, ChunkError> {
        let mut todo = distinct(chunks);
        let mut receipts = Vec::new();
        let mut round = 0;
        while !todo.is_empty() {
            let mut deferred = Vec::new();
            let mut wait = Duration::ZERO;
            for batch in todo.chunks(MAX_CHUNKS_PER_CALL) {
                let result = session.prepare_upload(batch.to_vec()).await?;
                let prepared = SystemTime::now();
                let plans: HashMap<_, _> = result
                    .chunks
                    .iter()
                    .map(|plan| (plan.hash.as_slice(), plan))
                    .collect();
                for chunk in batch {
                    let no_target = || ChunkError::NoTarget {
                        hash: chunk.hash.clone(),
                    };
                    let plan = plans.get(chunk.hash.as_slice()).ok_or_else(no_target)?;
                    match plan.action() {
                        Action::Present => {}
                        Action::RetryLater => {
                            wait = wait.max(Duration::from_millis(plan.retry_after_ms));
                            deferred.push(chunk.clone());
                        }
                        Action::Upload => {
                            let target = plan.target.as_ref().ok_or_else(no_target)?;
                            // A target that looks expiring on arrival means this clock runs ahead of the server's, so it
                            // is sent as given and the relay judges the ticket.
                            if expiring(target) && !expiring_at(target, prepared) {
                                deferred.push(chunk.clone());
                                continue;
                            }
                            let bytes = read(chunk).await.map_err(|source| ChunkError::Io {
                                hash: chunk.hash.clone(),
                                source,
                            })?;
                            if !matches(chunk, &bytes) {
                                return Err(ChunkError::SourceChanged {
                                    hash: chunk.hash.clone(),
                                });
                            }
                            match self
                                .retrying(|| self.put(target, chunk, bytes.clone()))
                                .await?
                            {
                                Some(Some(receipt)) => receipts.push(receipt),
                                Some(None) => {}
                                None => deferred.push(chunk.clone()),
                            }
                        }
                        Action::Unspecified => return Err(no_target()),
                    }
                }
            }
            if deferred.is_empty() {
                break;
            }
            if round >= self.attempts {
                let message =
                    "the server kept deferring the upload or refusing its ticket".to_owned();
                return Err(ChunkError::Unavailable {
                    hash: deferred[0].hash.clone(),
                    message,
                });
            }
            tokio::time::sleep(if wait.is_zero() {
                self.backoff.delay(round)
            } else {
                wait
            })
            .await;
            round += 1;
            todo = deferred;
        }
        Ok(receipts)
    }

    /// Downloads each distinct chunk once, checks it against its hash and size, and hands it to `write` in order.
    /// Bytes that do not match are fetched again and never passed on.
    ///
    /// As with [`Transfer::upload`], the future `write` returns borrows neither the chunk nor `write` itself.
    pub async fn download<W: Future<Output = io::Result<()>>>(
        &self,
        session: &Session,
        chunks: &[ChunkRef],
        mut write: impl FnMut(&ChunkRef, Bytes) -> W,
    ) -> Result<(), ChunkError> {
        for batch in distinct(chunks).chunks(MAX_CHUNKS_PER_CALL) {
            let hashes = batch.iter().map(|chunk| chunk.hash.clone()).collect();
            let mut targets: HashMap<Vec<u8>, Target> = session
                .get_download_targets(hashes)
                .await?
                .targets
                .into_iter()
                .filter_map(|t| Some((t.hash, t.target?)))
                .collect();
            for chunk in batch {
                let mut renewed = 0;
                let bytes = loop {
                    let target = match targets.get(&chunk.hash) {
                        Some(target) if !expiring(target) => target.clone(),
                        _ => self.download_target(session, chunk).await?,
                    };
                    match self.retrying(|| self.get(&target, chunk)).await? {
                        Some(bytes) => break bytes,
                        None if renewed < self.attempts => {
                            renewed += 1;
                            targets.remove(&chunk.hash);
                        }
                        None => {
                            let message = "the server kept refusing the download ticket".to_owned();
                            return Err(ChunkError::Unavailable {
                                hash: chunk.hash.clone(),
                                message,
                            });
                        }
                    }
                };
                write(chunk, bytes).await.map_err(|source| ChunkError::Io {
                    hash: chunk.hash.clone(),
                    source,
                })?;
            }
        }
        Ok(())
    }

    async fn download_target(
        &self,
        session: &Session,
        chunk: &ChunkRef,
    ) -> Result<Target, ChunkError> {
        let targets = session
            .get_download_targets(vec![chunk.hash.clone()])
            .await?;
        let found = targets
            .targets
            .into_iter()
            .find(|t| t.hash == chunk.hash)
            .and_then(|t| t.target);
        found.ok_or_else(|| ChunkError::NoTarget {
            hash: chunk.hash.clone(),
        })
    }

    /// Runs one transfer up to `attempts` times, waiting as long as the server asked or else with backoff. `None`
    /// means the target needs renewing.
    async fn retrying<T, F: Future<Output = Attempt<T>>>(
        &self,
        mut attempt: impl FnMut() -> F,
    ) -> Result<Option<T>, ChunkError> {
        let mut tries = 0;
        loop {
            match attempt().await {
                Attempt::Done(value) => return Ok(Some(value)),
                Attempt::Expired => return Ok(None),
                Attempt::Fail(error) => return Err(error),
                Attempt::Retry(error) => {
                    tries += 1;
                    if tries >= self.attempts {
                        return Err(error);
                    }
                    tracing::debug!(%error, "koidrive chunk transfer will be retried");
                    let asked = match &error {
                        ChunkError::Refused {
                            error: Some(server),
                            ..
                        } => server.retry_after(),
                        _ => None,
                    };
                    tokio::time::sleep(asked.unwrap_or_else(|| self.backoff.delay(tries - 1)))
                        .await;
                }
            }
        }
    }

    /// One upload within `attempt_timeout`. The relay answers with an `UploadReceipt`; a presigned URL answers
    /// without a body to keep.
    async fn put(
        &self,
        target: &Target,
        chunk: &ChunkRef,
        bytes: Bytes,
    ) -> Attempt<Option<UploadReceipt>> {
        let attempt = async {
            let response = match self.request(target, "PUT").body(bytes).send().await {
                Ok(response) => response,
                Err(error) => return Attempt::Retry(unavailable(chunk, error)),
            };
            let status = response.status();
            if status.is_success() && !protobuf(&response) {
                return Attempt::Done(None);
            }
            let body = if protobuf(&response) {
                match small_answer(response).await {
                    Ok(body) => body,
                    Err(error) => return Attempt::Retry(unavailable(chunk, error)),
                }
            } else {
                None
            };
            if status.is_success() {
                return match body.and_then(|body| UploadReceipt::decode(body.as_slice()).ok()) {
                    Some(receipt) if receipt.hash == chunk.hash && receipt.size == chunk.size => {
                        Attempt::Done(Some(receipt))
                    }
                    _ => Attempt::Fail(ChunkError::Refused {
                        hash: chunk.hash.clone(),
                        status: status.as_u16(),
                        error: None,
                    }),
                };
            }
            let refused = ChunkError::Refused {
                hash: chunk.hash.clone(),
                status: status.as_u16(),
                error: body.and_then(|body| ServerError::decode(&body)),
            };
            let refused = match by_code(refused) {
                Ok(attempt) => return attempt,
                Err(refused) => refused,
            };
            match status {
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Attempt::Expired,
                // The bytes were checked before sending, so a hash mismatch at the server happened on the way.
                StatusCode::BAD_REQUEST => Attempt::Retry(refused),
                status if retryable(status) => Attempt::Retry(refused),
                _ => Attempt::Fail(refused),
            }
        };
        self.bounded(chunk, attempt).await
    }

    /// One download within `attempt_timeout`, read up to the chunk's size and checked against its hash.
    async fn get(&self, target: &Target, chunk: &ChunkRef) -> Attempt<Bytes> {
        let attempt = async {
            let mut response = match self.request(target, "GET").send().await {
                Ok(response) => response,
                Err(error) => return Attempt::Retry(unavailable(chunk, error)),
            };
            let status = response.status();
            if !status.is_success() {
                let error = if protobuf(&response) {
                    small_answer(response)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|body| ServerError::decode(&body))
                } else {
                    None
                };
                let refused = ChunkError::Refused {
                    hash: chunk.hash.clone(),
                    status: status.as_u16(),
                    error,
                };
                let refused = match by_code(refused) {
                    Ok(attempt) => return attempt,
                    Err(refused) => refused,
                };
                return match status {
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Attempt::Expired,
                    status if retryable(status) => Attempt::Retry(refused),
                    _ => Attempt::Fail(refused),
                };
            }
            let corrupt = || {
                Attempt::Retry(ChunkError::Corrupt {
                    hash: chunk.hash.clone(),
                })
            };
            let limit = chunk.size as usize;
            let mut body = Vec::with_capacity(limit.min(MAX_PREALLOCATION));
            loop {
                match response.chunk().await {
                    Ok(Some(part)) if body.len() + part.len() <= limit => {
                        body.extend_from_slice(&part)
                    }
                    Ok(Some(_)) => return corrupt(),
                    Ok(None) => break,
                    Err(error) => return Attempt::Retry(unavailable(chunk, error)),
                }
            }
            if !matches(chunk, &body) {
                return corrupt();
            }
            Attempt::Done(Bytes::from(body))
        };
        self.bounded(chunk, attempt).await
    }

    /// Ends an attempt that has not finished within `attempt_timeout`, so a server that trickles its answer is retried.
    async fn bounded<T>(
        &self,
        chunk: &ChunkRef,
        attempt: impl Future<Output = Attempt<T>>,
    ) -> Attempt<T> {
        match tokio::time::timeout(self.attempt_timeout, attempt).await {
            Ok(attempt) => attempt,
            Err(_) => Attempt::Retry(ChunkError::Unavailable {
                hash: chunk.hash.clone(),
                message: format!("no complete answer within {:?}", self.attempt_timeout),
            }),
        }
    }

    fn request(&self, target: &Target, default_method: &str) -> reqwest::RequestBuilder {
        let method = if target.method.is_empty() {
            default_method
        } else {
            &target.method
        };
        let method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        let mut request = self.http.request(method, &target.url);
        for (name, value) in &target.headers {
            request = request.header(name, value);
        }
        request
    }
}

fn distinct(chunks: &[ChunkRef]) -> Vec<ChunkRef> {
    let mut seen = HashSet::new();
    chunks
        .iter()
        .filter(|chunk| seen.insert(chunk.hash.clone()))
        .cloned()
        .collect()
}

fn expiring(target: &Target) -> bool {
    expiring_at(target, SystemTime::now())
}

fn expiring_at(target: &Target, now: SystemTime) -> bool {
    if target.expires_at_ms <= 0 {
        return false;
    }
    let now = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    (now + TARGET_MARGIN).as_millis() >= target.expires_at_ms as u128
}

/// Applies H1 when the server sent an error: an unknown code, or `QUOTA_EXCEEDED` without a wait, fails the
/// transfer, and `RETRY_LATER` or `QUOTA_EXCEEDED` with a wait retries it. Any other refusal is given back for its
/// HTTP status to decide.
fn by_code<T>(refused: ChunkError) -> Result<Attempt<T>, ChunkError> {
    let ChunkError::Refused {
        error: Some(error), ..
    } = &refused
    else {
        return Err(refused);
    };
    match error.resend() {
        Resend::Never => Ok(Attempt::Fail(refused)),
        Resend::Allowed => Ok(Attempt::Retry(refused)),
        Resend::ByCode => Err(refused),
    }
}

fn retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

/// Reads a small answer, a receipt or an error, or gives `None` once it passes `SMALL_ANSWER`.
async fn small_answer(mut response: reqwest::Response) -> Result<Option<Vec<u8>>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(part) = response.chunk().await? {
        if body.len() + part.len() > SMALL_ANSWER {
            return Ok(None);
        }
        body.extend_from_slice(&part);
    }
    Ok(Some(body))
}

fn protobuf(response: &reqwest::Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/x-protobuf"))
}

/// A transport error, without the URL: chunk URLs carry tickets.
fn unavailable(chunk: &ChunkRef, error: reqwest::Error) -> ChunkError {
    let message = error.without_url().to_string();
    ChunkError::Unavailable {
        hash: chunk.hash.clone(),
        message,
    }
}

#[cfg(test)]
mod tests;
