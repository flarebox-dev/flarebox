// SPDX-License-Identifier: Apache-2.0

//! `koi setup` and `koi server ...`: log in with Cloudflare, then deploy a server to the user's account, add
//! this device to the server there, upgrade it or delete it (docs/design/cli-setup.md). The Cloudflare grant lives in
//! memory for one command and is revoked when the command ends, also on failure, Ctrl-C and SIGTERM (section 7).
//!
//! Nothing here asks a question unless [`mode`] says the command is guided: stdin and stderr are terminals, and
//! neither `--json` nor the `CI` variable rules it out, or `--interactive` asks for it.

#[cfg(test)]
mod bundle_files;
mod claim;
#[cfg(test)]
pub(crate) mod fake;
mod state;
mod uninstall;

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use koidrive_cloudflare::{
    Account, Bundle, CLIENT_ID, Cloudflare, Endpoints, Grant, Jurisdiction, Login, Loopback,
    Script, Secret, UploadPlan,
};
use koidrive_driver::Paths;
use koidrive_net::{Api, ApiError};
use koidrive_proto::v1::ServerInfo;
use tokio::signal::unix::{SignalKind, signal};
use url::Url;

use koidrive_telemetry::schema::{SetupFlow, SetupStep};

use crate::DeviceArgs;
use crate::error::{CliError, Code};
use crate::ipc::{self, Request, Response};
use crate::printable;
use crate::report::{Choice, ChoicesData, Done, Next};
use claim::Claim;
use state::{SetupState, StateFile, Step};

const API_BASE: &str = "https://api.cloudflare.com/client/v4/";
/// Marks the Worker scripts `koi setup` uploads (section 6).
const TAG: &str = "koidrive-server";
/// The Durable Object classes that identify an untagged Koidrive Worker, such as a renamed button deploy.
const SERVER_CLASSES: [&str; 2] = ["AccountDO", "SpaceDO"];
/// Cloudflare's error code for an account without an R2 subscription (section 4.1).
const R2_DISABLED: u64 = 10042;
const R2_WAIT: Duration = Duration::from_secs(30 * 60);
const LOGIN_WAIT: Duration = Duration::from_secs(10 * 60);
/// Refresh the access token when it has less than this left, before a step that may take long.
const TOKEN_MARGIN: Duration = Duration::from_secs(5 * 60);
/// How long a new or upgraded Worker may take to answer, and to report its new version.
const SERVE_WAIT: Duration = Duration::from_secs(90);

include!(concat!(env!("OUT_DIR"), "/server_bundle.rs"));

/// Whether this command may ask questions on the terminal; [`mode`] decides it once, before anything is asked.
static GUIDED: AtomicBool = AtomicBool::new(false);

/// Whether a Cloudflare login session is live. Its own signal handling revokes the grant before the command ends,
/// so main's Ctrl-C handling stands aside while it is.
static SESSION: AtomicBool = AtomicBool::new(false);

pub fn in_session() -> bool {
    SESSION.load(Ordering::Relaxed)
}

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Ask questions on the terminal, as the guided setup does. Needs a terminal on stdin and stderr, and cannot be
    /// used with --json.
    #[arg(long)]
    interactive: bool,
    /// Print the login page instead of opening a browser, and read the address the browser ends on from the terminal.
    /// Needs --interactive.
    #[arg(long)]
    no_browser: bool,
    /// The local port for the login callback, for example to forward it over SSH. Defaults to a free port.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(clap::Subcommand)]
