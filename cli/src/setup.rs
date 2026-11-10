// SPDX-License-Identifier: Apache-2.0

//! `flarebox setup` and `flarebox server ...`: log in with Cloudflare, then deploy a server to the user's account, add
//! this device to the server there, upgrade it or remove it (docs/design/cli-setup.md). The Cloudflare grant lives in
//! memory for one command and is revoked when the command ends, also on failure, Ctrl-C and SIGTERM (section 7).

#[cfg(test)]
mod bundle_files;
mod claim;
#[cfg(test)]
mod fake;
mod state;
mod uninstall;

use std::io::{BufRead, IsTerminal, Write};
use std::process::{Command as Process, Stdio};
use std::time::{Duration, Instant};

use flarebox_cloudflare::{
    Account, Bundle, CLIENT_ID, Cloudflare, Endpoints, Grant, Jurisdiction, Login, Loopback,
    Script, Secret, UploadPlan,
};
use flarebox_driver::Paths;
use flarebox_net::{Api, ApiError};
use flarebox_proto::v1::ServerInfo;
use tokio::signal::unix::{SignalKind, signal};
use url::Url;

use crate::DeviceArgs;
use crate::ipc::{self, Request, Response};
use claim::Claim;
use state::{SetupState, StateFile, Step};

const API_BASE: &str = "https://api.cloudflare.com/client/v4/";
/// Marks the Worker scripts `flarebox setup` uploads (section 6).
const TAG: &str = "flarebox-server";
/// The Durable Object classes that identify an untagged Flarebox Worker, such as a renamed button deploy.
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

