// SPDX-License-Identifier: Apache-2.0

//! The one error every command path returns: what went wrong, the code an agent branches on, the exit status, and the
//! next command when there is a real one. Errors are classified where their cause is known: a refusal from the
//! server, an error of the driver, a parse error of the arguments.

use koidrive_cloudflare::{ApiError as CloudflareError, OauthError};
use koidrive_driver::{ConfigError, DriverError, PathsError, SET_UP_AGAIN};
use koidrive_net::{ApiError, Resend, ServerError};
use koidrive_proto::v1::{Error as ProtoError, error::Code as ServerCode};
use serde::{Deserialize, Serialize};

use crate::report::{Done, Next};

/// What kind of failure an error is. Each code has one exit status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// No more specific code applies.
    #[default]
    Failed,
    /// The arguments are invalid, or input the command needs is missing.
    Usage,
    /// `status --check` found a decision that needs a choice.
    DecisionRequired,
    /// Credentials are missing or refused, or a person must log in.
    AuthRequired,
    /// Sync is halted or stopped, and the command cannot go on until that is fixed.
    Halted,
    /// The server or the network is offline, unavailable or limits the rate for now.
    Unavailable,
    /// The decision answered no longer exists, or a request ID was used before with other arguments or for a pairing
    /// code that has expired.
    StaleDecision,
    /// A signal stopped the command, or an earlier run with the same request ID has no recorded outcome.
    Interrupted,
}

impl Code {
    pub fn exit(self) -> u8 {
        match self {
            Code::Failed => 1,
            Code::Usage => 2,
            Code::DecisionRequired => 3,
            Code::AuthRequired => 4,
            Code::Halted => 5,
            Code::Unavailable => 6,
            Code::StaleDecision => 7,
            Code::Interrupted => 130,
        }
    }

    /// Whether the same command may succeed later without a change.
    pub fn retryable(self) -> bool {
        self == Code::Unavailable
    }
}

#[derive(Debug)]
pub struct CliError {
    pub code: Code,
    pub message: String,
    pub next: Vec<Next>,
    /// What the command read before it failed, such as the spaces that answered when another one refused.
    pub partial: Option<Box<Done>>,
}

impl CliError {
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        CliError {
            code,
            message: message.into(),
            next: Vec::new(),
            partial: None,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(Code::Usage, message)
    }

    /// `message` with the code of `cause`, for a message that puts the cause in context.
    pub fn caused(message: impl Into<String>, cause: impl Into<CliError>) -> Self {
        CliError {
            message: message.into(),
            ..cause.into()
        }
    }

    /// The same error, its message put in `context`: "context: message".
    pub fn context(mut self, context: impl std::fmt::Display) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    /// The same error with `argv` as a next command.
    pub fn then(mut self, argv: &[&str]) -> Self {
        self.next.push(Next::of(argv));
        self
    }
}

impl From<String> for CliError {
    fn from(message: String) -> Self {
        CliError::new(Code::Failed, message)
    }
}

impl From<&str> for CliError {
    fn from(message: &str) -> Self {
        CliError::new(Code::Failed, message)
    }
}

impl CliError {
    /// The server refused this device's credential.
    fn refused_credential() -> Self {
        CliError::new(
            Code::AuthRequired,
            format!(
                "the server refused this device's credential, so another device revoked it or the credential is \
                 wrong. To use this device again, {SET_UP_AGAIN}."
            ),
        )
    }

    /// The `Error` result of an RPC: a refused credential needs a person, and a refusal the server allows sending again
    /// may pass.
    pub fn rpc_refusal(error: ProtoError) -> Self {
        let error = ServerError(error);
        if error.code() == ServerCode::Unauthenticated {
            return CliError::refused_credential();
        }
        let code = match error.resend() {
            Resend::Allowed => Code::Unavailable,
            Resend::Never | Resend::ByCode => Code::Failed,
        };
        CliError::new(code, format!("the server refused: {}", error.message()))
    }
}