pub enum ServerAction {
    /// Log in with Cloudflare, deploy a new server and claim it with this device.
    Deploy {
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare and add this device to the server in the account, without a pairing code.
    AddDevice {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Claim a server deployed without koi setup, with the one-time claim code that its /setup page writes to the
    /// Worker's log. This device becomes the owner's first device.
    Claim {
        /// The server's address, such as https://koidrive.alice.workers.dev.
        #[arg(value_name = "SERVER_URL")]
        server: String,
        /// The claim code from the Worker's log.
        code: String,
        /// Your name, which the server keeps for its owner, at most 255 bytes.
        #[arg(long, value_parser = crate::short_name)]
        user: String,
        #[command(flatten)]
        device: DeviceArgs,
    },
    /// Log in with Cloudflare and upload this CLI's server version. Without a terminal it needs --worker and --yes.
    Upgrade {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        /// Go on without asking, for example past the Workers Builds warning.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare, delete the server's Worker, then empty and delete its bucket: every file, trashed file
    /// and version on the server is lost. Without a terminal it needs --worker or --bucket, and --yes.
    Delete {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        worker: Option<String>,
        /// Empty and delete this bucket when its Worker is already gone.
        #[arg(long)]
        bucket: Option<String>,
        /// The jurisdiction of --bucket: default, eu, us, fedramp or fedramp-high.
        #[arg(long, requires = "bucket", value_parser = jurisdiction, default_value = "default")]
        jurisdiction: Jurisdiction,
        /// Delete without asking.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        login: LoginArgs,
    },
}

/// How a setup or server command runs: guided, asking questions on the terminal, or direct, never asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Guided,
    Direct,
}

/// The mode of a setup or server command. It is guided only with a terminal on both stdin and stderr, and then when
/// `--interactive` asks for it, or when neither `--json` nor the `CI` variable rules it out. `--interactive` without
/// those terminals is a usage error, since nobody could answer.
pub fn mode(
    stdin_tty: bool,
    stderr_tty: bool,
    json: bool,
    ci: bool,
    interactive: bool,
) -> Result<Mode, CliError> {
    let terminal = stdin_tty && stderr_tty;
    if interactive && json {
        return Err(CliError::usage("--interactive cannot be used with --json"));
    }
    if interactive && !terminal {
        return Err(CliError::usage(
            "--interactive asks questions, so it needs a terminal on stdin and stderr",
        ));
    }
    match interactive || (terminal && !json && !ci) {
        true => Ok(Mode::Guided),
        false => Ok(Mode::Direct),
    }
}

/// [`mode`] for this process, which it also records for [`prompt`] and [`confirm`].
fn this_mode(json: bool, interactive: bool) -> Result<Mode, CliError> {
    let mode = mode(
        std::io::stdin().is_terminal(),
        std::io::stderr().is_terminal(),
        json,
        std::env::var_os("CI").is_some(),
        interactive,
    )?;
    GUIDED.store(mode == Mode::Guided, Ordering::Relaxed);
    Ok(mode)
}

/// Whether `koi setup` guides on this process, which main asks before the command runs so that the usage-report
/// question comes first. It records the mode for [`prompt`] as [`this_mode`] does.
pub fn guided(json: bool, login: &LoginArgs) -> bool {
    this_mode(json, login.interactive).is_ok_and(|mode| mode == Mode::Guided)
}

/// The ways to set up this machine, which `koi setup` lists when it does not guide.
fn choices() -> Vec<Choice> {
    let choice = |argv: &[&str], description| Choice {
        argv: Next::of(argv).argv,
        description,
    };
    vec![
        choice(
            &["koi", "join", "<CODE>"],
            "Join a server with a pairing code from a device that is already paired",
        ),
        choice(
            &["koi", "server", "deploy", "--interactive"],
            "Log in with Cloudflare and deploy a server to your account",
        ),
        choice(
            &["koi", "server", "add-device", "--interactive"],
            "Log in with Cloudflare and add this device to the server there, without a pairing code",
        ),
        choice(
            &[
                "koi",
                "server",
                "claim",
                "<SERVER_URL>",
                "<CODE>",
                "--user",
                "<NAME>",
            ],
            "Claim a server deployed without koi, with the code in its Worker's log",
        ),
        choice(
            &["koi", "setup", "--interactive"],
            "Choose in the guided setup, on a terminal",
        ),
    ]
}

/// `koi setup`: on a terminal, asks whether to join with a code or log in with Cloudflare. Otherwise it lists the
/// commands that set up this machine, and changes nothing.
pub async fn setup(
    paths: &Paths,
    device_name: &str,
    login: &LoginArgs,
    json: bool,
) -> Result<Done, CliError> {
    let mode = this_mode(json, login.interactive)?;
    if login.no_browser && !login.interactive {
        return Err(CliError::new(
            Code::AuthRequired,
            "--no-browser reads the login address from the terminal, so it needs --interactive",
        )
        .then(&["koi", "setup", "--interactive", "--no-browser"]));
    }
    not_set_up(paths)?;
    if mode == Mode::Direct {
        let choices = choices();
        let mut text = "Set up Koidrive on this machine with one of these commands:\n".to_owned();
        for choice in &choices {
            text.push_str(&format!(
                "  {}\n      {}\n",
                choice.argv.join(" "),
                choice.description
            ));
        }
        let next = (choices.iter())
            .map(|choice| Next {
                argv: choice.argv.clone(),
            })
            .collect();
        let mut done = Done::ok(ChoicesData { choices }, text);
        done.next = next;
        return Ok(done);
    }
    println!("How do you want to set up Koidrive on this machine?");
    println!(
        "  1. Log in with Cloudflare: deploy a server to your account, or add this device to the server there"
    );
    println!("  2. Enter a pairing code from another device");
    match prompt("Choose 1 or 2: ").await?.as_str() {
        "1" => {
            crate::telemetry::set_flow(SetupFlow::Cloudflare);
            bundled()?;
            with_session(login, async |session| {
                session.setup(paths, device_name).await
            })
            .await?;
        }
        "2" => {
            crate::telemetry::set_flow(SetupFlow::Pairing);
            let code = prompt("Pairing code: ").await?;
            let pair = async {
                let invitation = crate::pairing::parse(&code, None).map_err(CliError::usage)?;
                let request = Request::Pair {
                    server: invitation.server,
                    code: invitation.code,
                    name: device_name.to_owned(),
                };
                report_paired(paths, ipc::run(paths, request).await?)
            };
            crate::telemetry::step(SetupStep::Pair, pair).await?;
        }
        other => return Err(CliError::usage(format!("{other:?} is not 1 or 2"))),
    }
    Ok(Done::printed())
}

impl ServerAction {
    fn login(&self) -> Option<&LoginArgs> {
        match self {
            ServerAction::Deploy { login, .. }
            | ServerAction::AddDevice { login, .. }
            | ServerAction::Upgrade { login, .. }
            | ServerAction::Delete { login, .. } => Some(login),
            ServerAction::Claim { .. } => None,
        }
    }

    /// The same command with `--interactive`, for a person to run on a terminal. It keeps every argument that picks
    /// what the command acts on, so that it reaches the same Worker, bucket and jurisdiction, and drops `--yes`, so
    /// that the person is asked.
    fn interactive_argv(&self) -> Vec<String> {
        let (name, worker, bucket, jurisdiction, device) = match self {
            ServerAction::Deploy { device, .. } => ("deploy", None, None, None, Some(device)),
            ServerAction::AddDevice { worker, device, .. } => {
                ("add-device", worker.as_deref(), None, None, Some(device))
            }
            ServerAction::Upgrade { worker, .. } => {
                ("upgrade", worker.as_deref(), None, None, None)
            }
            ServerAction::Delete {
                worker,
                bucket,
                jurisdiction,
                ..
            } => (
                "delete",
                worker.as_deref(),
                bucket.as_deref(),
                Some(jurisdiction.as_str()).filter(|name| *name != "default"),
                None,
            ),
            ServerAction::Claim { .. } => ("claim", None, None, None, None),
        };
        let mut argv = Next::of(&["koi", "server", name]).argv;
        let port = self
            .login()
            .and_then(|login| login.port)
            .map(|port| port.to_string());
        let named = [
            ("--worker", worker),
            ("--bucket", bucket),
            ("--jurisdiction", jurisdiction),
            ("--name", device.and_then(|device| device.name.as_deref())),
        ];
        for (flag, value) in named {
            if let Some(value) = value {
                argv.extend([flag.to_owned(), value.to_owned()]);
            }
        }
        argv.push("--interactive".to_owned());
        if self.login().is_some_and(|login| login.no_browser) {
            argv.push("--no-browser".to_owned());
        }
        if let Some(port) = port {
            argv.extend(["--port".to_owned(), port]);
        }
        argv
    }
}

/// `koi server <action>`. Each logs in with Cloudflare in a browser, so it runs only with `--interactive` and a
/// terminal on stdin and stderr. Otherwise, with or without `--json`, it stops before the login and names the
/// `--interactive` command for a person to run.
pub async fn server(paths: &Paths, action: ServerAction, json: bool) -> Result<Done, CliError> {
    let login = action.login().expect("main sends a claim to the driver");
    if login.interactive && json {
        return Err(CliError::usage("--interactive cannot be used with --json"));
    }
    let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !(login.interactive && terminal) {
        let argv = action.interactive_argv();
        let words: Vec<&str> = argv.iter().map(String::as_str).collect();
        let message = format!(
            "this command logs in with Cloudflare in a browser; a person runs {} on a terminal",
            argv.join(" ")
        );
        return Err(CliError::new(Code::AuthRequired, message).then(&words));
    }
    GUIDED.store(true, Ordering::Relaxed);
    match action {
        ServerAction::Deploy { device, login } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, async |session| {
                session.server_deploy(paths, &name).await
            })
            .await?;
        }
        ServerAction::AddDevice {
            worker,
            device,
            login,
        } => {
            not_set_up(paths)?;
            bundled()?;
            let name = device.name();
            with_session(&login, async |session| {
                session
                    .server_add_device(paths, worker.as_deref(), &name)
                    .await
            })
            .await?;
        }
        ServerAction::Claim { .. } => unreachable!("main sends a claim to the driver"),
        ServerAction::Upgrade { worker, yes, login } => {
            bundled()?;
            with_session(&login, async |session| {
                let found = session.pick(worker.as_deref()).await?;
                crate::telemetry::step(SetupStep::Upgrade, session.upgrade(&found, yes)).await
            })
            .await?;
        }
        ServerAction::Delete {
            worker,
            bucket,
            jurisdiction,
            yes,
            login,
        } => {
            let resuming = StateFile::new(paths)
                .load()?
                .is_some_and(|state| state.step == Step::Uninstalling);
            if bucket.is_none() && !resuming {
                bundled()?;
            }
            with_session(&login, async |session| {
                let uninstall = session.uninstall(
                    paths,
                    worker.as_deref(),
                    bucket.as_deref(),
                    jurisdiction,
                    yes,
                );
                crate::telemetry::step(SetupStep::Remove, uninstall).await
            })
            .await?;
        }
    }
    Ok(Done::printed())
}

fn jurisdiction(text: &str) -> Result<Jurisdiction, String> {
    [
        Jurisdiction::Default,
        Jurisdiction::Eu,
        Jurisdiction::Us,
        Jurisdiction::Fedramp,
        Jurisdiction::FedrampHigh,
    ]
    .into_iter()
    .find(|jurisdiction| jurisdiction.as_str() == text)
    .ok_or_else(|| "not one of default, eu, us, fedramp and fedramp-high".to_owned())
}

/// Refuses a machine that is already set up, unless a run that stopped halfway left its state file: that run checks
/// the saved credential before it claims (section 4.6).
fn not_set_up(paths: &Paths) -> Result<(), CliError> {
    if paths.credential().exists() && !StateFile::new(paths).path().exists() {
        return Err(format!(
            "this device is already set up; its credential is {}",
            paths.credential().display()
        )
        .into());
    }
    Ok(())
}

/// Why a new deploy or claim must not replace `state`, the state file of a run in `account` or another one that has
/// not finished: the file is that run's only record of what it created.
fn unfinished(state: &SetupState, account: &str, file: &Path) -> CliError {
    let run = if state.account != account {
        format!(
            "a setup of {} in another Cloudflare account ({}) has not finished; run koi setup and log in to that account to finish it",
            state.script, state.account
        )
    } else if state.step == Step::Uninstalling {
        format!(
            "the delete of {} has not finished; run koi server delete to finish it",
            state.script
        )
    } else {
        format!(
            "the deploy of {} has not finished; run koi setup to finish it",
            state.script
        )
    };
    format!("{run}, or delete {} to forget it", file.display()).into()
}

