// SPDX-License-Identifier: Apache-2.0

//! Network client for a Flarebox server, the client side of spec v0 and RFC 0001 to 0003. It does I/O only and has
//! no dependency on the sync engine: requests and events are `flarebox-proto` types, and a driver feeds them to the
//! engine and stores what must survive a restart (credentials, the stream position, the commit slot).
//!
//! - [`Api`] calls the HTTP endpoints of spec v0 section 15.1, and [`Auth`] keeps a device's access token fresh.
//! - [`Session`] is the WebSocket session with one space: RPCs, the change stream as [`Event`]s, `Reauth`, and
//!   reconnects with backoff that resend only what RFC 0002 section 2 allows.
//!
//! TLS is rustls with the platform verifier, for HTTP (through reqwest) and for the WebSocket alike.

mod api;
mod auth;
mod backoff;
mod error;
mod event;
mod session;
mod stream;

pub use api::Api;
pub use auth::{Auth, Credentials, DEFAULT_REFRESH_MARGIN};
pub use backoff::Backoff;
pub use error::{ApiError, CallError, NotResent, ServerError, StopReason};
pub use event::{Committed, DisconnectReason, Event};
pub use session::{
    Events, MAX_CLIENT_MESSAGE, MAX_SERVER_MESSAGE, PROTO_MAX, PROTO_MIN, Session, SessionConfig,
};
