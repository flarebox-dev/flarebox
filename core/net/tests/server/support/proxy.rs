// SPDX-License-Identifier: Apache-2.0

//! A TCP proxy in front of the Worker that can drop a connection at a chosen moment.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Fault {
    #[default]
    None,
    /// Drop the connection that next carries bytes from the client, before forwarding them.
    DropOnClientData,
    /// Drop the connection that next carries bytes from the server, before forwarding them.
    DropOnServerData,
}

pub struct Proxy {
    pub url: Url,
    fault: Arc<Mutex<Fault>>,
    websocket_client_bytes: Arc<AtomicU64>,
    corrupt: Arc<Mutex<Corrupt>>,
}

/// How many chunk bodies to damage on the way, by flipping one byte.
#[derive(Clone, Copy, Debug, Default)]
pub struct Corrupt {
    pub uploads: u32,
    pub downloads: u32,
}

#[derive(Clone)]
struct Shared {
    upstream: SocketAddr,
    fault: Arc<Mutex<Fault>>,
    websocket_client_bytes: Arc<AtomicU64>,
    corrupt: Arc<Mutex<Corrupt>>,
}

impl Proxy {
    pub async fn start(upstream: &Url) -> Proxy {
        let upstream: SocketAddr = upstream.socket_addrs(|| None).unwrap()[0];
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let shared = Shared {
            upstream,
            fault: Arc::default(),
            websocket_client_bytes: Arc::default(),
            corrupt: Arc::default(),
        };
        let proxy = Proxy {
            url,
            fault: shared.fault.clone(),
            websocket_client_bytes: shared.websocket_client_bytes.clone(),
            corrupt: shared.corrupt.clone(),
        };
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                tokio::spawn(relay(client, shared.clone()));
            }
        });
        proxy
    }

    pub fn arm(&self, fault: Fault) {
        *self.fault.lock().unwrap() = fault;
    }

    /// True once the armed fault has dropped a connection.
    pub fn fired(&self) -> bool {
        *self.fault.lock().unwrap() == Fault::None
    }

    /// Damages the next chunk bodies that pass: a relay upload (`PUT .../chunks/...`) or a chunk download
    /// (an `application/octet-stream` response).
    pub fn corrupt(&self, corrupt: Corrupt) {
        *self.corrupt.lock().unwrap() = corrupt;
    }

    /// Corruptions still armed.
    pub fn corrupt_left(&self) -> Corrupt {
        *self.corrupt.lock().unwrap()
    }

    /// Bytes sent by clients on WebSocket connections after the upgrade request.
    pub fn websocket_client_bytes(&self) -> u64 {
        self.websocket_client_bytes.load(Ordering::SeqCst)
    }
}

async fn relay(mut client: TcpStream, shared: Shared) {
    let Shared {
        upstream,
        fault,
        websocket_client_bytes,
        corrupt,
    } = shared;
    let Ok(mut server) = TcpStream::connect(upstream).await else {
        return;
    };
    let mut first = true;
    let mut websocket = false;
    let (mut damage_upload, mut damage_download) = (false, false);
    let (mut from_client, mut from_server) = ([0u8; 64 * 1024], [0u8; 64 * 1024]);
    let trips = |trigger: Fault| {
        let mut armed = fault.lock().unwrap();
        let fire = *armed == trigger;
        if fire {
            *armed = Fault::None;
        }
        fire
    };
    loop {
        tokio::select! {
            read = client.read(&mut from_client) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if first {
                        websocket = from_client[..n].windows(13).any(|w| w == b"/ws HTTP/1.1\r");
                        first = false;
                    } else if websocket {
                        websocket_client_bytes.fetch_add(n as u64, Ordering::SeqCst);
                    }
                    let data = &mut from_client[..n];
                    if data.starts_with(b"PUT ") && contains(data, b"/chunks/") {
                        damage_upload = take(&corrupt, |c| &mut c.uploads);
                    }
                    damage_upload = damage_upload && !flip_body(data);
                    if trips(Fault::DropOnClientData) || server.write_all(data).await.is_err() {
                        return;
                    }
                }
            },
            read = server.read(&mut from_server) => match read {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    let data = &mut from_server[..n];
                    if data.starts_with(b"HTTP/1.1 200") && contains(data, b"octet-stream") {
                        damage_download = take(&corrupt, |c| &mut c.downloads);
                    }
                    damage_download = damage_download && !flip_body(data);
                    if trips(Fault::DropOnServerData) || client.write_all(data).await.is_err() {
                        return;
                    }
                }
            },
        }
    }
}

fn contains(data: &[u8], needle: &[u8]) -> bool {
    data.windows(needle.len()).any(|w| w == needle)
}

/// Uses up one armed corruption of this kind, if any.
fn take(corrupt: &Mutex<Corrupt>, kind: impl FnOnce(&mut Corrupt) -> &mut u32) -> bool {
    let mut corrupt = corrupt.lock().unwrap();
    let left = kind(&mut corrupt);
    let armed = *left > 0;
    *left = left.saturating_sub(1);
    armed
}

/// Flips the first body byte in `data`: after the header end when the headers are here, else the first byte.
/// Returns whether a byte was flipped.
fn flip_body(data: &mut [u8]) -> bool {
    let start = match data.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(end) => end + 4,
        None if data.starts_with(b"PUT ") || data.starts_with(b"HTTP/") => return false,
        None => 0,
    };
    match data.get_mut(start) {
        Some(byte) => {
            *byte ^= 0xff;
            true
        }
        None => false,
    }
}