fn report_paired(paths: &Paths, response: Response) -> Result<(), CliError> {
    match response {
        Response::Paired { account } => {
            println!(
                "This device is set up as {} ({}) with {}.\nThe credential is in {}.",
                printable(&account.device_name),
                printable(&account.device_id),
                printable(&account.server),
                paths.credential().display()
            );
            Ok(())
        }
        other => Err(format!("the daemon answered {other:?}").into()),
    }
}

/// Logs in, runs `work`, and revokes the grant however `work` ends: success, failure, Ctrl-C or SIGTERM. After a
/// signal it exits at once with 130 for Ctrl-C or 143 for SIGTERM, as a shell does for a command the signal ended.
async fn with_session(
    args: &LoginArgs,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let mut status = None;
    let stopped = async || {
        let (how, code) = tokio::select! {
            _ = terminate.recv() => ("stopped by SIGTERM", 143),
            _ = interrupt.recv() => ("stopped", 130),
        };
        status = Some(code);
        how
    };
    SESSION.store(true, Ordering::Relaxed);
    let login = crate::telemetry::step(SetupStep::Login, Session::login(args));
    let outcome = guarded(login, stopped, work).await;
    SESSION.store(false, Ordering::Relaxed);
    if let (Err(error), Some(code)) = (&outcome, status) {
        crate::telemetry::flush();
        eprintln!("koi: {}", printable(&error.message));
        std::process::exit(code);
    }
    outcome
}

/// Runs `work` on the session `login` returns, then revokes its grant. The grant exists once `login` returns, so
/// everything after that, the account lookup included, ends with the revocation, also when `stopped` fires.
async fn guarded(
    login: impl Future<Output = Result<Session, CliError>>,
    mut stopped: impl AsyncFnMut() -> &'static str,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), CliError>,
) -> Result<(), CliError> {
    let mut session = tokio::select! {
        session = login => session?,
        how = stopped() => return Err(format!("{how} before the login finished").into()),
    };
    let outcome = tokio::select! {
        outcome = async {
            session.pick_account().await?;
            work(&mut session).await
        } => outcome,
        how = stopped() => Err(format!("{how}; run the same command again to continue").into()),
    };
    session.end().await;
    outcome
}

/// Asks `question` on stdout and reads one line of the answer from stdin, in a guided command only.
pub(crate) async fn prompt(question: &str) -> Result<String, CliError> {
    if !GUIDED.load(Ordering::Relaxed) {
        return Err(format!(
            "{} needs an answer, and this command does not ask without a terminal",
            question.trim_end().trim_end_matches(':')
        )
        .into());
    }
    print!("{question}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    read_answer(|| read_line(std::io::stdin().lock())).await
}

/// Reads one line. A closed input is an error rather than an empty answer, so Ctrl-D never reads as Enter.
fn read_line(mut input: impl std::io::BufRead) -> std::io::Result<String> {
    let mut line = String::new();
    match input.read_line(&mut line)? {
        0 => Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "stdin closed before an answer",
        )),
        _ => Ok(line),
    }
}

/// Runs the blocking `read` on a thread of its own, so the caller can stop waiting for it: `with_session` ends the
/// command at Ctrl-C while a prompt waits. The thread is not one of the runtime's blocking threads, which the runtime
/// waits for when it is dropped, so a read nobody answers stays blocked until the process exits.
async fn read_answer(
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
) -> Result<String, CliError> {
    let (sender, answer) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let _ = sender.send(read());
    });
    let line = answer
        .await
        .map_err(|_| "the answer could not be read".to_owned())?
        .map_err(|error| error.to_string())?;
    Ok(line.trim().to_owned())
}

/// Asks a yes-or-no question in a guided command. Otherwise it refuses, naming `flag`.
async fn confirm(question: &str, yes: bool, flag: &str) -> Result<(), CliError> {
    if yes {
        return Ok(());
    }
    if !GUIDED.load(Ordering::Relaxed) {
        return Err(
            format!("{question} Run again with {flag} to go on without a terminal.").into(),
        );
    }
    match prompt(&format!("{question} [y/N] ")).await?.as_str() {
        "y" | "Y" | "yes" => Ok(()),
        _ => Err("stopped; nothing more was changed".into()),
    }
}

