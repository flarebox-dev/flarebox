// SPDX-License-Identifier: Apache-2.0

use std::io;
use std::sync::Arc;

use bytes::Bytes;
use koidrive_net::{ChunkError, Session, Transfer};
use koidrive_proto::v1::{ChunkRef, UploadReceipt};

async fn upload(
    session: Arc<Session>,
    transfer: Transfer,
    chunks: Vec<ChunkRef>,
) -> Result<Vec<UploadReceipt>, ChunkError> {
    let source = Arc::new(Vec::<u8>::new());
    let read = |chunk: &ChunkRef| {
        let (source, size) = (source.clone(), chunk.size as usize);
        async move {
            tokio::task::spawn_blocking(move || Ok(Bytes::copy_from_slice(&source[..size])))
                .await
                .map_err(io::Error::other)?
        }
    };
    transfer.upload(&session, &chunks, read).await
}

async fn download(
    session: Arc<Session>,
    transfer: Transfer,
    chunks: Vec<ChunkRef>,
) -> Result<Vec<Bytes>, ChunkError> {
    let mut received = Vec::new();
    let write = |_: &ChunkRef, bytes: Bytes| {
        received.push(bytes);
        std::future::ready(Ok(()))
    };
    transfer.download(&session, &chunks, write).await?;
    Ok(received)
}

fn spawnable<A, B, C, F: Future + Send + 'static>(_: fn(A, B, C) -> F) {}

#[test]
fn transfer_futures_are_send() {
    spawnable(upload);
    spawnable(download);
}
