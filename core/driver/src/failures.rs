// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::sync::Mutex;

use koidrive_net::{CallError, ChunkError, ServerError, StopReason};
use koidrive_proto::v1::error::Code;
pub use koidrive_telemetry::schema::Failure;
pub(crate) use koidrive_telemetry::schema::{Asked, Halted, Refusal, Transfer};

use crate::disk::Stop;
use crate::question::QuestionKind;

static FAILURES: Mutex<BTreeMap<Failure, u64>> = Mutex::new(BTreeMap::new());

/// How many times each kind failed in this process.
pub fn failures() -> BTreeMap<Failure, u64> {
    FAILURES
        .lock()
        .map(|failures| failures.clone())
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) fn count(failure: Failure) -> u64 {
    failures().get(&failure).copied().unwrap_or(0)
}

pub(crate) fn failed(failure: Failure) {
    if let Ok(mut failures) = FAILURES.lock() {
        *failures.entry(failure).or_default() += 1;
    }
}

pub(crate) fn refusal(error: &ServerError) -> Refusal {
    match error.code() {
        Code::Unauthenticated => Refusal::Unauthenticated,
        Code::Forbidden => Refusal::Forbidden,
        Code::InvalidArgument => Refusal::InvalidArgument,
        Code::MissingChunks => Refusal::MissingChunks,
        Code::LimitExceeded => Refusal::LimitExceeded,
        Code::QuotaExceeded => Refusal::QuotaExceeded,
        Code::ReadOnly => Refusal::ReadOnly,
        Code::RetryLater => Refusal::RetryLater,
        Code::UnsupportedVersion => Refusal::UnsupportedVersion,
        Code::Internal => Refusal::Internal,
        Code::HistoryMismatch => Refusal::HistoryMismatch,
        Code::StaleDeviceState => Refusal::StaleDeviceState,
        Code::HeadMoved => Refusal::HeadMoved,
        Code::AheadOfServer => Refusal::AheadOfServer,
        Code::NotFound => Refusal::NotFound,
        Code::ConfirmationRequired => Refusal::ConfirmationRequired,
        Code::Unspecified => Refusal::Unknown,
    }
}

pub(crate) fn call_failed(error: &CallError) {
    if let CallError::Server(refusal) = error {
        failed(Failure::Refused(self::refusal(refusal)));
    }
}

pub(crate) fn transfer_failed(error: &ChunkError, as_failure: fn(Transfer) -> Failure) {
    let why = match error {
        ChunkError::Call(CallError::Stopped(_)) => return,
        ChunkError::Call(call) => {
            call_failed(call);
            Transfer::Call
        }
        ChunkError::SourceChanged { .. } => Transfer::SourceChanged,
        ChunkError::Corrupt { .. } => Transfer::Corrupt,
        ChunkError::Refused { error, .. } => {
            if let Some(refusal) = error {
                failed(Failure::Refused(self::refusal(refusal)));
            }
            Transfer::Refused
        }
        ChunkError::Unavailable { .. } => Transfer::Unavailable,
        ChunkError::NoTarget { .. } => Transfer::NoTarget,
        ChunkError::Io { .. } => Transfer::Io,
    };
    failed(as_failure(why));
}

pub(crate) fn halted(stop: &Stop) -> Halted {
    match stop {
        Stop::Withdrawn(_) => Halted::Withdrawn,
        Stop::Root(_) => Halted::Root,
        Stop::NoReplace => Halted::NoReplace,
        Stop::StoreBehind { .. } => Halted::StoreBehind,
        Stop::DiskBehind { .. } => Halted::DiskBehind,
        Stop::OtherRoot => Halted::OtherRoot,
        Stop::Record { .. } => Halted::Record,
        Stop::Store(_) => Halted::Store,
        Stop::Io(_) => Halted::Io,
        Stop::Session(reason) => session_ended(reason),
        Stop::Recovery(_) => Halted::Recovery,
        Stop::Refused(_) => Halted::Refused,
        Stop::Protocol(_) => Halted::Protocol,
    }
}

pub(crate) fn session_ended(reason: &StopReason) -> Halted {
    match reason {
        StopReason::Closed => Halted::SessionClosed,
        StopReason::CredentialsRefused => Halted::CredentialsRefused,
        StopReason::DeviceRevoked => Halted::DeviceRevoked,
        StopReason::UnsupportedVersion(_) => Halted::UnsupportedVersion,
        StopReason::Forbidden => Halted::NotAMember,
        StopReason::NotFound => Halted::SpaceNotFound,
        StopReason::BadUrl(_) | StopReason::Tls(_) => Halted::SessionStart,
        StopReason::Refused(_) => Halted::ConnectionRefused,
    }
}

pub(crate) fn asked(kind: &QuestionKind) -> Asked {
    match kind {
        QuestionKind::MassDelete { .. } => Asked::MassDelete,
        QuestionKind::RemoteMassDelete { .. } => Asked::RemoteMassDelete,
        QuestionKind::AmbiguousDisappearance => Asked::AmbiguousDisappearance,
        QuestionKind::Recovery { .. } => Asked::Recovery,
        QuestionKind::Conflict { .. } => Asked::Conflict,
    }
}