/// Opens `url` in the default browser, without a shell. A failure only means the user opens it by hand.
fn open_browser(url: &Url) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = Process::new(opener)
        .arg(url.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// A Koidrive server found in the account.
struct Found {
    script: Script,
    bucket: Option<Bound>,
    url: Url,
    info: Option<ServerInfo>,
}

/// The R2 bucket a server's Worker binds.
#[derive(Clone)]
struct Bound {
    name: String,
    jurisdiction: Jurisdiction,
}

/// The Cloudflare account a command acts on, with an access token that is fresh for the next calls.
trait Cloud {
    fn account(&self) -> &str;
    async fn fresh(&mut self) -> Result<Cloudflare, CliError>;
}

/// One command's Cloudflare login: the grant and the account the user picked on the consent page.
struct Session {
    http: reqwest::Client,
    grant: Grant,
    /// The Cloudflare API's base URL. Tests point it at a local server.
    api_base: Url,
    account: Account,
    subdomain: Option<String>,
}

impl Cloud for Session {
    fn account(&self) -> &str {
        &self.account.id
    }

    /// Refreshes the access token when it is about to expire. A refresh that fails ends the command, because a retry
    /// with the old refresh token would revoke the grant (section 2).
    async fn fresh(&mut self) -> Result<Cloudflare, CliError> {
        if self.grant.expires_within(TOKEN_MARGIN) {
            self.grant.refresh(&self.http).await.map_err(|error| {
                CliError::caused(
                    format!("{error}; run the command again to log in again"),
                    error,
                )
            })?;
        }
        Ok(self.api())
    }
}

impl Session {
    /// Logs in with Cloudflare and returns once the code is exchanged for a grant, before any other call.
    async fn login(args: &LoginArgs) -> Result<Session, CliError> {
        let http = koidrive_cloudflare::http_client().map_err(|error| error.to_string())?;
        let loopback = if args.no_browser {
            None
        } else {
            Some(Loopback::bind(args.port).await?)
        };
        let port = match &loopback {
            Some(loopback) => loopback.port(),
            None => args.port.unwrap_or(8976),
        };
        let login = Login::new(Endpoints::cloudflare(), CLIENT_ID, port);
        let page = login.authorize_url();
        let code = match &loopback {
            Some(loopback) => {
                println!(
                    "Log in with Cloudflare in your browser. If it does not open, open this page:\n{page}"
                );
                open_browser(&page);
                tokio::time::timeout(LOGIN_WAIT, loopback.callback(&login))
                    .await
                    .map_err(|_| {
                        CliError::new(
                            Code::AuthRequired,
                            "the login did not finish within 10 minutes",
                        )
                    })??
            }
            None => {
                println!(
                    "Open this page on any device, log in with Cloudflare and select Authorize:\n{page}"
                );
                println!(
                    "The browser then fails to load a page on 127.0.0.1. Copy that page's address."
                );
                let pasted = prompt("Address: ").await?;
                let url = Url::parse(&pasted)
                    .map_err(|error| CliError::usage(format!("that is not an address: {error}")))?;
                login.code_from(&url)?
            }
        };
        let grant = login.exchange(&http, code).await?;
        Ok(Session {
            http,
            grant,
            api_base: Url::parse(API_BASE).expect("the API base is a URL"),
            account: Account {
                id: String::new(),
                name: String::new(),
            },
            subdomain: None,
        })
    }

    /// Reads the one account the user picked on the consent page.
    async fn pick_account(&mut self) -> Result<(), CliError> {
        let accounts = self
            .api()
            .accounts()
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account"))?;
        match <[Account; 1]>::try_from(accounts) {
            Ok([account]) => self.account = account,
            Err(accounts) => {
                return Err(format!(
                    "the login gave access to {} accounts; pick exactly one on the consent page",
                    accounts.len()
                )
                .into());
            }
        }
        println!(
            "Logged in to the Cloudflare account {}.",
            printable(&self.account.name)
        );
        Ok(())
    }

    /// Revokes the grant, retrying three times. A failure leaves the grant active until its tokens expire, so the user
    /// is told where to revoke it.
    async fn end(self) {
        for attempt in 0..4 {
            match self.grant.revoke(&self.http).await {
                Ok(()) => return,
                Err(error) if attempt == 3 => {
                    eprintln!(
                        "koi: could not revoke the Cloudflare login ({error}). It stays active; its access token expires within an hour. Revoke it now under Manage OAuth authorizations in the Cloudflare dashboard."
                    );
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        }
    }

    fn api(&self) -> Cloudflare {
        Cloudflare::new(
            self.http.clone(),
            self.api_base.clone(),
            self.grant.access_token().clone(),
        )
    }

    async fn subdomain(&mut self) -> Result<String, CliError> {
        if let Some(subdomain) = &self.subdomain {
            return Ok(subdomain.clone());
        }
        let subdomain = self
            .api()
            .workers_subdomain(self.account())
            .await
            .map_err(|error| {
                CliError::from(error).context("cannot read the account's workers.dev subdomain")
            })?;
        self.subdomain = Some(subdomain.clone());
        Ok(subdomain)
    }

    async fn worker_url(&mut self, script: &str) -> Result<Url, CliError> {
        let subdomain = self.subdomain().await?;
        Url::parse(&format!("https://{script}.{subdomain}.workers.dev/")).map_err(|error| {
            CliError::from(format!(
                "{script} does not make a workers.dev address: {error}"
            ))
        })
    }

    /// Waits until R2 is enabled on the account. R2 cannot be enabled through the API, so the user completes the
    /// dashboard's checkout (section 4.1).
    async fn require_r2(&mut self) -> Result<(), CliError> {
        let started = Instant::now();
        let mut told = false;
        loop {
            match self
                .fresh()
                .await?
                .buckets(self.account(), Jurisdiction::Default)
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if error.has_code(R2_DISABLED) => {
                    if !told {
                        let page = Url::parse(&format!(
                            "https://dash.cloudflare.com/{}/r2/overview",
                            self.account()
                        ))
                        .expect("the R2 page is a URL");
                        println!(
                            "R2 is not enabled on this account. Koidrive keeps your files in R2."
                        );
                        println!(
                            "Enable it on this page. Its checkout includes free monthly usage and bills use beyond it:\n{page}"
                        );
                        open_browser(&page);
                        println!("Waiting for R2...");
                        told = true;
                    }
                    if started.elapsed() > R2_WAIT {
                        return Err("R2 was not enabled within 30 minutes; run the command again once it is".into());
                    }
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
                Err(error) => {
                    return Err(CliError::caused(
                        format!("cannot list the account's R2 buckets: {error}"),
                        error,
                    ));
                }
            }
        }
    }

    /// The Koidrive servers in the account (section 6): scripts with the tag, and untagged scripts that bind the
    /// Koidrive Durable Object classes, each checked with `GET /v1/server`. A tagged script that binds a bucket but does
    /// not answer is a deploy that stopped halfway, and is kept without `info`.
    async fn find_servers(&mut self) -> Result<Vec<Found>, CliError> {
        let binding = embedded_bundle()?.manifest.r2_binding;
        let api = self.fresh().await?;
        let scripts = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
        let mut found = Vec::new();
        for script in scripts {
            let settings = api
                .script_settings(self.account(), &script.id)
                .await
                .map_err(|error| {
                    CliError::caused(
                        format!("cannot read the settings of {}: {error}", script.id),
                        error,
                    )
                })?;
            let tagged = script.tags.iter().any(|tag| tag == TAG);
            let binds = |class: &str| {
                settings.bindings.iter().any(|bound| {
                    bound.kind == "durable_object_namespace"
                        && bound.class_name.as_deref() == Some(class)
                })
            };
            if !tagged && !SERVER_CLASSES.iter().all(|class| binds(class)) {
                continue;
            }
            let url = self.worker_url(&script.id).await?;
            let info = server_info(&url).await;
            let bucket = settings.r2_bucket(&binding).and_then(|bound| {
                Some(Bound {
                    name: bound.bucket_name.clone()?,
                    jurisdiction: bound.jurisdiction.unwrap_or_default(),
                })
            });
            if info.is_some() || (tagged && bucket.is_some()) {
                found.push(Found {
                    script,
                    bucket,
                    url,
                    info,
                });
            }
        }
        Ok(found)
    }

    /// The one server a command acts on: the named one, or the only one.
    async fn pick(&mut self, name: Option<&str>) -> Result<Found, CliError> {
        let mut found = self.find_servers().await?;
        if let Some(name) = name {
            found.retain(|server| server.script.id == name);
        }
        match found.len() {
            0 => Err(CliError::from(match name {
                Some(name) => format!("the account has no Koidrive server named {name}"),
                None => "the account has no Koidrive server; run koi server deploy".into(),
            })),
            1 => Ok(found.remove(0)),
            _ => Err(format!(
                "the account has several Koidrive servers; name one with --worker: {}",
                found
                    .iter()
                    .map(|server| server.script.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
            .into()),
        }
    }

    /// The state file of an unfinished deploy in this account, if there is one. Any other run that has not finished
    /// stops the command here, before it calls the account: its file is that run's only record of what it created.
    fn resumable(&self, paths: &Paths) -> Result<Option<SetupState>, CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(state) if self.continues(&state) => Ok(Some(state)),
            Some(other) => Err(unfinished(&other, self.account(), file.path())),
            None => Ok(None),
        }
    }

    /// Whether `state` is an unfinished deploy in this session's account, which setup goes on with.
    fn continues(&self, state: &SetupState) -> bool {
        state.account == self.account() && state.step != Step::Uninstalling
    }

    /// `koi server deploy` after the login: finish an interrupted deploy, or deploy a new server into an account
    /// that has none.
    async fn server_deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let found = self.find_servers().await?;
        if let Some(server) = found.first() {
            return Err(format!(
                "the account already has a Koidrive server, {} at {}; run koi server add-device",
                server.script.id, server.url
            )
            .into());
        }
        self.deploy(paths, device_name).await
    }

    /// `koi server add-device` after the login: add this device to the named server, or to the only one.
    async fn server_add_device(
        &mut self,
        paths: &Paths,
        server: Option<&str>,
        device_name: &str,
    ) -> Result<(), CliError> {
        if let (Some(state), Some(name)) = (self.resumable(paths)?, server)
            && state.script != name
        {
            return Err(unfinished(
                &state,
                self.account(),
                StateFile::new(paths).path(),
            ));
        }
        let found = self.pick(server).await?;
        self.add_device(paths, found, device_name, false).await
    }

    /// `koi setup` after the login: finish an interrupted deploy, deploy a new server, or add this device.
    async fn setup(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        if let Some(state) = self.resumable(paths)? {
            println!(
                "Continuing the setup that stopped earlier ({}).",
                state.script
            );
            return self.finish_deploy(paths, state, device_name).await;
        }
        crate::telemetry::step(SetupStep::R2Checkout, self.require_r2()).await?;
        let mut found = self.find_servers().await?;
        let server = match found.len() {
            0 => return self.deploy(paths, device_name).await,
            1 => found.remove(0),
            _ => {
                println!("The account has several Koidrive servers:");
                for (index, server) in found.iter().enumerate() {
                    println!(
                        "  {}. {} at {}",
                        index + 1,
                        printable(&server.script.id),
                        printable(server.url.as_str())
                    );
                }
                let choice = prompt("Which one? ").await?;
                let index = choice
                    .parse::<usize>()
                    .ok()
                    .filter(|index| (1..=found.len()).contains(index))
                    .ok_or_else(|| format!("{choice:?} is not one of the numbers"))?;
                found.remove(index - 1)
            }
        };
        self.add_device(paths, server, device_name, true).await
    }

    /// Picks a free Worker name and a new bucket name, then deploys and claims a new server (sections 4.3 and 4.4).
    async fn deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), CliError> {
        let bundle = embedded_bundle()?;
        let api = self.fresh().await?;
        let scripts: Vec<String> = api
            .scripts(self.account())
            .await
            .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?
            .into_iter()
            .map(|script| script.id)
            .collect();
        let base = &bundle.manifest.default_bucket;
        let state = SetupState {
            account: self.account().to_owned(),
            script: free_name(base, &scripts),
            bucket: new_bucket_name(base),
            ..SetupState::default()
        };
        StateFile::new(paths).save(&state)?;
        self.finish_deploy(paths, state, device_name).await
    }

    /// Runs the deploy steps after `state.step` and claims the server. Every step is safe to repeat.
    async fn finish_deploy(
        &mut self,
        paths: &Paths,
        mut state: SetupState,
        device_name: &str,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let bundle = embedded_bundle()?;
        let mut key = None;
        if state.step < Step::BucketCreated {
            crate::telemetry::step(SetupStep::Bucket, create_bucket(self, &file, &mut state))
                .await?;
        }
        if state.step < Step::ScriptUploaded {
            keep_script_name(
                self,
                &mut state,
                &bundle.manifest.r2_binding,
                &bundle.manifest.default_bucket,
            )
            .await?;
            println!(
                "Uploading Koidrive {} as the Worker {}...",
                bundle.manifest.version, state.script
            );
            let secret = Secret::random(32);
            state.secret_set = true;
            file.save(&state)?;
            let bucket = Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            };
            let upload = self.upload(&bundle, &state.script, &bucket, None, Some(&secret));
            crate::telemetry::step(SetupStep::Script, upload).await?;
            state.step = Step::ScriptUploaded;
            file.save(&state)?;
            key = Some(secret);
        }
        let url = self.worker_url(&state.script).await?;
        if state.step < Step::Serving {
            crate::telemetry::step(SetupStep::Serving, self.serve(&state.script, &url)).await?;
            state.step = Step::Serving;
            file.save(&state)?;
        }
        let server = Found {
            script: Script {
                id: state.script.clone(),
                created_on: None,
                tags: vec![TAG.to_owned()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            }),
            info: server_info(&url).await,
            url,
        };
        let claim =
            Claim::new(self, paths, &server, device_name, state, key).run(Duration::from_secs(60));
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Adds this device to a server that already exists (section 4.5). `interactive` lets it finish a deploy that
    /// stopped before the server answered, and offer an upgrade when the server's claim code does not handle setup
    /// attempts; otherwise it stops and names the command that does.
    async fn add_device(
        &mut self,
        paths: &Paths,
        mut server: Found,
        device_name: &str,
        interactive: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        let saved = match file.load()? {
            Some(state) if self.continues(&state) && state.script == server.script.id => {
                Some(state)
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => None,
        };
        let info = match server.info.clone() {
            Some(info) => info,
            None if interactive => {
                println!(
                    "{} stopped halfway through its deploy. Finishing it...",
                    server.script.id
                );
                let info = self.serve(&server.script.id, &server.url).await?;
                server.info = Some(info.clone());
                info
            }
            None => {
                return Err(format!(
                    "{} stopped halfway through its deploy; run koi setup to finish it",
                    server.script.id
                )
                .into());
            }
        };
        let mut window = Duration::from_secs(60);
        if !info.setup_attempts {
            let reason = format!(
                "{} runs Koidrive {}, which cannot make a retried claim end with one device. It needs an upgrade to {} first.",
                server.script.id,
                info.server_version,
                embedded_bundle()?.manifest.version
            );
            if !interactive {
                return Err(
                    format!("{reason} Run koi server upgrade, then this command again.").into(),
                );
            }
            if confirm(&format!("{reason} Upgrade it now?"), false, "--yes")
                .await
                .is_err()
            {
                return Err(
                    format!("{reason} Run koi server upgrade, then koi setup again.").into(),
                );
            }
            self.upgrade(&server, false).await?;
            wait_for_setup_attempts(&server.url).await?;
            window = Duration::from_secs(330);
        }
        if !info.claimed {
            window = Duration::from_secs(330);
        }
        let state = match saved {
            Some(state) => state,
            None => SetupState {
                account: self.account().to_owned(),
                script: server.script.id.clone(),
                bucket: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.name.clone())
                    .unwrap_or_default(),
                jurisdiction: server
                    .bucket
                    .as_ref()
                    .map(|bucket| bucket.jurisdiction)
                    .unwrap_or_default(),
                step: Step::Serving,
                ..SetupState::default()
            },
        };
        let claim = Claim::new(self, paths, &server, device_name, state, None).run(window);
        crate::telemetry::step(SetupStep::Claim, claim).await
    }

    /// Turns on the Worker's workers.dev route and waits until the server answers there.
    async fn serve(&mut self, script: &str, url: &Url) -> Result<ServerInfo, CliError> {
        self.fresh()
            .await?
            .enable_workers_dev(self.account(), script)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot turn on workers.dev for {script}"))
            })?;
        wait_until_serving(url, None).await
    }

    /// Uploads the embedded server over an existing one (section 5).
    async fn upgrade(&mut self, server: &Found, yes: bool) -> Result<(), CliError> {
        let bundle = embedded_bundle()?;
        let Some(info) = &server.info else {
            return Err(format!(
                "{} does not answer; run koi setup to finish its deploy",
                server.script.id
            )
            .into());
        };
        if newer(&info.server_version, &bundle.manifest.version) {
            return Err(format!(
                "{} runs Koidrive {}, newer than this CLI's {}; install a newer koi",
                server.script.id, info.server_version, bundle.manifest.version
            )
            .into());
        }
        if !server.script.tags.iter().any(|tag| tag == TAG) {
            confirm(
                &format!(
                    "{} was deployed with the deploy button. A push to the repository the button created deploys that repository's code again and replaces this upgrade. Update that repository too, or disconnect its builds in the dashboard. Upgrade now?",
                    server.script.id
                ),
                yes,
                "--yes",
            ).await?;
        }
        let bucket = server
            .bucket
            .clone()
            .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
        println!(
            "Uploading Koidrive {} over {} {}...",
            bundle.manifest.version, server.script.id, info.server_version
        );
        self.upload(
            &bundle,
            &server.script.id,
            &bucket,
            server.script.migration_tag.as_deref(),
            None,
        )
        .await?;
        wait_until_serving(&server.url, Some(&bundle.manifest.version)).await?;
        println!(
            "{} now runs Koidrive {}. Open sync sessions move to it within 5 minutes.",
            server.script.id, bundle.manifest.version
        );
        Ok(())
    }

    async fn upload(
        &mut self,
        bundle: &Bundle,
        script: &str,
        bucket: &Bound,
        deployed_tag: Option<&str>,
        secret: Option<&Secret>,
    ) -> Result<(), CliError> {
        let api = self.fresh().await?;
        let jwt = api
            .upload_assets(self.account(), script, &bundle.assets)
            .await
            .map_err(|error| CliError::from(error).context("cannot upload the static assets"))?;
        let plan = UploadPlan::new(
            bundle,
            &bucket.name,
            bucket.jurisdiction,
            deployed_tag,
            secret,
            Some(&jwt),
        )
        .map_err(|error| error.to_string())?;
        api.upload_script(self.account(), script, &plan)
            .await
            .map_err(|error| {
                CliError::from(error).context(format!("cannot upload the Worker {script}"))
            })?;
        Ok(())
    }

    /// Deletes the Worker, then empties and deletes its bucket unless another Worker or application uses it (section 5).
    /// A state file of an unfinished uninstall in this account continues it, unless `bucket` names one.
    async fn uninstall(
        &mut self,
        paths: &Paths,
        name: Option<&str>,
        bucket: Option<&str>,
        jurisdiction: Jurisdiction,
        yes: bool,
    ) -> Result<(), CliError> {
        let file = StateFile::new(paths);
        match file.load()? {
            Some(state)
                if state.account == self.account()
                    && state.step == Step::Uninstalling
                    && bucket.is_none() =>
            {
                println!(
                    "Continuing the uninstall that stopped earlier ({}).",
                    state.bucket
                );
                let script = state.script.clone();
                uninstall::finish(self, &file, state).await?;
                self.forget(paths, &script).await;
                return Ok(());
            }
            Some(other) => return Err(unfinished(&other, self.account(), file.path())),
            None => {}
        }
        let (state, contents) = match bucket {
            Some(bucket) => uninstall::plan(self, None, bucket, jurisdiction).await?,
            None => {
                let server = self.pick(name).await?;
                let bound = server
                    .bucket
                    .clone()
                    .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
                uninstall::plan(self, Some(&server.script), &bound.name, bound.jurisdiction).await?
            }
        };
        let worker = match state.script.as_str() {
            "" => String::new(),
            script => format!("the Worker {script} and "),
        };
        confirm(
            &format!(
                "Delete {worker}the R2 bucket {} in the {} jurisdiction with its {} objects? Every file in the bucket is lost, also anything another application stored there through R2's S3 API.",
                state.bucket,
                state.jurisdiction.as_str(),
                contents.objects
            ),
            yes,
            "--yes",
        ).await?;
        file.save(&state)?;
        let script = state.script.clone();
        uninstall::finish(self, &file, state).await?;
        self.forget(paths, &script).await;
        Ok(())
    }

    /// Stops and removes this device's background service when the deleted Worker `script` was the server it syncs
    /// with, at its workers.dev address.
    async fn forget(&mut self, paths: &Paths, script: &str) {
        if script.is_empty() {
            return;
        }
        let url = match self.worker_url(script).await {
            Ok(url) => url,
            Err(error) => {
                println!(
                    "Couldn't check whether this device synced with the deleted server: {}. If it did, unlink its folders with koi folder unlink, which also removes the background service.",
                    printable(error.message.trim_end_matches('.'))
                );
                return;
            }
        };
        if let Some(report) = crate::service::forget(paths, &url) {
            print!("{}", report.text());
        }
    }
}