impl From<ApiError> for CliError {
    /// A refusal of the server: refused credentials need a person, a transport failure or a refusal the server allows
    /// sending again may pass.
    fn from(error: ApiError) -> Self {
        if error.is_unauthenticated() {
            return CliError::refused_credential();
        }
        let busy = match error
            .server()
            .map(|server| (server.code(), server.resend()))
        {
            Some((ServerCode::RetryLater | ServerCode::QuotaExceeded, resend)) => {
                resend == Resend::Allowed
            }
            _ => matches!(error.status(), Some(429 | 502 | 503 | 504)),
        };
        let code = match error {
            ApiError::Transport(_) | ApiError::TooLarge { .. } => Code::Unavailable,
            _ if busy => Code::Unavailable,
            _ => Code::Failed,
        };
        CliError::new(code, error.to_string())
    }
}

impl From<CloudflareError> for CliError {
    /// A failed Cloudflare API call: no answer, a rate limit or a server error may pass, and a refusal of the login's
    /// access needs a person to log in again.
    fn from(error: CloudflareError) -> Self {
        let code = match &error {
            CloudflareError::Transport { .. } => Code::Unavailable,
            CloudflareError::Failed { status, .. } => match status {
                401 | 403 => Code::AuthRequired,
                429 | 500..=599 => Code::Unavailable,
                _ => Code::Failed,
            },
            CloudflareError::Unreadable { .. } | CloudflareError::Name { .. } => Code::Failed,
        };
        CliError::new(code, error.to_string())
    }
}

impl From<OauthError> for CliError {
    /// A failed Cloudflare login: a pasted address that is not the callback is a usage error, a refused or incomplete
    /// login needs a person, and an unreachable OAuth endpoint may pass.
    fn from(error: OauthError) -> Self {
        let code = match &error {
            OauthError::NotCallback | OauthError::StateMismatch | OauthError::MissingCode => {
                Code::Usage
            }
            OauthError::Denied { .. }
            | OauthError::TokenRefused { .. }
            | OauthError::MissingScopes { .. }
            | OauthError::Spent => Code::AuthRequired,
            OauthError::Transport(_) => Code::Unavailable,
            OauthError::Listen { .. }
            | OauthError::Accept(_)
            | OauthError::MalformedTokenResponse
            | OauthError::RevokeRefused { .. } => Code::Failed,
        };
        CliError::new(code, error.to_string())
    }
}