#[derive(clap::Args)]
pub struct LoginArgs {
    /// Print the login page instead of opening a browser, and read the address the browser ends on from the terminal.
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
    /// Log in with Cloudflare and add this device to the server in the account.
    AddDevice {
        /// The server's Worker name, when the account holds several.
        #[arg(long)]
        server: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare and upload this CLI's server version.
    Upgrade {
        #[arg(long)]
        server: Option<String>,
        /// Go on without asking, for example past the Workers Builds warning.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        login: LoginArgs,
    },
    /// Log in with Cloudflare, delete the server's Worker, then empty and delete its bucket.
    Uninstall {
        #[arg(long)]
        server: Option<String>,
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

/// `flarebox setup` without a code: asks whether to join with a code or log in with Cloudflare.
pub async fn setup(paths: &Paths, device_name: &str, login: &LoginArgs) -> Result<(), String> {
    if !std::io::stdin().is_terminal() {
        return Err("flarebox setup asks questions; in a script use flarebox join, flarebox server deploy or flarebox server add-device".into());
    }
    not_set_up(paths)?;
    println!("How do you want to set up Flarebox on this machine?");
    println!(
        "  1. Log in with Cloudflare: deploy a server to your account, or add this device to the server there"
    );
    println!("  2. Enter a pairing code from another device");
    match prompt("Choose 1 or 2: ")?.as_str() {
        "1" => {
            with_session(login, async |session| {
                session.setup(paths, device_name).await
            })
            .await
        }
        "2" => {
            let code = prompt("Pairing code: ")?;
            let invitation = crate::pairing::parse(&code, None)?;
            let request = Request::Pair {
                server: invitation.server,
                code: invitation.code,
                name: device_name.to_owned(),
            };
            report_paired(paths, ipc::run(paths, request).await?)
        }
        other => Err(format!("{other:?} is not 1 or 2")),
    }
}

/// `flarebox server <action>`. These never ask unless a terminal is attached and the step needs a yes.
pub async fn server(paths: &Paths, action: ServerAction) -> Result<(), String> {
    match action {
        ServerAction::Deploy { device, login } => {
            not_set_up(paths)?;
            let name = device.name();
            with_session(&login, async |session| {
                if let Some(state) = session.resumable(paths)? {
                    return session.finish_deploy(paths, state, &name).await;
                }
                let found = session.find_servers().await?;
                if let Some(server) = found.first() {
                    return Err(format!(
                        "the account already has a Flarebox server, {} at {}; run flarebox server add-device",
                        server.script.id, server.url
                    ));
                }
                session.deploy(paths, &name).await
            })
            .await
        }
        ServerAction::AddDevice {
            server,
            device,
            login,
        } => {
            not_set_up(paths)?;
            let name = device.name();
            with_session(&login, async |session| {
                let found = session.pick(server.as_deref()).await?;
                session.add_device(paths, found, &name, false).await
            })
            .await
        }
        ServerAction::Upgrade { server, yes, login } => {
            with_session(&login, async |session| {
                let found = session.pick(server.as_deref()).await?;
                session.upgrade(&found, yes).await
            })
            .await
        }
        ServerAction::Uninstall {
            server,
            bucket,
            jurisdiction,
            yes,
            login,
        } => {
            with_session(&login, async |session| {
                session
                    .uninstall(
                        paths,
                        server.as_deref(),
                        bucket.as_deref(),
                        jurisdiction,
                        yes,
                    )
                    .await
            })
            .await
        }
    }
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
fn not_set_up(paths: &Paths) -> Result<(), String> {
    if paths.credential().exists() && !StateFile::new(paths).path().exists() {
        return Err(format!(
            "this device is already set up; its credential is {}",
            paths.credential().display()
        ));
    }
    Ok(())
}

fn report_paired(paths: &Paths, response: Response) -> Result<(), String> {
    match response {
        Response::Paired { account } => {
            println!(
                "This device is set up as {} ({}) with {}.\nThe credential is in {}.",
                account.device_name,
                account.device_id,
                account.server,
                paths.credential().display()
            );
            Ok(())
        }
        Response::Error { message } => Err(message),
        other => Err(format!("the daemon answered {other:?}")),
    }
}

/// Logs in, runs `work`, and revokes the grant however `work` ends: success, failure, Ctrl-C or SIGTERM.
async fn with_session(
    args: &LoginArgs,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), String>,
) -> Result<(), String> {
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    let stopped = async || {
        tokio::select! {
            _ = terminate.recv() => "stopped by SIGTERM",
            _ = interrupt.recv() => "stopped",
        }
    };
    guarded(Session::login(args), stopped, work).await
}

/// Runs `work` on the session `login` returns, then revokes its grant. The grant exists once `login` returns, so
/// everything after that, the account lookup included, ends with the revocation, also when `stopped` fires.
async fn guarded(
    login: impl Future<Output = Result<Session, String>>,
    mut stopped: impl AsyncFnMut() -> &'static str,
    work: impl AsyncFnOnce(&mut Session) -> Result<(), String>,
) -> Result<(), String> {
    let mut session = tokio::select! {
        session = login => session?,
        how = stopped() => return Err(format!("{how} before the login finished")),
    };
    let outcome = tokio::select! {
        outcome = async {
            session.pick_account().await?;
            work(&mut session).await
        } => outcome,
        how = stopped() => Err(format!("{how}; run the same command again to continue")),
    };
    session.end().await;
    outcome
}

fn prompt(question: &str) -> Result<String, String> {
    print!("{question}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|error| error.to_string())?;
    Ok(line.trim().to_owned())
}

/// Asks a yes-or-no question on a terminal. Without a terminal it refuses, naming `flag`.
fn confirm(question: &str, yes: bool, flag: &str) -> Result<(), String> {
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{question} Run again with {flag} to go on without a terminal."
        ));
    }
    match prompt(&format!("{question} [y/N] "))?.as_str() {
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

/// A Flarebox server found in the account.
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
    async fn fresh(&mut self) -> Result<Cloudflare, String>;
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
    async fn fresh(&mut self) -> Result<Cloudflare, String> {
        if self.grant.expires_within(TOKEN_MARGIN) {
            self.grant
                .refresh(&self.http)
                .await
                .map_err(|error| format!("{error}; run the command again to log in again"))?;
        }
        Ok(self.api())
    }
}

impl Session {
    /// Logs in with Cloudflare and returns once the code is exchanged for a grant, before any other call.
    async fn login(args: &LoginArgs) -> Result<Session, String> {
        let http = flarebox_cloudflare::http_client().map_err(|error| error.to_string())?;
        let loopback = if args.no_browser {
            None
        } else {
            Some(
                Loopback::bind(args.port)
                    .await
                    .map_err(|error| error.to_string())?,
            )
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
                    .map_err(|_| "the login did not finish within 10 minutes".to_owned())?
                    .map_err(|error| error.to_string())?
            }
            None => {
                println!(
                    "Open this page on any device, log in with Cloudflare and select Authorize:\n{page}"
                );
                println!(
                    "The browser then fails to load a page on 127.0.0.1. Copy that page's address."
                );
                let pasted = prompt("Address: ")?;
                let url = Url::parse(&pasted)
                    .map_err(|error| format!("that is not an address: {error}"))?;
                login.code_from(&url).map_err(|error| error.to_string())?
            }
        };
        let grant = login
            .exchange(&http, code)
            .await
            .map_err(|error| error.to_string())?;
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
    async fn pick_account(&mut self) -> Result<(), String> {
        let accounts = self
            .api()
            .accounts()
            .await
            .map_err(|error| format!("cannot list the account: {error}"))?;
        match <[Account; 1]>::try_from(accounts) {
            Ok([account]) => self.account = account,
            Err(accounts) => {
                return Err(format!(
                    "the login gave access to {} accounts; pick exactly one on the consent page",
                    accounts.len()
                ));
            }
        }
        println!("Logged in to the Cloudflare account {}.", self.account.name);
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
                        "flarebox: could not revoke the Cloudflare login ({error}). It stays active; its access token expires within an hour. Revoke it now under Manage OAuth authorizations in the Cloudflare dashboard."
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

    async fn subdomain(&mut self) -> Result<String, String> {
        if let Some(subdomain) = &self.subdomain {
            return Ok(subdomain.clone());
        }
        let subdomain = self
            .api()
            .workers_subdomain(self.account())
            .await
            .map_err(|error| format!("cannot read the account's workers.dev subdomain: {error}"))?;
        self.subdomain = Some(subdomain.clone());
        Ok(subdomain)
    }

    async fn worker_url(&mut self, script: &str) -> Result<Url, String> {
        let subdomain = self.subdomain().await?;
        Url::parse(&format!("https://{script}.{subdomain}.workers.dev/"))
            .map_err(|error| format!("{script} does not make a workers.dev address: {error}"))
    }

    /// Waits until R2 is enabled on the account. R2 cannot be enabled through the API, so the user completes the
    /// dashboard's checkout (section 4.1).
    async fn require_r2(&mut self) -> Result<(), String> {
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
                            "R2 is not enabled on this account. Flarebox keeps your files in R2."
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
                Err(error) => return Err(format!("cannot list the account's R2 buckets: {error}")),
            }
        }
    }

    /// The Flarebox servers in the account (section 6): scripts with the tag, and untagged scripts that bind the
    /// Flarebox Durable Object classes, each checked with `GET /v1/server`. A tagged script that binds a bucket but does
    /// not answer is a deploy that stopped halfway, and is kept without `info`.
    async fn find_servers(&mut self) -> Result<Vec<Found>, String> {
        let binding = embedded_bundle()?.manifest.r2_binding;
        let api = self.fresh().await?;
        let scripts = api
            .scripts(self.account())
            .await
            .map_err(|error| format!("cannot list the account's Workers: {error}"))?;
        let mut found = Vec::new();
        for script in scripts {
            let settings = api
                .script_settings(self.account(), &script.id)
                .await
                .map_err(|error| format!("cannot read the settings of {}: {error}", script.id))?;
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
    async fn pick(&mut self, name: Option<&str>) -> Result<Found, String> {
        let mut found = self.find_servers().await?;
        if let Some(name) = name {
            found.retain(|server| server.script.id == name);
        }
        match found.len() {
            0 => Err(match name {
                Some(name) => format!("the account has no Flarebox server named {name}"),
                None => "the account has no Flarebox server; run flarebox server deploy".into(),
            }),
            1 => Ok(found.remove(0)),
            _ => Err(format!(
                "the account has several Flarebox servers; name one with --server: {}",
                found
                    .iter()
                    .map(|server| server.script.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// The state file of an unfinished deploy in this account, if there is one.
    fn resumable(&self, paths: &Paths) -> Result<Option<SetupState>, String> {
        Ok(StateFile::new(paths)
            .load()?
            .filter(|state| state.account == self.account() && state.step != Step::Uninstalling))
    }

    /// `flarebox setup` after the login: finish an interrupted deploy, deploy a new server, or add this device.
    async fn setup(&mut self, paths: &Paths, device_name: &str) -> Result<(), String> {
        if let Some(state) = self.resumable(paths)? {
            println!(
                "Continuing the setup that stopped earlier ({}).",
                state.script
            );
            return self.finish_deploy(paths, state, device_name).await;
        }
        self.require_r2().await?;
        let mut found = self.find_servers().await?;
        let server = match found.len() {
            0 => return self.deploy(paths, device_name).await,
            1 => found.remove(0),
            _ => {
                println!("The account has several Flarebox servers:");
                for (index, server) in found.iter().enumerate() {
                    println!("  {}. {} at {}", index + 1, server.script.id, server.url);
                }
                let choice = prompt("Which one? ")?;
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

    /// Picks free names, then deploys and claims a new server (sections 4.3 and 4.4).
    async fn deploy(&mut self, paths: &Paths, device_name: &str) -> Result<(), String> {
        let bundle = embedded_bundle()?;
        self.require_r2().await?;
        let api = self.fresh().await?;
        let scripts: Vec<String> = api
            .scripts(self.account())
            .await
            .map_err(|error| format!("cannot list the account's Workers: {error}"))?
            .into_iter()
            .map(|script| script.id)
            .collect();
        let buckets = api
            .buckets(self.account(), Jurisdiction::Default)
            .await
            .map_err(|error| format!("cannot list the account's R2 buckets: {error}"))?;
        let base = &bundle.manifest.default_bucket;
        let state = SetupState {
            account: self.account().to_owned(),
            script: free_name(base, &scripts),
            bucket: free_name(base, &buckets),
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
    ) -> Result<(), String> {
        let file = StateFile::new(paths);
        let bundle = embedded_bundle()?;
        let mut key = None;
        if state.step < Step::BucketCreated {
            create_bucket(self, &file, &mut state, &bundle.manifest.default_bucket).await?;
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
                "Uploading Flarebox {} as the Worker {}...",
                bundle.manifest.version, state.script
            );
            let secret = Secret::random(32);
            state.secret_set = true;
            file.save(&state)?;
            let bucket = Bound {
                name: state.bucket.clone(),
                jurisdiction: state.jurisdiction,
            };
            self.upload(&bundle, &state.script, &bucket, None, Some(&secret))
                .await?;
            state.step = Step::ScriptUploaded;
            file.save(&state)?;
            key = Some(secret);
        }
        let url = self.worker_url(&state.script).await?;
        if state.step < Step::Serving {
            self.serve(&state.script, &url).await?;
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
        Claim::new(self, paths, &server, device_name, state, key)
            .run(Duration::from_secs(60))
            .await
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
    ) -> Result<(), String> {
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
                    "{} stopped halfway through its deploy; run flarebox setup to finish it",
                    server.script.id
                ));
            }
        };
        let mut window = Duration::from_secs(60);
        if !info.setup_attempts {
            let reason = format!(
                "{} runs Flarebox {}, which cannot make a retried claim end with one device. It needs an upgrade to {} first.",
                server.script.id,
                info.server_version,
                embedded_bundle()?.manifest.version
            );
            if !interactive {
                return Err(format!(
                    "{reason} Run flarebox server upgrade, then this command again."
                ));
            }
            if confirm(&format!("{reason} Upgrade it now?"), false, "--yes").is_err() {
                return Err(format!(
                    "{reason} Run flarebox server upgrade, then flarebox setup again."
                ));
            }
            self.upgrade(&server, false).await?;
            wait_for_setup_attempts(&server.url).await?;
            window = Duration::from_secs(330);
        }
        if !info.claimed {
            window = Duration::from_secs(330);
        }
        let state = match self
            .resumable(paths)?
            .filter(|state| state.script == server.script.id)
        {
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
        Claim::new(self, paths, &server, device_name, state, None)
            .run(window)
            .await
    }

    /// Turns on the Worker's workers.dev route and waits until the server answers there.
    async fn serve(&mut self, script: &str, url: &Url) -> Result<ServerInfo, String> {
        self.fresh()
            .await?
            .enable_workers_dev(self.account(), script)
            .await
            .map_err(|error| format!("cannot turn on workers.dev for {script}: {error}"))?;
        wait_until_serving(url, None).await
    }

    /// Uploads the embedded server over an existing one (section 5).
    async fn upgrade(&mut self, server: &Found, yes: bool) -> Result<(), String> {
        let bundle = embedded_bundle()?;
        let Some(info) = &server.info else {
            return Err(format!(
                "{} does not answer; run flarebox setup to finish its deploy",
                server.script.id
            ));
        };
        if newer(&info.server_version, &bundle.manifest.version) {
            return Err(format!(
                "{} runs Flarebox {}, newer than this CLI's {}; install a newer flarebox",
                server.script.id, info.server_version, bundle.manifest.version
            ));
        }
        if !server.script.tags.iter().any(|tag| tag == TAG) {
            confirm(
                &format!(
                    "{} was deployed with the deploy button. A push to the repository the button created deploys that repository's code again and replaces this upgrade. Update that repository too, or disconnect its builds in the dashboard. Upgrade now?",
                    server.script.id
                ),
                yes,
                "--yes",
            )?;
        }
        let bucket = server
            .bucket
            .clone()
            .ok_or_else(|| format!("{} binds no R2 bucket", server.script.id))?;
        println!(
            "Uploading Flarebox {} over {} {}...",
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
            "{} now runs Flarebox {}. Open sync sessions move to it within 5 minutes.",
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
    ) -> Result<(), String> {
        let api = self.fresh().await?;
        let jwt = api
            .upload_assets(self.account(), script, &bundle.assets)
            .await
            .map_err(|error| format!("cannot upload the static assets: {error}"))?;
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
            .map_err(|error| format!("cannot upload the Worker {script}: {error}"))?;
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
    ) -> Result<(), String> {
        let file = StateFile::new(paths);
        let unfinished = file
            .load()?
            .filter(|state| state.account == self.account() && state.step == Step::Uninstalling);
        if let (Some(state), None) = (unfinished, bucket) {
            println!(
                "Continuing the uninstall that stopped earlier ({}).",
                state.bucket
            );
            return uninstall::finish(self, &file, state).await;
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
        )?;
        file.save(&state)?;
        uninstall::finish(self, &file, state).await
    }
}

/// The embedded server bundle, read through [`Bundle::from_dir`] from a private copy, so the release's bundle passes the
/// same checks as one on disk.
fn embedded_bundle() -> Result<Bundle, String> {
    if SERVER_BUNDLE.is_empty() {
        return Err("this flarebox was built without a server bundle; install a release build to deploy or upgrade a server".into());
    }
    let broken =
        |error: &dyn std::fmt::Display| format!("this flarebox's server bundle is broken: {error}");
    let dir =
        tempfile::tempdir().map_err(|error| format!("cannot unpack the server bundle: {error}"))?;
    for (path, bytes) in SERVER_BUNDLE {
        let target = dir.path().join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| broken(&error))?;
        }
        std::fs::write(&target, bytes).map_err(|error| broken(&error))?;
    }
    Bundle::from_dir(dir.path()).map_err(|error| broken(&error))
}

/// Creates the bucket `state.bucket`, or moves to the next free name after `base` while the name belongs to someone
/// else, then records the step. Uninstall empties and deletes the bucket the state file names, so the deploy adopts a
/// bucket that already exists only when `created_by_this_deploy` shows it is its own.
async fn create_bucket(
    cloud: &mut impl Cloud,
    file: &StateFile,
    state: &mut SetupState,
    base: &str,
) -> Result<(), String> {
    if state.bucket_create_started_ms.is_none() {
        state.bucket_create_started_ms = Some(now_ms());
        file.save(state)?;
    }
    let account = cloud.account().to_owned();
    let mut refused = Vec::new();
    loop {
        println!("Creating the R2 bucket {}...", state.bucket);
        let api = cloud.fresh().await?;
        match api
            .create_bucket(&account, &state.bucket, state.jurisdiction)
            .await
        {
            Ok(()) => break,
            Err(error) if error.status() == Some(409) => {
                if created_by_this_deploy(cloud, state).await? {
                    break;
                }
            }
            Err(error) => {
                return Err(format!(
                    "cannot create the bucket {}: {error}",
                    state.bucket
                ));
            }
        }
        refused.push(state.bucket.clone());
        let mut taken = cloud
            .fresh()
            .await?
            .buckets(&account, state.jurisdiction)
            .await
            .map_err(|error| format!("cannot list the account's R2 buckets: {error}"))?;
        taken.extend(refused.iter().cloned());
        let free = free_name(base, &taken);
        println!(
            "The bucket {} belongs to someone else, so the server uses the bucket {free}.",
            state.bucket
        );
        state.bucket = free;
        file.save(state)?;
    }
    state.step = Step::BucketCreated;
    file.save(state)
}

/// How far this machine's clock may run ahead of R2's before a bucket this deploy created looks older than the deploy.
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

/// Whether the existing bucket `state.bucket` is one this deploy created before it could record that: R2 dates it
/// after the deploy's first create, no Worker binds it, and it holds no key outside `spaces/`.
async fn created_by_this_deploy(
    cloud: &mut impl Cloud,
    state: &SetupState,
) -> Result<bool, String> {
    let Some(started) = state.bucket_create_started_ms else {
        return Ok(false);
    };
    let account = cloud.account().to_owned();
    let found = match cloud
        .fresh()
        .await?
        .bucket(&account, &state.bucket, state.jurisdiction)
        .await
    {
        Ok(found) => found,
        Err(error) if error.status() == Some(404) => return Ok(false),
        Err(error) => {
            return Err(format!("cannot read the bucket {}: {error}", state.bucket));
        }
    };
    if !utc_ms(&found.creation_date).is_some_and(|created| created >= started - CLOCK_SKEW_MS)
        || found
            .jurisdiction
            .is_some_and(|other| other != state.jurisdiction)
    {
        return Ok(false);
    }
    if uninstall::binder(cloud, &state.bucket, state.jurisdiction)
        .await?
        .is_some()
    {
        return Ok(false);
    }
    Ok(uninstall::scan(cloud, &state.bucket, state.jurisdiction)
        .await?
        .is_some_and(|contents| contents.foreign.is_none()))
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// Milliseconds since the Unix epoch of an RFC 3339 time in UTC, such as R2's `2026-11-10T01:02:03.456Z`. Any other
/// form is `None`.
fn utc_ms(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    let number = |part: &str| -> Option<i64> {
        (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse().ok())
            .flatten()
    };
    let date: Vec<i64> = date.split('-').map(number).collect::<Option<_>>()?;
    let clock: Vec<i64> = clock.split(':').map(number).collect::<Option<_>>()?;
    let [year, month, day] = <[i64; 3]>::try_from(date).ok()?;
    let [hour, minute, second] = <[i64; 3]>::try_from(clock).ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let millis = match fraction {
        "" => 0,
        fraction => {
            number(fraction)?;
            number(&format!("{fraction:0<3}")[..3])?
        }
    };
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

/// Keeps `state.script` while no other application holds the name. A Worker under it that has the tag and binds
/// `state.bucket` is this deploy's own upload from before the state file recorded it, so the upload goes over it. Any
/// other Worker under the name belongs to someone else, so the deploy moves to the first free name after `base`.
async fn keep_script_name(
    cloud: &mut impl Cloud,
    state: &mut SetupState,
    binding: &str,
    base: &str,
) -> Result<(), String> {
    let api = cloud.fresh().await?;
    let account = cloud.account().to_owned();
    let scripts = api
        .scripts(&account)
        .await
        .map_err(|error| format!("cannot list the account's Workers: {error}"))?;
    let Some(current) = scripts.iter().find(|script| script.id == state.script) else {
        return Ok(());
    };
    if current.tags.iter().any(|tag| tag == TAG) {
        let settings = api
            .script_settings(&account, &state.script)
            .await
            .map_err(|error| format!("cannot read the settings of {}: {error}", state.script))?;
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

/// The Flarebox API of the server at `url`. Tests serve it over http on a loopback address.
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
async fn wait_until_serving(url: &Url, version: Option<&str>) -> Result<ServerInfo, String> {
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
            ));
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Polls until the Account DO reports `setup_attempts`, which can take up to 300 seconds after an upgrade.
async fn wait_for_setup_attempts(url: &Url) -> Result<(), String> {
    println!("Waiting for the server's storage to move to the new version (up to 5 minutes)...");
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
    use flarebox_proto::v1::DeviceCredentials;
    use serde_json::json;

    use super::fake::{self, ACCOUNT, Fake, Reply, cf, proto, revoked, tokens};
    use super::*;

    fn deployed(fake: &Fake) -> Found {
        Found {
            script: Script {
                id: "flarebox".into(),
                created_on: None,
                tags: vec![TAG.into()],
                migration_tag: None,
                has_assets: true,
            },
            bucket: Some(Bound {
                name: "flarebox".into(),
                jurisdiction: Jurisdiction::Default,
            }),
            url: fake.base.clone(),
            info: None,
        }
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
            outcome.unwrap_err(),
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
                json!([{ "id": "flarebox", "tags": [] }]),
                None,
                "flarebox-2",
            ),
            (
                json!([{ "id": "flarebox", "tags": [TAG] }, { "id": "flarebox-2", "tags": [] }]),
                Some(binds("photos")),
                "flarebox-3",
            ),
            (
                json!([{ "id": "flarebox", "tags": [TAG] }]),
                Some(binds("flarebox")),
                "flarebox",
            ),
            (json!([{ "id": "photos", "tags": [] }]), None, "flarebox"),
        ];
        for (scripts, settings, expected) in cases {
            let mut replies = vec![tokens(), cf("GET", "/workers/scripts", scripts)];
            replies.extend(
                settings.map(|settings| cf("GET", "/workers/scripts/flarebox/settings", settings)),
            );
            let fake = Fake::start(replies).await;
            let mut session = fake::session(&fake).await;
            let mut state = SetupState {
                account: ACCOUNT.into(),
                script: "flarebox".into(),
                bucket: "flarebox".into(),
                step: Step::BucketCreated,
                ..SetupState::default()
            };

            keep_script_name(&mut session, &mut state, "BUCKET", "flarebox")
                .await
                .unwrap();

            assert_eq!(state.script, expected);
        }
    }

    #[test]
    fn utc_times_read_as_milliseconds_since_the_epoch() {
        assert_eq!(utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(utc_ms("2026-11-10T01:02:03.456Z"), Some(1_794_272_523_456));
        assert_eq!(utc_ms("2026-11-10T01:02:03Z"), Some(1_794_272_523_000));
        assert_eq!(utc_ms("2026-11-10T01:02:03.4567Z"), Some(1_794_272_523_456));
        assert_eq!(utc_ms("2026-11-10T01:02:03.4Z"), Some(1_794_272_523_400));
        assert_eq!(utc_ms("2026-11-10T01:02:03+00:00"), None);
        assert_eq!(utc_ms("2026-11-10 01:02:03Z"), None);
        assert_eq!(utc_ms("2026-13-10T01:02:03Z"), None);
        assert_eq!(utc_ms("2026-11-10T01:02:03.4é5Z"), None);
    }

    #[tokio::test]
    async fn a_bucket_name_someone_else_holds_moves_the_deploy_to_the_next_name() {
        let conflict = Reply {
            method: "POST",
            path: format!("/client/v4/accounts/{ACCOUNT}/r2/buckets"),
            status: 409,
            content_type: "application/json",
            body: json!({ "success": false, "errors": [{ "code": 10004, "message": "The bucket you tried to create already exists." }], "result": null })
                .to_string()
                .into_bytes(),
        };
        let found = |date: &str| {
            cf(
                "GET",
                "/r2/buckets/flarebox",
                json!({ "name": "flarebox", "creation_date": date }),
            )
        };
        let objects = |key: &str| {
            Reply {
            method: "GET",
            path: format!("/client/v4/accounts/{ACCOUNT}/r2/buckets/flarebox/objects?per_page=1000"),
            status: 200,
            content_type: "application/json",
            body: json!({ "success": true, "errors": [], "result": [{ "key": key }], "result_info": { "cursor": "", "is_truncated": false } })
                .to_string()
                .into_bytes(),
        }
        };
        let no_workers = || cf("GET", "/workers/scripts", json!([]));
        let next_name = || {
            vec![
                cf(
                    "GET",
                    "/r2/buckets?per_page=1000",
                    json!({ "buckets": [{ "name": "flarebox" }] }),
                ),
                cf("POST", "/r2/buckets", json!({})),
            ]
        };
        let older = "2026-11-09T23:00:00.000Z";
        let newer = "2026-11-10T01:00:05.000Z";
        let cases = [
            (vec![found(older)], next_name(), "flarebox-2"),
            (
                vec![found(newer), no_workers(), objects("photos/cat.jpg")],
                next_name(),
                "flarebox-2",
            ),
            (
                vec![
                    found(newer),
                    cf(
                        "GET",
                        "/workers/scripts",
                        json!([{ "id": "photos", "tags": [] }]),
                    ),
                    cf(
                        "GET",
                        "/workers/scripts/photos/settings",
                        json!({ "bindings": [{ "type": "r2_bucket", "name": "PHOTOS", "bucket_name": "flarebox" }] }),
                    ),
                ],
                next_name(),
                "flarebox-2",
            ),
            (
                vec![found(newer), no_workers(), objects("spaces/0194f8a2/chunk")],
                Vec::new(),
                "flarebox",
            ),
        ];
        for (checks, after, expected) in cases {
            let mut replies = vec![tokens(), conflict.clone()];
            replies.extend(checks);
            replies.extend(after);
            let fake = Fake::start(replies).await;
            let mut session = fake::session(&fake).await;
            let dir = tempfile::tempdir().unwrap();
            let file = StateFile::new(&Paths::under(dir.path()));
            let mut state = SetupState {
                account: ACCOUNT.into(),
                script: "flarebox".into(),
                bucket: "flarebox".into(),
                bucket_create_started_ms: utc_ms("2026-11-10T01:00:00Z"),
                ..SetupState::default()
            };

            create_bucket(&mut session, &file, &mut state, "flarebox")
                .await
                .unwrap();

            assert_eq!(state.bucket, expected);
            assert_eq!(state.step, Step::BucketCreated);
            assert_eq!(file.load().unwrap(), Some(state));
        }
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
        let secrets = "/workers/scripts/flarebox/secrets";
        let fake = Fake::start(vec![
            tokens(),
            cf("POST", "/workers/scripts/flarebox/subdomain", json!(null)),
            proto("GET", "/v1/server", info),
            cf("GET", secrets, json!([])),
            cf("PUT", secrets, json!(null)),
            proto("POST", "/v1/claim", issued),
            cf(
                "DELETE",
                &format!("{secrets}/FLAREBOX_RECOVERY_KEY"),
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
            && seen.path.ends_with("/workers/scripts/flarebox/subdomain")));
        assert!(paths.credential().exists());
        assert_eq!(StateFile::new(&paths).load().unwrap(), None);
    }

    #[test]
    fn free_names_count_up_from_the_default() {
        let taken = vec!["flarebox".to_owned(), "flarebox-2".to_owned()];
        assert_eq!(free_name("flarebox", &[]), "flarebox");
        assert_eq!(free_name("flarebox", &taken), "flarebox-3");
    }

    #[test]
    fn the_embedded_bundle_reads_back_when_the_build_has_one() {
        match option_env!("FLAREBOX_SERVER_BUNDLE") {
            None => assert!(
                embedded_bundle()
                    .unwrap_err()
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