/// Fails when this koi has no embedded server bundle. The commands that find, deploy or upgrade a server call it before
/// the Cloudflare login, so that nobody authorizes a login that cannot be used.
fn bundled() -> Result<(), String> {
    if SERVER_BUNDLE.is_empty() {
        return Err("this koi was built without a server bundle, so it cannot find, deploy or upgrade a server; build it with KOIDRIVE_SERVER_BUNDLE as docs/guide/install.md describes".into());
    }
    Ok(())
}

/// The embedded server bundle, read through [`Bundle::from_dir`] from a private copy, so the release's bundle passes the
/// same checks as one on disk.
pub(crate) fn embedded_bundle() -> Result<Bundle, CliError> {
    bundled()?;
    let broken = |error: &dyn std::fmt::Display| {
        CliError::from(format!("this koi's server bundle is broken: {error}"))
    };
    let dir = tempfile::tempdir()
        .map_err(|error| CliError::from(format!("cannot unpack the server bundle: {error}")))?;
    for (path, bytes) in SERVER_BUNDLE {
        let target = dir.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| broken(&error))?;
        }
        std::fs::write(&target, bytes).map_err(|error| broken(&error))?;
    }
    Bundle::from_dir(dir.path()).map_err(|error| broken(&error))
}

/// Creates the bucket `state.bucket` and records the step. `deploy` saved that name, which only this deploy produces, before
/// the first try, so a 409 for it means an earlier try of this deploy created the bucket and its answer was lost.
async fn create_bucket(
    cloud: &mut impl Cloud,
    file: &StateFile,
    state: &mut SetupState,
) -> Result<(), CliError> {
    println!("Creating the R2 bucket {}...", state.bucket);
    let account = cloud.account().to_owned();
    match cloud
        .fresh()
        .await?
        .create_bucket(&account, &state.bucket, state.jurisdiction)
        .await
    {
        Ok(()) => {}
        Err(error) if error.status() == Some(409) => {}
        Err(error) => {
            return Err(CliError::caused(
                format!("cannot create the bucket {}: {error}", state.bucket),
                error,
            ));
        }
    }
    state.step = Step::BucketCreated;
    Ok(file.save(state)?)
}