impl From<DriverError> for CliError {
    fn from(error: DriverError) -> Self {
        let message = error.to_string();
        match error {
            DriverError::Api(error) => CliError::from(error),
            DriverError::NotPaired => {
                CliError::new(Code::AuthRequired, message).then(&["koi", "setup"])
            }
            DriverError::NeedsConfirmation => CliError::new(Code::AuthRequired, message),
            DriverError::QuestionNotFound(_) | DriverError::AnsweredOtherwise { .. } => {
                CliError::new(
                    Code::StaleDecision,
                    format!("{message}; koi resolve lists the decisions waiting now"),
                )
                .then(&["koi", "resolve", "--json"])
            }
            DriverError::AnswerNotAllowed { .. } => {
                CliError::usage(message).then(&["koi", "resolve", "--json"])
            }
            DriverError::EmptyName
            | DriverError::EmptyUserName
            | DriverError::SpaceNotFound(_)
            | DriverError::SpaceAmbiguous { .. }
            | DriverError::FolderNotFound(_)
            | DriverError::NotAFolder(_)
            | DriverError::NotLinked(_)
            | DriverError::SpaceNotLinked(_)
            | DriverError::InvalidRestore(_)
            | DriverError::AlreadyPaired { .. }
            | DriverError::FolderHoldsProfile(_)
            | DriverError::FolderOverlaps(_)
            | DriverError::FolderOfOtherDevice { .. }
            | DriverError::SpaceAlreadyLinked(_)
            | DriverError::MarkerOfOtherSpace(_)
            | DriverError::UnsupportedFileSystem
            | DriverError::NothingToLift
            | DriverError::Paths(PathsError::BadSpaceId(_))
            | DriverError::Config(ConfigError::Invalid { .. }) => CliError::usage(message),
            _ => CliError::from(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn refused(status: u16, code: ServerCode) -> ApiError {
        ApiError::Server {
            status,
            error: ServerError(koidrive_proto::v1::Error {
                code: code as i32,
                message: "refused".into(),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn every_code_has_its_exit_status_and_only_unavailable_is_retryable() {
        let all = [
            (Code::Failed, 1, "failed"),
            (Code::Usage, 2, "usage"),
            (Code::DecisionRequired, 3, "decision_required"),
            (Code::AuthRequired, 4, "auth_required"),
            (Code::Halted, 5, "halted"),
            (Code::Unavailable, 6, "unavailable"),
            (Code::StaleDecision, 7, "stale_decision"),
            (Code::Interrupted, 130, "interrupted"),
        ];
        for (code, exit, name) in all {
            assert_eq!(code.exit(), exit, "{code:?}");
            assert_eq!(serde_json::to_value(code).unwrap(), name);
            assert_eq!(code.retryable(), code == Code::Unavailable, "{code:?}");
        }
    }

    #[test]
    fn server_refusals_are_classified_by_their_cause() {
        let classify = |error: ApiError| CliError::from(error).code;
        assert_eq!(
            classify(refused(401, ServerCode::Unauthenticated)),
            Code::AuthRequired
        );
        assert_eq!(
            classify(refused(429, ServerCode::Unspecified)),
            Code::Unavailable
        );
        assert_eq!(
            classify(ApiError::Status { status: 503 }),
            Code::Unavailable
        );
        assert_eq!(
            classify(refused(400, ServerCode::RetryLater)),
            Code::Unavailable
        );
        assert_eq!(
            classify(refused(403, ServerCode::QuotaExceeded)),
            Code::Failed
        );
        let mut waiting = refused(429, ServerCode::QuotaExceeded);
        if let ApiError::Server { error, .. } = &mut waiting {
            error.0.retry_after_ms = 1_000;
        }
        assert_eq!(classify(waiting), Code::Unavailable);
        assert_eq!(classify(ApiError::TooLarge { limit: 1 }), Code::Unavailable);
        assert_eq!(classify(ApiError::Status { status: 404 }), Code::Failed);
        assert_eq!(classify(ApiError::Decode("Status")), Code::Failed);
    }

    #[test]
    fn an_rpc_refusal_keeps_the_meaning_of_its_code() {
        let rpc = |code: ServerCode, retry_after_ms: u64| {
            CliError::rpc_refusal(ProtoError {
                code: code as i32,
                message: "refused".into(),
                retry_after_ms,
                ..Default::default()
            })
        };
        let revoked = rpc(ServerCode::Unauthenticated, 0);
        assert_eq!(revoked.code, Code::AuthRequired);
        assert!(revoked.message.contains(SET_UP_AGAIN), "{revoked:?}");
        assert_eq!(rpc(ServerCode::RetryLater, 0).code, Code::Unavailable);
        assert_eq!(
            rpc(ServerCode::QuotaExceeded, 1_000).code,
            Code::Unavailable
        );
        assert_eq!(rpc(ServerCode::QuotaExceeded, 0).code, Code::Failed);
        let other = rpc(ServerCode::NotFound, 0);
        assert_eq!(other.code, Code::Failed);
        assert_eq!(other.message, "the server refused: refused");
    }

    #[test]
    fn driver_errors_are_classified_and_name_the_real_next_command() {
        let stale = CliError::from(DriverError::QuestionNotFound("q1".into()));
        assert_eq!(stale.code, Code::StaleDecision);
        assert_eq!(stale.next, [Next::of(&["koi", "resolve", "--json"])]);
        let unpaired = CliError::from(DriverError::NotPaired);
        assert_eq!(unpaired.code, Code::AuthRequired);
        assert_eq!(unpaired.next, [Next::of(&["koi", "setup"])]);
        assert_eq!(
            CliError::from(DriverError::NotLinked(PathBuf::from("/x"))).code,
            Code::Usage
        );
        assert_eq!(
            CliError::from(DriverError::Api(ApiError::Status { status: 503 })).code,
            Code::Unavailable
        );
        let other = CliError::from(DriverError::Io {
            path: PathBuf::from("/x"),
            error: std::io::Error::other("the disk failed"),
        });
        assert_eq!(other.code, Code::Failed);
        assert!(other.next.is_empty());
    }

    #[test]
    fn argument_errors_are_usage_and_a_change_that_waits_for_the_owner_needs_auth() {
        let usage = [
            DriverError::FolderOverlaps(PathBuf::from("/home/ada/Docs")),
            DriverError::FolderHoldsProfile(PathBuf::from("/home/ada/.local/state/koidrive")),
            DriverError::FolderOfOtherDevice {
                device_id: "d2".into(),
                server: "https://files.example.com".into(),
            },
            DriverError::SpaceAlreadyLinked(PathBuf::from("/home/ada/Docs")),
            DriverError::MarkerOfOtherSpace("s2".into()),
            DriverError::UnsupportedFileSystem,
            DriverError::AlreadyPaired {
                device_id: "d1".into(),
            },
            DriverError::NothingToLift,
            DriverError::Paths(PathsError::BadSpaceId("../x".into())),
            DriverError::Config(ConfigError::Invalid {
                path: PathBuf::from("/home/ada/.config/koidrive/config.toml"),
                reason: "unknown key".into(),
            }),
        ];
        for error in usage {
            let shown = error.to_string();
            assert_eq!(CliError::from(error).code, Code::Usage, "{shown}");
        }
        assert_eq!(
            CliError::from(DriverError::NeedsConfirmation).code,
            Code::AuthRequired
        );
    }

    #[test]
    fn cloudflare_and_login_errors_are_classified_by_their_cause() {
        let transport = || reqwest::Client::new().get("not a url").build().unwrap_err();
        let failed = |status| CloudflareError::Failed {
            call: "list the account's Workers",
            status,
            errors: Vec::new(),
        };
        let classify = |error: CloudflareError| CliError::from(error).code;
        assert_eq!(
            classify(CloudflareError::Transport {
                call: "list the account's Workers",
                source: transport(),
            }),
            Code::Unavailable
        );
        assert_eq!(classify(failed(503)), Code::Unavailable);
        assert_eq!(classify(failed(429)), Code::Unavailable);
        assert_eq!(classify(failed(403)), Code::AuthRequired);
        assert_eq!(classify(failed(400)), Code::Failed);

        let login = |error: OauthError| CliError::from(error).code;
        assert_eq!(login(OauthError::NotCallback), Code::Usage);
        assert_eq!(login(OauthError::StateMismatch), Code::Usage);
        assert_eq!(
            login(OauthError::Denied {
                error: "access_denied".into(),
                description: None,
            }),
            Code::AuthRequired
        );
        assert_eq!(
            login(OauthError::TokenRefused {
                status: 400,
                error: "invalid_grant".into(),
                description: None,
            }),
            Code::AuthRequired
        );
        assert_eq!(login(OauthError::Transport(transport())), Code::Unavailable);

        let wrapped = CliError::caused(
            "cannot list the account's Workers: no answer; run the command again",
            failed(502),
        );
        assert_eq!(wrapped.code, Code::Unavailable);
        assert!(wrapped.message.ends_with("run the command again"));
        let prefixed = CliError::from(failed(401)).context("cannot read the bucket fb");
        assert_eq!(prefixed.code, Code::AuthRequired);
        assert!(
            prefixed
                .message
                .starts_with("cannot read the bucket fb: could not")
        );
    }
}
