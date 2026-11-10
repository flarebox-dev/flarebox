// SPDX-License-Identifier: Apache-2.0

//! A local HTTP server for tests that answers each request with the first unused reply for its method and path, and
//! records every request.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use url::Url;

pub struct Reply {
    pub method: &'static str,
    pub path: String,
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

/// A request the fake received: method, path with query, and its `cf-r2-jurisdiction` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub jurisdiction: Option<String>,
}

pub struct Fake {
    pub base: Url,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub async fn start(replies: Vec<Reply>) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let replies = Arc::new(Mutex::new(replies));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = seen.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let (replies, seen) = (replies.clone(), recorder.clone());
                let service =
                    service_fn(move |request| answer(request, replies.clone(), seen.clone()));
                tokio::spawn(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
            }
        });
        Fake { base, seen }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// Whether any request used one of `methods`.
    pub fn sent(&self, methods: &[&str]) -> bool {
        self.seen()
            .iter()
            .any(|seen| methods.contains(&seen.method.as_str()))
    }
}

async fn answer(
    request: Request<Incoming>,
    replies: Arc<Mutex<Vec<Reply>>>,
    seen: Arc<Mutex<Vec<Seen>>>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = request.method().to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|path| path.as_str().to_owned())
        .unwrap_or_default();
    let jurisdiction = request
        .headers()
        .get("cf-r2-jurisdiction")
        .map(|value| value.to_str().unwrap().to_owned());
    seen.lock().unwrap().push(Seen {
        method: method.clone(),
        path: path.clone(),
        jurisdiction,
    });
    let mut replies = replies.lock().unwrap();
    let reply = match replies
        .iter()
        .position(|reply| reply.method == method && reply.path == path)
    {
        Some(index) => replies.remove(index),
        None => Reply {
            method: "",
            path,
            status: 500,
            content_type: "text/plain",
            body: format!("the fake has no reply for {method}").into_bytes(),
        },
    };
    Ok(Response::builder()
        .status(reply.status)
        .header("content-type", reply.content_type)
        .body(Full::new(Bytes::from(reply.body)))
        .unwrap())
}