/// A bucket name only this deploy produces: `base`, a hyphen and 16 random hex digits (64 bits). Another
/// application cannot hold it unless it guesses it, so the deploy never takes over someone else's bucket.
fn new_bucket_name(base: &str) -> String {
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("the operating system provides random bytes");
    let suffix: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{base}-{suffix}")
}

/// Keeps `state.script` while no other application holds the name. A Worker under it that has the tag and binds
/// `state.bucket` is this deploy's own upload from before the state file recorded it, so the upload goes over it. Any
/// other Worker under the name belongs to someone else, so the deploy moves to the first free name after `base`.
async fn keep_script_name(
    cloud: &mut impl Cloud,
    state: &mut SetupState,
    binding: &str,
    base: &str,
) -> Result<(), CliError> {
    let api = cloud.fresh().await?;
    let account = cloud.account().to_owned();
    let scripts = api
        .scripts(&account)
        .await
        .map_err(|error| CliError::from(error).context("cannot list the account's Workers"))?;
    let Some(current) = scripts.iter().find(|script| script.id == state.script) else {
        return Ok(());
    };
    if current.tags.iter().any(|tag| tag == TAG) {
        let settings = api
            .script_settings(&account, &state.script)
            .await
            .map_err(|error| {
                CliError::caused(
                    format!("cannot read the settings of {}: {error}", state.script),
                    error,
                )
            })?;
        if settings.r2_bucket(binding).is_some_and(|bound| {
            bound.bucket_name.as_deref() == Some(state.bucket.as_str())
                && bound.jurisdiction.unwrap_or_default() == state.jurisdiction
        }) {
            return Ok(());
        }
    }
    let taken: Vec<String> = scripts.into_iter().map(|script| script.id).collect();
    let free = free_name(base, &taken);
    println!(
        "Another application now has a Worker named {}, so the server becomes the Worker {free}.",
        state.script
    );
    state.script = free;
    Ok(())
}

/// `base`, or the first of `base-2`, `base-3`, ... that is not taken.
fn free_name(base: &str, taken: &[String]) -> String {
    std::iter::once(base.to_owned())
        .chain((2..).map(|n| format!("{base}-{n}")))
        .find(|name| !taken.contains(name))
        .expect("some name is free")
}

/// Whether version `a` is newer than `b`, comparing dotted numbers. Versions that are not dotted numbers compare
/// as equal.
fn newer(a: &str, b: &str) -> bool {
    let parse = |version: &str| -> Option<Vec<u64>> {
        version.split('.').map(|part| part.parse().ok()).collect()
    };
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// The Koidrive API of the server at `url`. Tests serve it over http on a loopback address.
fn server_api(url: &Url) -> Result<Api, ApiError> {
    if cfg!(test) {
        Api::local_development(url.clone())
    } else {
        Api::new(url.clone())
    }
}

async fn server_info(url: &Url) -> Option<ServerInfo> {
    let api = server_api(url).ok()?;
    tokio::time::timeout(Duration::from_secs(10), api.server_info())
        .await
        .ok()?
        .ok()
}

/// Polls `GET /v1/server` until it answers, and until it reports `version` when one is given.
async fn wait_until_serving(url: &Url, version: Option<&str>) -> Result<ServerInfo, CliError> {
    let started = Instant::now();
    loop {
        if let Some(info) = server_info(url)
            .await
            .filter(|info| version.is_none_or(|version| info.server_version == version))
        {
            return Ok(info);
        }
        if started.elapsed() > SERVE_WAIT {
            return Err(format!(
                "{url} did not answer as expected within {} seconds",
                SERVE_WAIT.as_secs()
            )
            .into());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Polls until the Account DO reports `setup_attempts`, which can take up to 300 seconds after an upgrade.
async fn wait_for_setup_attempts(url: &Url) -> Result<(), CliError> {
    println!("Waiting for the server's storage to move to the new version (up to 330 seconds)...");
    let started = Instant::now();
    loop {
        if server_info(url)
            .await
            .is_some_and(|info| info.setup_attempts)
        {
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(330) {
            return Err("the server did not move to the new version within 330 seconds; run the command again".into());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use koidrive_proto::v1::DeviceCredentials;
    use serde_json::json;

    use super::fake::{self, ACCOUNT, Fake, Reply, cf, proto, revoked, tokens};
    use super::*;

    #[test]
    fn a_closed_input_is_not_an_empty_answer() {
        let closed = read_line(&b""[..]).unwrap_err();
        assert_eq!(closed.kind(), std::io::ErrorKind::UnexpectedEof);
        assert_eq!(read_line(&b"\n"[..]).unwrap(), "\n");
        assert_eq!(read_line(&b"n\n"[..]).unwrap(), "n\n");
    }

    #[test]
    fn setup_guides_only_on_two_terminals_without_json_or_ci() {
        let guided = |stdin, stderr, json, ci| mode(stdin, stderr, json, ci, false).unwrap();
        assert_eq!(guided(true, true, false, false), Mode::Guided);
        for (stdin, stderr, json, ci) in [
            (false, true, false, false),
            (true, false, false, false),
            (false, false, false, false),
            (true, true, true, false),
            (true, true, false, true),
            (true, true, true, true),
        ] {
            assert_eq!(
                guided(stdin, stderr, json, ci),
                Mode::Direct,
                "stdin {stdin}, stderr {stderr}, json {json}, CI {ci}"
            );
        }
    }

    #[test]
    fn interactive_asks_for_the_guide_and_fails_without_terminals() {
        assert_eq!(mode(true, true, false, true, true).unwrap(), Mode::Guided);
        for (stdin, stderr) in [(false, true), (true, false), (false, false)] {
            let error = mode(stdin, stderr, false, false, true).unwrap_err();
            assert_eq!(error.code, Code::Usage);
            assert!(error.message.contains("needs a terminal"), "{error:?}");
        }
        assert_eq!(
            mode(true, true, true, false, true).unwrap_err().code,
            Code::Usage
        );
    }

    #[test]
    fn setup_lists_every_way_in_with_its_argv() {
        let argv: Vec<String> = choices()
            .iter()
            .map(|choice| choice.argv.join(" "))
            .collect();
        assert_eq!(
            argv,
            [
                "koi join <CODE>",
                "koi server deploy --interactive",
                "koi server add-device --interactive",
                "koi server claim <SERVER_URL> <CODE> --user <NAME>",
                "koi setup --interactive",
            ]
        );
    }

    #[tokio::test]
    async fn json_and_a_pasted_login_stop_before_the_login_and_name_the_interactive_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };

        let error = server(&paths, parse(&["koi", "server", "deploy"]), true)
            .await
            .unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&["koi", "server", "deploy", "--interactive"])]
        );

        let action = parse(&["koi", "server", "upgrade", "--worker", "fb", "--no-browser"]);
        let error = server(&paths, action, false).await.unwrap_err();
        assert_eq!(error.code, Code::AuthRequired);
        assert_eq!(
            error.next,
            [Next::of(&[
                "koi",
                "server",
                "upgrade",
                "--worker",
                "fb",
                "--interactive",
                "--no-browser"
            ])]
        );
    }

    #[tokio::test]
    async fn owner_commands_without_interactive_and_terminals_need_auth_and_name_the_interactive_command()
     {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let parse = |args: &[&str]| match crate::Cli::try_parse_from(args).unwrap().command {
            Some(crate::Command::Server { action }) => action,
            _ => unreachable!(),
        };
        for (args, next) in [
            (&["deploy"][..], &["deploy", "--interactive"][..]),
            (&["add-device"], &["add-device", "--interactive"]),
            (
                &["upgrade", "--worker", "fb", "--yes"],
                &["upgrade", "--worker", "fb", "--interactive"],
            ),
            (&["upgrade"], &["upgrade", "--interactive"]),
            (
                &["delete", "--bucket", "fb-1a2b", "--yes"],
                &["delete", "--bucket", "fb-1a2b", "--interactive"],
            ),
            (&["deploy", "--interactive"], &["deploy", "--interactive"]),
        ] {
            let argv = [&["koi", "server"][..], args].concat();
            let error = server(&paths, parse(&argv), false).await.unwrap_err();
            assert_eq!(error.code, Code::AuthRequired, "{args:?}");
            assert_eq!(
                error.next,
                [Next::of(&[&["koi", "server"][..], next].concat())],
                "{args:?}"
            );
        }
        assert!(!dir.path().join("state").exists(), "nothing was set up");
    }

    fn deployed(fake: &Fake) -> Found {
        Found {
            script: Script {
                id: "koidrive".into(),
                created_on: None,
                tags: vec![TAG.into()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: "koidrive".into(),
                jurisdiction: Jurisdiction::Default,
            }),
            url: fake.base.clone(),
            info: None,
        }
    }

    #[test]
    fn a_stop_ends_a_prompt_nobody_answers_and_the_runtime_does_not_wait_for_it() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = runtime.block_on(guarded(
            async {
                read_answer(|| {
                    loop {
                        std::thread::park();
                    }
                })
                .await?;
                unreachable!("nobody answers the prompt")
            },
            async || "stopped",
            async |_: &mut Session| Ok(()),
        ));
        assert_eq!(
            outcome.unwrap_err().message,
            "stopped before the login finished"
        );
        let (dropped, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(runtime);
            dropped.send(()).unwrap();
        });
        done.recv_timeout(Duration::from_secs(5))
            .expect("the runtime stops without waiting for the blocked read");
    }

    #[tokio::test]
    async fn an_answer_is_read_without_its_line_end() {
        let answer = read_answer(|| Ok(" y\n".to_owned())).await;
        assert_eq!(answer.unwrap(), "y");
        let failed = read_answer(|| Err(std::io::Error::other("closed"))).await;
        assert_eq!(failed.unwrap_err().message, "closed");
    }

    #[tokio::test]
    async fn a_stop_during_the_account_lookup_still_revokes_the_grant() {
        let oauth = Fake::start(vec![tokens(), revoked(), revoked()]).await;
        let api = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base =
            Url::parse(&format!("http://{}/client/v4/", api.local_addr().unwrap())).unwrap();
        let mut held = Vec::new();
        let stopped = async || {
            held.push(api.accept().await.unwrap().0);
            "stopped"
        };

        let outcome = guarded(
            async { Ok(fake::login(&oauth, api_base).await) },
            stopped,
            async |_: &mut Session| Ok(()),
        )
        .await;

        assert_eq!(
            outcome.unwrap_err().message,
            "stopped; run the same command again to continue"
        );
        let revocations = oauth
            .seen()
            .iter()
            .filter(|seen| seen.path == "/oauth2/revoke")
            .count();
        assert_eq!(revocations, 2);
    }

    #[tokio::test]
    async fn a_resumed_deploy_moves_off_a_worker_name_another_application_took() {
        let binds = |bucket: &str| json!({ "bindings": [{ "type": "r2_bucket", "name": "BUCKET", "bucket_name": bucket }] });
        let cases = [
            (
                json!([{ "id": "koidrive", "tags": [] }]),
                None,
                "koidrive-2",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }, { "id": "koidrive-2", "tags": [] }]),
                Some(binds("photos")),
                "koidrive-3",
            ),
            (
                json!([{ "id": "koidrive", "tags": [TAG] }]),
                Some(binds("koidrive")),
                "koidrive",
            ),
            (json!([{ "id": "photos", "tags": [] }]), None, "koidrive"),
        ];
        for (scripts, settings, expected) in cases {
            let mut replies = vec![tokens(), cf("GET", "/workers/scripts", scripts)];
            replies.extend(
                settings.map(|settings| cf("GET", "/workers/scripts/koidrive/settings", settings)),
            );
            let fake = Fake::start(replies).await;
            let mut session = fake::session(&fake).await;
            let mut state = SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: "koidrive".into(),
                step: Step::BucketCreated,
                ..SetupState::default()
            };

            keep_script_name(&mut session, &mut state, "BUCKET", "koidrive")
                .await
                .unwrap();

            assert_eq!(state.script, expected);
        }
    }

    #[test]
    fn a_deploy_names_its_bucket_with_a_random_suffix() {
        let first = new_bucket_name("koidrive");
        let suffix = first.strip_prefix("koidrive-").unwrap();
        assert_eq!(suffix.len(), 16);
        assert!(
            suffix
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        );
        assert_ne!(first, new_bucket_name("koidrive"));
    }

    /// The bucket names of the creates `fake` received.
    fn created(fake: &Fake) -> Vec<String> {
        fake.seen()
            .iter()
            .filter(|seen| seen.method == "POST" && seen.path.ends_with("/r2/buckets"))
            .map(|seen| {
                let body: serde_json::Value = serde_json::from_slice(&seen.body).unwrap();
                body["name"].as_str().unwrap().to_owned()
            })
            .collect()
    }

    fn create_answer(status: u16) -> Reply {
        Reply {
            method: "POST",
            path: format!("/client/v4/accounts/{ACCOUNT}/r2/buckets"),
            status,
            content_type: "application/json",
            body: json!({ "success": status == 200, "errors": [], "result": {} })
                .to_string()
                .into_bytes(),
        }
    }

    #[tokio::test]
    async fn a_lost_create_answer_is_adopted_under_the_saved_name_on_the_next_run() {
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let name = new_bucket_name("koidrive");
        let planned = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: name.clone(),
            ..SetupState::default()
        };
        file.save(&planned).unwrap();

        let lost = Fake::start(vec![tokens(), create_answer(502)]).await;
        let mut state = file.load().unwrap().unwrap();
        let error = create_bucket(&mut fake::session(&lost).await, &file, &mut state)
            .await
            .unwrap_err()
            .message;
        assert!(error.contains("cannot create the bucket"), "{error}");
        assert_eq!(file.load().unwrap(), Some(planned));

        let retry = Fake::start(vec![tokens(), create_answer(409)]).await;
        let mut state = file.load().unwrap().unwrap();
        create_bucket(&mut fake::session(&retry).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&lost), [name.as_str()]);
        assert_eq!(created(&retry), [name.as_str()]);
        let saved = file.load().unwrap().unwrap();
        assert_eq!((saved.bucket, saved.step), (name, Step::BucketCreated));
    }

    #[tokio::test]
    async fn a_bucket_named_like_the_default_is_never_created_read_or_deleted() {
        let fake = Fake::start(vec![tokens(), create_answer(200)]).await;
        let dir = tempfile::tempdir().unwrap();
        let file = StateFile::new(&Paths::under(dir.path()));
        let mut state = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            ..SetupState::default()
        };

        create_bucket(&mut fake::session(&fake).await, &file, &mut state)
            .await
            .unwrap();

        assert_eq!(created(&fake), [state.bucket.as_str()]);
        assert_ne!(state.bucket, "koidrive");
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/r2/buckets/koidrive") || seen.method == "DELETE")
        );
    }

    #[tokio::test]
    async fn setup_finishes_a_deploy_that_stopped_before_the_server_answered() {
        let info = ServerInfo {
            server_version: "0.1.0".into(),
            setup_attempts: true,
            ..ServerInfo::default()
        };
        let issued = DeviceCredentials {
            user_id: "user".into(),
            device_id: "0194f8a2-0000-7000-8000-000000000002".into(),
            refresh_credential: "device".into(),
            ..DeviceCredentials::default()
        };
        let secrets = "/workers/scripts/koidrive/secrets";
        let fake = Fake::start(vec![
            tokens(),
            cf("POST", "/workers/scripts/koidrive/subdomain", json!(null)),
            proto("GET", "/v1/server", info),
            cf("GET", secrets, json!([])),
            cf("PUT", secrets, json!(null)),
            proto("POST", "/v1/claim", issued),
            cf(
                "DELETE",
                &format!("{secrets}/KOIDRIVE_RECOVERY_KEY"),
                json!(null),
            ),
            cf("GET", secrets, json!([])),
        ])
        .await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());

        session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap();

        assert!(fake.seen().iter().any(|seen| seen.method == "POST"
            && seen.path.ends_with("/workers/scripts/koidrive/subdomain")));
        assert!(paths.credential().exists());
        assert_eq!(StateFile::new(&paths).load().unwrap(), None);
    }

    #[tokio::test]
    async fn uninstall_stops_at_another_unfinished_run_before_calling_the_account() {
        let deploy = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        let uninstall = SetupState {
            step: Step::Uninstalling,
            ..deploy.clone()
        };
        let elsewhere = SetupState {
            account: "another-account".into(),
            ..uninstall.clone()
        };
        let bucket = new_bucket_name("koidrive");
        for (other, named) in [
            (deploy, None),
            (elsewhere, None),
            (uninstall, Some(bucket.as_str())),
        ] {
            let fake = Fake::start(vec![tokens()]).await;
            let mut session = fake::session(&fake).await;
            let before = fake.seen().len();
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            StateFile::new(&paths).save(&other).unwrap();

            let error = session
                .uninstall(&paths, None, named, Jurisdiction::Default, true)
                .await
                .unwrap_err()
                .message;

            assert!(error.contains("has not finished"), "{error}");
            assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other));
            assert_eq!(fake.seen().len(), before, "{error}");
        }
    }

    #[tokio::test]
    async fn setup_and_deploy_stop_at_an_unfinished_run_before_calling_the_account() {
        let others = [
            SetupState {
                account: "another-account".into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::BucketCreated,
                ..SetupState::default()
            },
            SetupState {
                account: ACCOUNT.into(),
                script: "koidrive".into(),
                bucket: new_bucket_name("koidrive"),
                step: Step::Uninstalling,
                ..SetupState::default()
            },
        ];
        for other in others {
            for entry in ["setup", "server deploy", "server add-device"] {
                let fake = Fake::start(vec![tokens()]).await;
                let mut session = fake::session(&fake).await;
                let before = fake.seen().len();
                let dir = tempfile::tempdir().unwrap();
                let paths = Paths::under(dir.path());
                StateFile::new(&paths).save(&other).unwrap();

                let error = match entry {
                    "setup" => session.setup(&paths, "desk").await,
                    "server deploy" => session.server_deploy(&paths, "desk").await,
                    _ => session.server_add_device(&paths, None, "desk").await,
                }
                .unwrap_err()
                .message;

                assert!(error.contains("has not finished"), "{entry}: {error}");
                assert_eq!(StateFile::new(&paths).load().unwrap(), Some(other.clone()));
                assert_eq!(fake.seen().len(), before, "{entry} called the account");
            }
        }
    }

    #[tokio::test]
    async fn adding_a_device_keeps_the_unfinished_deploy_of_another_worker() {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .add_device(&paths, deployed(&fake), "laptop", true)
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert!(
            !fake
                .seen()
                .iter()
                .any(|seen| seen.path.contains("/accounts/") || seen.path.starts_with("/v1/"))
        );
    }

    #[tokio::test]
    async fn adding_a_device_to_a_named_server_stops_at_another_workers_deploy_before_calling_the_account()
     {
        let fake = Fake::start(vec![tokens()]).await;
        let mut session = fake::session(&fake).await;
        let before = fake.seen().len();
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let unfinished = SetupState {
            account: ACCOUNT.into(),
            script: "koidrive-2".into(),
            bucket: new_bucket_name("koidrive"),
            step: Step::BucketCreated,
            ..SetupState::default()
        };
        StateFile::new(&paths).save(&unfinished).unwrap();

        let error = session
            .server_add_device(&paths, Some("koidrive"), "laptop")
            .await
            .unwrap_err()
            .message;

        assert!(
            error.contains("the deploy of koidrive-2 has not finished"),
            "{error}"
        );
        assert_eq!(StateFile::new(&paths).load().unwrap(), Some(unfinished));
        assert_eq!(fake.seen().len(), before);
    }

    #[test]
    fn free_names_count_up_from_the_default() {
        let taken = vec!["koidrive".to_owned(), "koidrive-2".to_owned()];
        assert_eq!(free_name("koidrive", &[]), "koidrive");
        assert_eq!(free_name("koidrive", &taken), "koidrive-3");
    }

    #[test]
    fn the_embedded_bundle_reads_back_when_the_build_has_one() {
        match option_env!("KOIDRIVE_SERVER_BUNDLE") {
            None => assert!(
                embedded_bundle()
                    .unwrap_err()
                    .message
                    .contains("built without a server bundle")
            ),
            Some(_) => {
                let bundle = embedded_bundle().unwrap();
                assert!(!bundle.module.is_empty() && !bundle.assets.is_empty());
            }
        }
    }

    #[test]
    fn newer_compares_dotted_numbers_only() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(!newer("0.3.0", "0.3.0"));
        assert!(!newer("0.2.9", "0.3.0"));
        assert!(!newer("0.4.0-rc1", "0.3.0"));
    }
}
