// SPDX-License-Identifier: Apache-2.0

//! The `koi` command: sets up a server or joins one, links folders, shows status and pending decisions, lists a folder's
//! trash and a file's versions, manages the account's devices, shows what each space stores, and runs the sync daemon.
//! Commands run in the daemon through its socket when it is running, and on their own otherwise. The device, usage,
//! trash and versions commands talk to the server directly with the credential.

mod history;
mod ipc;
mod notify;
mod pairing;
mod setup;

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand, ValueEnum};
use koidrive_driver::{
    Answer, Credential, Driver, DriverError, FolderStatus, Limit, MAX_DELETIONS, Paths, Question,
    QuestionKind, RecoveryGroup, SET_UP_AGAIN, Status,
};
use koidrive_net::{ApiError, Auth, Reload};
use koidrive_proto::v1::{
    CreateSpaceRequest, DeviceInfo, SpaceInfo, SpaceUsage, device_info::Kind,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::ipc::{Request, Response};

#[derive(Parser)]
#[command(name = "koi", version, about = "Koidrive sync client.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Set up Koidrive on this machine: join with a code from another device, or log in with Cloudflare to deploy a
    /// server to your account or add this device to the server there.
    Setup {
        /// A pairing code from another device, such as K7QM-2XPD@koidrive.alice.workers.dev.
        code: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
        #[command(flatten)]
        login: setup::LoginArgs,
    },
    /// Join a server with a pairing code from a device that is already paired.
    Join {
        /// The pairing code, such as K7QM-2XPD@koidrive.alice.workers.dev.
        code: String,
        /// The server's address, for a code without @host or to replace its host.
        #[arg(long)]
        server: Option<String>,
        #[command(flatten)]
        device: DeviceArgs,
    },
    /// Deploy, add this device to, upgrade or remove the Koidrive server in your Cloudflare account.
    Server {
        #[command(subcommand)]
        action: setup::ServerAction,
    },
    /// Create a space, or list the spaces of the server.
    Space {
        #[command(subcommand)]
        action: SpaceAction,
    },
    /// Link a folder to a space and start syncing it.
    Link {
        /// The folder to sync. It must exist and may already hold files.
        folder: PathBuf,
        /// The space's id or name.
        #[arg(long)]
        space: String,
    },
    /// Stop syncing a folder. Its files stay where they are.
    Unlink {
        /// The linked folder.
        folder: PathBuf,
    },
    /// Sync a linked folder again from the start, after fixing the problem that halted it.
    Restart {
        /// The linked folder.
        folder: PathBuf,
    },
    /// Sync a folder again whose changes the server refused, after fixing the cause the status names.
    #[command(group = clap::ArgGroup::new("how").required(true).args(["resend", "replan"]))]
    Lift {
        /// The linked folder whose changes the server refused.
        folder: PathBuf,
        /// Send the refused changes again as they were, for a cause fixed on the server.
        #[arg(long)]
        resend: bool,
        /// Plan the refused changes again into a new commit, for a cause fixed on this device.
        #[arg(long)]
        replan: bool,
    },
    /// Show this device and its linked folders.
    Status {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// List the decisions sync is waiting on, or make one.
    Resolve {
        /// The decision to make, as koi resolve lists it.
        #[arg(requires = "answer")]
        id: Option<String>,
        /// One of the answers the decision lists.
        #[arg(value_parser = |text: &str| text.parse::<Answer>())]
        answer: Option<Answer>,
        /// Print the list of decisions as JSON instead of text. Making a decision always prints text.
        #[arg(long, conflicts_with = "id")]
        json: bool,
    },
    /// List the devices paired with the server, or revoke a lost one.
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// List what was deleted in a linked folder and is still in its space's trash.
    Trash {
        /// A linked folder, or a folder inside one.
        #[arg(default_value = ".")]
        folder: PathBuf,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// List the versions the server keeps of a file in a linked folder, newest first.
    Versions {
        /// A file in a linked folder.
        file: PathBuf,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Bring a deleted file or folder back from its space's trash to where it was, or make an earlier version of a
    /// file current again. The daemon sends the request with the folder's next commit.
    Restore {
        /// The deleted file or folder at the path it had, or the file whose version to restore.
        path: PathBuf,
        /// Restore this version of the file, as koi versions lists it.
        #[arg(long)]
        version: Option<String>,
        /// The deleted node to restore, as koi trash --json lists it, when several deletions had this path.
        #[arg(long, conflicts_with = "version")]
        node: Option<String>,
    },
    /// List the restores asked for in a linked folder and how each one went.
    Restores {
        /// A linked folder, or a folder inside one.
        #[arg(default_value = ".")]
        folder: PathBuf,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Show how many bytes each space stores: in live files, in the trash, in old versions and in unreferenced chunks.
    Usage {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Run the sync daemon in the foreground until SIGTERM or SIGINT.
    Daemon {
        #[arg(long, value_enum, default_value_t = LogFormat::Text)]
        log_format: LogFormat,
    },
}

impl Command {
    /// Whether the command ends with the pending-decision banner on stderr. `status` and `resolve` show the decisions
    /// themselves, the daemon never ends, and JSON output carries decisions as data, without decoration.
    fn shows_banner(&self) -> bool {
        match self {
            Command::Daemon { .. } | Command::Status { .. } | Command::Resolve { .. } => false,
            Command::Trash { json, .. }
            | Command::Versions { json, .. }
            | Command::Restores { json, .. }
            | Command::Usage { json }
            | Command::Space {
                action: SpaceAction::List { json },
            }
            | Command::Device {
                action: DeviceAction::List { json },
            } => !json,
            _ => true,
        }
    }
}

#[derive(Subcommand)]
enum SpaceAction {
    /// Create a space. Link a folder to it with koi link.
    Create {
        /// The space's name, at most 255 bytes.
        #[arg(value_parser = short_name)]
        name: String,
    },
    /// List the spaces of the server.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DeviceAction {
    /// Show a pairing code that adds another device to this server. It works once, for 10 minutes.
    Add,
    /// List the devices paired with the server, revoked ones included.
    List {
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
    /// Revoke another device, such as a lost one. It stops syncing at once, and the files on it stay there.
    Revoke {
        /// The device's ID, as koi device list shows it.
        #[arg(value_parser = device_id)]
        id: String,
    },
}

#[derive(clap::Args)]
pub struct DeviceArgs {
    /// A name for this device, shown on the others, at most 255 bytes. Defaults to this machine's host name.
    #[arg(long, value_parser = short_name)]
    name: Option<String>,
}

impl DeviceArgs {
    pub fn name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            let host = rustix::system::uname()
                .nodename()
                .to_string_lossy()
                .into_owned();
            host.split('.').next().unwrap_or_default().to_owned()
        })
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        println!("koi {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    };
    let log_format = match &command {
        Command::Daemon { log_format } => Some(*log_format),
        _ => None,
    };
    init_logging(log_format);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return fail(format!("cannot start: {error}")),
    };
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => return fail(error.to_string()),
    };
    let shows_banner = command.shows_banner();
    let banner_paths = paths.clone();
    let (outcome, banner) = runtime.block_on(async move {
        let outcome = match command {
            Command::Daemon { .. } => daemon(paths).await,
            Command::Device { action } => device(paths, action).await,
            Command::Space { action } => space(paths, action).await,
            Command::Usage { json } => usage(paths, json).await,
            Command::Trash { folder, json } => history::trash(&paths, &folder, json).await,
            Command::Versions { file, json } => history::versions(&paths, &file, json).await,
            Command::Restore {
                path,
                version,
                node,
            } => history::restore(&paths, &path, version, node).await,
            Command::Restores { folder, json } => history::restores(&paths, &folder, json).await,
            Command::Setup {
                code: None,
                device,
                login,
            } => setup::setup(&paths, &device.name(), &login).await,
            command @ Command::Server {
                action: setup::ServerAction::Claim { .. },
            } => client(paths, command).await,
            Command::Server { action } => setup::server(&paths, action).await,
            command => client(paths, command).await,
        };
        let banner = match shows_banner {
            true => pending_banner(&banner_paths).await,
            false => None,
        };
        (outcome, banner)
    });
    let code = match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(message),
    };
    if let Some(banner) = banner {
        eprintln!("{banner}");
    }
    code
}

fn fail(message: String) -> ExitCode {
    eprintln!("koi: {message}");
    ExitCode::FAILURE
}

/// Logs go to stderr: structured key-value text by default, one JSON object per line with `--log-format json`. The
/// level comes from `KOIDRIVE_LOG` (`info` for the daemon, `warn` for other commands). No log line carries a
/// credential, an access token or a pairing code.
fn init_logging(daemon: Option<LogFormat>) {
    let default = if daemon.is_some() { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_env("KOIDRIVE_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_writer(std::io::stderr);
    match daemon {
        Some(LogFormat::Json) => builder.json().with_current_span(false).init(),
        _ => builder.init(),
    }
}

async fn daemon(paths: Paths) -> Result<(), String> {
    let driver = Driver::open(paths).map_err(|error| match error {
        DriverError::Busy(_) => {
            "another Koidrive daemon or command is using this profile".to_owned()
        }
        other => other.to_string(),
    })?;
    let socket = driver.paths().socket();
    let listener = ipc::bind(&socket)
        .map_err(|error| format!("cannot listen on {}: {error}", socket.display()))?;
    let mut terminate = signal(SignalKind::terminate()).map_err(|error| error.to_string())?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|error| error.to_string())?;
    driver.start();
    let server = tokio::spawn(ipc::serve(listener, driver.clone()));
    let notifications = tokio::spawn(notify::decisions(driver.clone()));
    tracing::info!(
        socket = %socket.display(),
        version = env!("CARGO_PKG_VERSION"),
        folders = driver.status().folders.len(),
        "the daemon is running"
    );
    let signal = tokio::select! {
        _ = terminate.recv() => "SIGTERM",
        _ = interrupt.recv() => "SIGINT",
    };
    tracing::info!(signal, "stopping the daemon");
    server.abort();
    notifications.abort();
    let _ = std::fs::remove_file(&socket);
    driver.stop().await;
    tracing::info!("the daemon stopped");
    Ok(())
}

async fn client(paths: Paths, command: Command) -> Result<(), String> {
    let (request, json) = match command {
        Command::Setup {
            code: Some(code),
            device,
            ..
        } => join_request(&code, None, &device)?,
        Command::Join {
            code,
            server,
            device,
        } => join_request(&code, server.as_deref(), &device)?,
        Command::Server {
            action:
                setup::ServerAction::Claim {
                    server,
                    code,
                    user,
                    device,
                },
        } => (
            Request::Claim {
                server: pairing::server_url(&server),
                code: code.trim().to_owned(),
                user_name: user,
                name: device.name(),
            },
            false,
        ),
        Command::Link { folder, space } => (
            Request::Link {
                folder: absolute(&folder)?,
                space,
            },
            false,
        ),
        Command::Unlink { folder } => (
            Request::Unlink {
                folder: absolute(&folder)?,
            },
            false,
        ),
        Command::Restart { folder } => (
            Request::Restart {
                folder: absolute(&folder)?,
            },
            false,
        ),
        Command::Lift { folder, replan, .. } => (
            Request::Lift {
                folder: absolute(&folder)?,
                replan,
            },
            false,
        ),
        Command::Status { json } => (Request::Status, json),
        Command::Resolve {
            id: Some(id),
            answer: Some(answer),
            ..
        } => (
            Request::Answer {
                question_id: id,
                answer,
            },
            false,
        ),
        Command::Resolve { json, .. } => (Request::Questions, json),
        Command::Daemon { .. }
        | Command::Device { .. }
        | Command::Usage { .. }
        | Command::Space { .. }
        | Command::Trash { .. }
        | Command::Versions { .. }
        | Command::Restore { .. }
        | Command::Restores { .. }
        | Command::Setup { code: None, .. }
        | Command::Server { .. } => {
            unreachable!("main runs these commands itself")
        }
    };
    let credential = paths.credential();
    match ipc::run(&paths, request).await? {
        Response::Error { message } => Err(message),
        Response::Paired { account } => {
            println!(
                "Paired as {} ({}) with {}.\nThe credential is in {}.",
                account.device_name,
                account.device_id,
                account.server,
                credential.display()
            );
            Ok(())
        }
        Response::Linked { folder } => {
            println!(
                "Linked {} to space {} ({}).",
                folder.path, folder.space_name, folder.space_id
            );
            if folder.state == koidrive_driver::FolderState::Stopped {
                println!("Run koi daemon to sync it.");
            }
            Ok(())
        }
        Response::Unlinked { space_id, path } => {
            println!(
                "Unlinked {} from space {space_id}. Its files and its .koidrive folder stay.",
                path.display()
            );
            Ok(())
        }
        Response::Restarted { folder } => match folder.state {
            koidrive_driver::FolderState::Stopped => {
                println!("Run koi daemon to sync {}.", folder.path);
                Ok(())
            }
            koidrive_driver::FolderState::Halted => Err(format!(
                "{} cannot sync: {}",
                folder.path,
                folder.problem.unwrap_or_default()
            )),
            _ => {
                println!("Syncing {} again.", folder.path);
                Ok(())
            }
        },
        Response::Status { status } if json => print_json(&status),
        Response::Status { status } => {
            let questions = match status.folders.iter().any(|folder| folder.questions > 0) {
                true => match ipc::run(&paths, Request::Questions).await? {
                    Response::Questions { questions } => questions,
                    _ => Vec::new(),
                },
                false => Vec::new(),
            };
            print_status(&status, &questions);
            Ok(())
        }
        Response::Questions { questions } if json => {
            let questions: Vec<serde_json::Value> = questions
                .iter()
                .map(|question| {
                    let mut value = serde_json::to_value(question).expect("a question serializes");
                    value["answers"] =
                        serde_json::to_value(question.kind.answers()).expect("answers serialize");
                    value
                })
                .collect();
            print_json(&questions)
        }
        Response::Questions { questions } => {
            print_questions(&questions);
            Ok(())
        }
        Response::Answered => {
            println!("Resolved.");
            Ok(())
        }
        Response::SignedOut | Response::Restore { .. } | Response::Restores { .. } => {
            Err("the daemon answered a request that this command did not send".into())
        }
    }
}

/// This profile's credential, for the commands that talk to the server directly. They write nothing to the profile, so
/// they run the same whether or not the daemon holds it.
fn signed_in(paths: &Paths) -> Result<Auth, String> {
    let credential = Credential::load(&paths.credential())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| DriverError::NotPaired.to_string())?;
    reading(paths, &credential)
}

/// An [`Auth`] for a command that only reads the profile's credential. The process that holds the profile may rotate
/// the credential meanwhile, so when the server refuses it, it reads both credential files again, once, and presents
/// what they hold (RFC 0011 section 2). A reload that finds a credential for another server fails, so that no
/// credential reaches a server it was not issued by.
fn reading(paths: &Paths, credential: &Credential) -> Result<Auth, String> {
    let path = paths.credential();
    let server = credential.server.clone();
    let reload: Reload = Arc::new(move || {
        let stored = Credential::load_stored(&path)
            .map_err(io::Error::other)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, DriverError::NotPaired.to_string())
            })?;
        if stored.credential.server != server {
            return Err(io::Error::other(
                "the profile now holds a credential for another server",
            ));
        }
        Ok(stored.net())
    });
    Ok(Auth::reloading(
        credential.api().map_err(|error| error.to_string())?,
        credential.net(),
        reload,
    ))
}

/// Runs a device command (RFC 0009).
async fn device(paths: Paths, action: DeviceAction) -> Result<(), String> {
    let auth = signed_in(&paths)?;
    match action {
        DeviceAction::List { json } => {
            let mut devices = Vec::new();
            let mut cursor = String::new();
            loop {
                let page = auth
                    .api()
                    .list_devices(&auth, &cursor)
                    .await
                    .map_err(refusal)?;
                devices.extend(page.devices);
                if page.next_cursor.is_empty() {
                    break;
                }
                cursor = page.next_cursor;
            }
            if json {
                return print_json(&devices.iter().map(device_json).collect::<Vec<_>>());
            }
            print!("{}", device_table(&devices, now_ms()));
            Ok(())
        }
        DeviceAction::Add => {
            let started = auth.api().pair_start(&auth).await.map_err(refusal)?;
            let minutes = ((started.expires_at_ms - now_ms() + 59_999) / 60_000).max(1);
            println!(
                "On the new device, run:\n  koi join {}\nThe code works once, for about {minutes} minutes.",
                pairing::join_args(&started.code, auth.api().base())
            );
            Ok(())
        }
        DeviceAction::Revoke { id } => {
            if id == auth.credentials().device_id {
                return Err(
                    "this device cannot revoke itself; revoke it from another device".to_owned(),
                );
            }
            auth.api()
                .revoke_device(&auth, &id)
                .await
                .map_err(refusal)?;
            println!(
                "Revoked device {id}. It can no longer sync; the files already on it stay there."
            );
            Ok(())
        }
    }
}

/// Runs a space command. Creating refuses a name another space has, since koi link names a space by its name.
async fn space(paths: Paths, action: SpaceAction) -> Result<(), String> {
    let auth = signed_in(&paths)?;
    let spaces = auth.api().list_spaces(&auth).await.map_err(refusal)?.spaces;
    match action {
        SpaceAction::List { json } => {
            if json {
                return print_json(&spaces.iter().map(space_json).collect::<Vec<_>>());
            }
            print!("{}", space_table(&spaces));
            Ok(())
        }
        SpaceAction::Create { name } => {
            let name = name.trim();
            if name.is_empty() {
                return Err("a space needs a name".to_owned());
            }
            if let Some(taken) = spaces.iter().find(|space| space.name == name) {
                return Err(format!(
                    "the space {} already has that name; link a folder to it, or pick another name",
                    printable(&taken.space_id)
                ));
            }
            let created = auth
                .api()
                .create_space(
                    &auth,
                    &CreateSpaceRequest {
                        name: name.to_owned(),
                    },
                )
                .await
                .map_err(refusal)?;
            println!(
                "Created the space {} ({}).\nLink a folder to it with koi link <folder> --space {}.",
                printable(&created.name),
                created.space_id,
                created.space_id
            );
            Ok(())
        }
    }
}

fn space_json(space: &SpaceInfo) -> serde_json::Value {
    serde_json::json!({
        "space_id": space.space_id,
        "name": space.name,
        "created_at_ms": space.created_at_ms,
    })
}

/// One line per space: its ID and its name.
fn space_table(spaces: &[SpaceInfo]) -> String {
    if spaces.is_empty() {
        return "No spaces. Create one with koi space create <name>.\n".to_owned();
    }
    let mut table = format!("{:<36}  NAME\n", "ID");
    for space in spaces {
        table.push_str(&format!(
            "{:<36}  {}\n",
            printable(&space.space_id),
            printable(&space.name)
        ));
    }
    table
}

/// Shows what each space of the account stores (RFC 0010). A space the server refuses, such as one a rewind holds
/// closed, does not hide the others: they are printed, and the command then fails naming the refused ones on stderr.
async fn usage(paths: Paths, json: bool) -> Result<(), String> {
    let auth = signed_in(&paths)?;
    let spaces = auth.api().list_spaces(&auth).await.map_err(refusal)?.spaces;
    let mut usages = Vec::new();
    let mut refused = Vec::new();
    for space in spaces {
        match auth.api().space_usage(&auth, &space.space_id).await {
            Ok(usage) => usages.push((space, usage)),
            Err(error) if error.is_unauthenticated() => return Err(refusal(error)),
            Err(error) => refused.push(format!(
                "{} ({}): {error}",
                printable(&space.name),
                space.space_id
            )),
        }
    }
    print!("{}", usage_output(&usages, refused.len(), json));
    if refused.is_empty() {
        Ok(())
    } else {
        Err(format!("no usage for {}", refused.join("; ")))
    }
}

/// What `koi usage` prints on stdout: the spaces that answered, as text or as a JSON array. It prints nothing when
/// every space refused, so that stdout never shows an empty account that is not empty; the error names them instead.
fn usage_output(usages: &[(SpaceInfo, SpaceUsage)], refused: usize, json: bool) -> String {
    if usages.is_empty() && refused > 0 {
        return String::new();
    }
    if !json {
        return usage_report(usages);
    }
    let entries: Vec<_> = usages
        .iter()
        .map(|(space, usage)| usage_json(space, usage))
        .collect();
    serde_json::to_string_pretty(&entries).expect("usage serializes") + "\n"
}

fn usage_json(space: &SpaceInfo, usage: &SpaceUsage) -> serde_json::Value {
    serde_json::json!({
        "space_id": space.space_id,
        "name": space.name,
        "live_files": usage.live_files,
        "live_bytes": usage.live_bytes,
        "trashed_files": usage.trashed_files,
        "trash_bytes": usage.trash_bytes,
        "old_versions": usage.old_versions,
        "old_version_bytes": usage.old_version_bytes,
        "unreferenced_bytes": usage.unreferenced_bytes,
        "total_bytes": total_bytes(usage),
    })
}

fn total_bytes(usage: &SpaceUsage) -> u64 {
    usage.live_bytes + usage.trash_bytes + usage.old_version_bytes + usage.unreferenced_bytes
}

/// One block per space: its live files, trash, old versions and unreferenced chunks, then a note on how they count.
fn usage_report(usages: &[(SpaceInfo, SpaceUsage)]) -> String {
    if usages.is_empty() {
        return "No spaces.\n".to_owned();
    }
    let mut report = String::new();
    for (space, usage) in usages {
        let rows = [
            (
                "Live files",
                count(usage.live_files, "file"),
                usage.live_bytes,
            ),
            (
                "Trash",
                count(usage.trashed_files, "file"),
                usage.trash_bytes,
            ),
            (
                "Old versions",
                count(usage.old_versions, "version"),
                usage.old_version_bytes,
            ),
            ("Unreferenced", String::new(), usage.unreferenced_bytes),
            ("Total", String::new(), total_bytes(usage)),
        ];
        report.push_str(&format!(
            "{} ({})\n",
            printable(&space.name),
            space.space_id
        ));
        for (label, what, bytes) in rows {
            report.push_str(&format!("  {label:<14}{what:>14}  {:>9}\n", size(bytes)));
        }
    }
    report.push_str(
        "\nEach stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
         Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n",
    );
    report
}

fn count(n: u64, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// Bytes in decimal units, such as 999 B, 1.0 kB or 1.5 GB.
fn size(bytes: u64) -> String {
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    for unit in ["kB", "MB", "GB", "TB"] {
        value /= 1000.0;
        if value < 999.95 {
            return format!("{value:.1} {unit}");
        }
    }
    format!("{:.1} PB", value / 1000.0)
}

/// A device ID as the server issues it: a lowercase UUID. Checked here so that the argument cannot name another path.
/// A space, device or user name as the server takes it: at most 255 bytes of UTF-8, measured before trimming (RFC 0007
/// section 1). Checking it here names the argument instead of sending a request the server refuses.
fn short_name(text: &str) -> Result<String, String> {
    const MAX_NAME: usize = 255;
    if text.len() > MAX_NAME {
        return Err(format!(
            "a name is at most {MAX_NAME} bytes of UTF-8, and this one has {}",
            text.len()
        ));
    }
    Ok(text.to_owned())
}

fn device_id(text: &str) -> Result<String, String> {
    let id = text.to_ascii_lowercase();
    let shaped = id.len() == 36
        && id.char_indices().all(|(at, c)| match at {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if shaped {
        Ok(id)
    } else {
        Err("not a device ID; koi device list shows them".to_owned())
    }
}

fn refusal(error: ApiError) -> String {
    if error.is_unauthenticated() {
        return format!(
            "the server refused this device's credential, so another device revoked it or the credential is wrong. To \
             use this device again, {SET_UP_AGAIN}."
        );
    }
    error.to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn device_json(device: &DeviceInfo) -> serde_json::Value {
    serde_json::json!({
        "device_id": device.device_id,
        "name": device.name,
        "platform": device.platform,
        "kind": match device.kind() {
            Kind::Device => "device",
            Kind::Agent => "agent",
            Kind::Unspecified => "unspecified",
        },
        "created_at_ms": device.created_at_ms,
        "last_seen_ms": device.last_seen_ms,
        "revoked_at_ms": device.revoked_at_ms,
        "credential_expires_at_ms": device.credential_expires_at_ms,
        "issued_by": device.issued_by,
        "current": device.current,
    })
}

/// One line per device: its ID, name, platform, when it last refreshed a token, and whether it is this device, active
/// or revoked.
fn device_table(devices: &[DeviceInfo], now_ms: i64) -> String {
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|device| {
            let state = match (device.revoked_at_ms, device.current) {
                (0, true) => "this device".to_owned(),
                (0, false) => "active".to_owned(),
                (revoked, _) => format!("revoked {}", ago(revoked, now_ms)),
            };
            [
                printable(&device.device_id),
                printable(&device.name),
                printable(&device.platform),
                ago(device.last_seen_ms, now_ms),
                state,
            ]
        })
        .collect();
    let header = ["ID", "NAME", "PLATFORM", "LAST SEEN", "STATE"].map(str::to_owned);
    let mut widths = [0; 5];
    for row in std::iter::once(&header).chain(&rows) {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut table = String::new();
    for row in std::iter::once(&header).chain(&rows) {
        let cells: Vec<String> = row
            .iter()
            .zip(widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        table.push_str(cells.join("  ").trim_end());
        table.push('\n');
    }
    table
}

/// The text with control characters and bidirectional formatting characters escaped, such as `\u{1b}`. Another device
/// chose its name and platform, and a terminal would run the escape sequences in them or show forged rows.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}' => {
                c.escape_unicode().to_string()
            }
            c if c.is_control() => c.escape_unicode().to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// How long before `now_ms` a time in Unix milliseconds was, or "never" for 0.
fn ago(ms: i64, now_ms: i64) -> String {
    if ms == 0 {
        return "never".to_owned();
    }
    let minutes = (now_ms - ms).max(0) / 60_000;
    match minutes {
        0 => "just now".to_owned(),
        1..60 => format!("{minutes} min ago"),
        60..2880 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

fn join_request(
    code: &str,
    server: Option<&str>,
    device: &DeviceArgs,
) -> Result<(Request, bool), String> {
    let invitation = pairing::parse(code, server)?;
    Ok((
        Request::Pair {
            server: invitation.server,
            code: invitation.code,
            name: device.name(),
        },
        false,
    ))
}

/// The folder as an absolute path, since the daemon does not share this command's working directory.
fn absolute(folder: &Path) -> Result<PathBuf, String> {
    std::path::absolute(folder).map_err(|error| format!("{}: {error}", folder.display()))
}

fn print_json(value: &impl serde::Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    println!("{text}");
    Ok(())
}

fn print_status(status: &Status, questions: &[Question]) {
    match &status.account {
        Some(account) => println!(
            "Device: {} ({}) on {}",
            account.device_name, account.device_id, account.server
        ),
        None => println!("Device: not set up; run koi setup"),
    }
    println!(
        "Daemon: {}",
        if status.running {
            "running"
        } else {
            "not running"
        }
    );
    if status.folders.is_empty() {
        println!("No linked folders; run koi link.");
    }
    for folder in &status.folders {
        print_folder(folder);
        for question in (questions.iter()).filter(|question| question.space_id == folder.space_id) {
            print!("{}", question_brief(question));
        }
    }
}

fn print_folder(folder: &FolderStatus) {
    let seq = match (folder.last_seq, folder.head_seq) {
        (Some(last), Some(head)) => format!("seq {last} of {head}"),
        (Some(last), None) => format!("seq {last}"),
        _ => "not read yet".to_owned(),
    };
    println!(
        "{}  space {} ({})  {:?}  {seq}, {} remote nodes{}, {}",
        folder.path,
        folder.space_name,
        folder.space_id,
        folder.state,
        folder.remote_nodes,
        folder
            .local_entries
            .map(|entries| format!(", {entries} local entries"))
            .unwrap_or_default(),
        count(folder.questions, "pending decision")
    );
    if let Some(problem) = &folder.problem {
        println!("  {problem}");
    }
}

fn print_questions(questions: &[Question]) {
    if questions.is_empty() {
        println!("No decisions are waiting.");
    }
    for question in questions {
        print!("{}", question_text(question));
    }
}

/// The command that lists and answers decisions, as every message names it.
const RESOLVE: &str = "koi resolve";

/// The most items `koi resolve` lists for one decision.
const ITEMS_SHOWN: usize = 20;

/// What a decision is about, in one line.
fn question_summary(question: &Question) -> String {
    let why = |exceeded: &[Limit]| {
        let limits: Vec<String> = (exceeded.iter())
            .map(|limit| match limit {
                Limit::Deletions => format!("more than {MAX_DELETIONS} deletions in 10 minutes"),
                Limit::Share => "more than half of the space in 10 minutes".to_owned(),
            })
            .collect();
        match &limits[..] {
            [] => String::new(),
            limits => format!(", {}", limits.join(" and ")),
        }
    };
    match &question.kind {
        QuestionKind::MassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "held {deletions} deletions that would remove {affected_files} of {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::RemoteMassDelete {
            deletions,
            affected_files,
            space_files,
            exceeded,
        } => format!(
            "applied {deletions} deletions from other devices that removed {affected_files} of \
             {space_files} files{}",
            why(exceeded)
        ),
        QuestionKind::AmbiguousDisappearance => {
            "files disappeared while changes were not tracked".to_owned()
        }
        QuestionKind::Recovery { group } => match group {
            RecoveryGroup::CreatedAfterRestorePoint => {
                "files created after the restore point, which the restore removed from the server"
            }
            RecoveryGroup::ChangedOnThisDevice => {
                "files changed on this device since it last synced"
            }
            RecoveryGroup::CreatedOnThisDevice => {
                "files on this device that the space does not hold"
            }
            RecoveryGroup::DeletedOnThisDevice => {
                "files deleted on this device that the server still holds"
            }
            RecoveryGroup::RequestedBeforeReset => {
                "restores asked for before the reset, which the server does not show"
            }
        }
        .to_owned(),
    }
}

/// The first path a decision holds, after its summary, or nothing for a decision that holds none.
fn first_path(question: &Question) -> String {
    (question.items.first())
        .map(|item| format!("; first path: {}", printable(&item.path)))
        .unwrap_or_default()
}

/// Each answer of a decision: what it does, and under it the full command that gives it.
fn resolve_commands(question: &Question, indent: &str) -> String {
    let id = printable(&question.id);
    (question.kind.answers().iter())
        .map(|answer| {
            format!(
                "{indent}{}:\n{indent}  {RESOLVE} {id} {}\n",
                answer_effect(*answer),
                answer.as_str()
            )
        })
        .collect()
}

/// What an answer does.
fn answer_effect(answer: Answer) -> &'static str {
    match answer {
        Answer::Delete => "Delete these files on the server and other devices",
        Answer::Restore => "Download these files to this device again",
        Answer::Keep => "Put these files back on the server",
        Answer::Discard => "Move this device's copies to the local trash and follow the server",
        Answer::Publish => "Send this device's version to the server",
        Answer::Repeat => "Send the request again",
        Answer::Cancel => "Drop the request",
        Answer::Dismiss => "Close this notice",
    }
}

/// How `koi status` shows a decision under its folder: what it is about, its first path, and how to make it.
fn question_brief(question: &Question) -> String {
    format!(
        "  {}: {}{}\n{}",
        printable(&question.id),
        question_summary(question),
        first_path(question),
        resolve_commands(question, "  ")
    )
}

/// How `koi resolve` shows a decision: what it is about, how to make it, and its first [`ITEMS_SHOWN`] items.
fn question_text(question: &Question) -> String {
    let mut text = format!(
        "{}  space {}  {}\n{}",
        printable(&question.id),
        printable(&question.space_id),
        question_summary(question),
        resolve_commands(question, "  ")
    );
    for item in question.items.iter().take(ITEMS_SHOWN) {
        text += &format!(
            "  {} ({})\n",
            printable(&item.path),
            count(item.files, "file")
        );
    }
    let more = question.items.len().saturating_sub(ITEMS_SHOWN);
    if more > 0 {
        text += &format!("  and {more} more; {RESOLVE} --json lists them all\n");
    }
    text
}

/// The text of a desktop notification for a decision: what it is about, its first path, and the command to run.
fn question_notice(question: &Question) -> String {
    format!(
        "{}{}. Run {RESOLVE}.",
        question_summary(question),
        first_path(question)
    )
}

/// The line every command but `status`, `resolve` and `daemon` prints on stderr while decisions are pending.
fn banner(questions: &[Question]) -> Option<String> {
    let n = questions.len() as u64;
    let verb = if n == 1 { "is" } else { "are" };
    (n > 0).then(|| {
        format!(
            "koi: {} {verb} waiting; run {RESOLVE}",
            count(n, "sync decision")
        )
    })
}

/// The banner for the decisions pending in the profile, asked of the daemon. Without a daemon there is none, because
/// a command run on its own leaves the profile as it found it.
async fn pending_banner(paths: &Paths) -> Option<String> {
    match ipc::ask(paths, &Request::Questions).await {
        Ok(Some(Response::Questions { questions })) => banner(&questions),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0190a0b0-0000-7000-8000-000000000001";

    #[tokio::test]
    async fn a_reload_refuses_a_credential_for_another_server() {
        use crate::setup::fake::{Fake, Reply};
        use prost::Message;

        let refused = || Reply {
            method: "POST",
            path: "/v1/token".into(),
            status: 401,
            content_type: "application/x-protobuf",
            body: koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
                message: "unknown device or refresh credential".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
        let other_server = Fake::start(Vec::new()).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let first = Credential {
            server: first_server.base.to_string(),
            user_id: "same-user".into(),
            user_name: None,
            device_id: ID.into(),
            device_name: "same-device".into(),
            refresh_credential: "first-credential".into(),
        };
        first.save_new(&paths.credential()).unwrap();
        let reader = reading(&paths, &first).unwrap();
        let owner = Driver::open(paths.clone()).unwrap();
        owner.sign_out_device(ID).await.unwrap();
        let replacement = Credential {
            server: other_server.base.to_string(),
            refresh_credential: "other-server-credential".into(),
            ..first
        };
        owner.save_credential(replacement.clone()).await.unwrap();

        assert!(matches!(
            reader.access_token().await,
            Err(ApiError::Credential(_))
        ));
        let presented: Vec<String> = first_server
            .seen()
            .iter()
            .map(|seen| {
                koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                    .unwrap()
                    .refresh_credential
            })
            .collect();
        assert!(
            !presented.contains(&replacement.refresh_credential),
            "{presented:?}"
        );
    }

    #[tokio::test]
    async fn a_reload_never_presents_another_profiles_credential_to_the_first_server() {
        use crate::setup::fake::{Fake, Reply};
        use prost::Message;

        let refused = || Reply {
            method: "POST",
            path: "/v1/token".into(),
            status: 401,
            content_type: "application/x-protobuf",
            body: koidrive_proto::v1::Error {
                code: koidrive_proto::v1::error::Code::Unauthenticated as i32,
                message: "unknown device or refresh credential".into(),
                ..Default::default()
            }
            .encode_to_vec(),
        };
        let first_server = Fake::start(vec![refused(), refused(), refused()]).await;
        let other_server = Fake::start(Vec::new()).await;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        let first = Credential {
            server: first_server.base.to_string(),
            user_id: "first-user".into(),
            user_name: None,
            device_id: ID.into(),
            device_name: "first".into(),
            refresh_credential: "first-credential".into(),
        };
        first.save_new(&paths.credential()).unwrap();
        let reader = reading(&paths, &first).unwrap();
        let owner = Driver::open(paths.clone()).unwrap();
        owner.sign_out_device(ID).await.unwrap();
        let replacement = Credential {
            server: other_server.base.to_string(),
            user_id: "other-user".into(),
            user_name: None,
            device_id: "0190a0b0-0000-7000-8000-000000000002".into(),
            device_name: "other".into(),
            refresh_credential: "other-credential".into(),
        };
        owner.save_credential(replacement.clone()).await.unwrap();

        assert!(matches!(
            reader.access_token().await,
            Err(ApiError::Credential(_))
        ));
        let presented: Vec<String> = first_server
            .seen()
            .iter()
            .map(|seen| {
                koidrive_proto::v1::TokenRequest::decode(seen.body.as_slice())
                    .unwrap()
                    .refresh_credential
            })
            .collect();
        assert!(
            !presented.contains(&replacement.refresh_credential),
            "{presented:?}"
        );
    }

    fn held(
        (deletions, affected_files, space_files): (u64, u64, u64),
        exceeded: &[Limit],
        items: usize,
    ) -> Question {
        Question {
            id: "q1".into(),
            lineage: "q1".into(),
            space_id: "s1".into(),
            asked_at_ms: 1,
            kind: QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
                exceeded: exceeded.to_vec(),
            },
            items: (0..items)
                .map(|i| koidrive_driver::HeldItem {
                    node_id: format!("n{i}"),
                    path: format!("old/{i:02}"),
                    files: 1,
                })
                .collect(),
        }
    }

    #[test]
    fn a_held_path_is_shown_with_its_control_characters_escaped() {
        let mut question = held((1, 15, 20), &[Limit::Share], 1);
        question.items[0].path = "old/a\nforged line\u{1b}[2J".into();
        for text in [question_text(&question), question_brief(&question)] {
            assert_eq!(
                text.lines().count(),
                6 - text.starts_with(' ') as usize,
                "{text}"
            );
            assert!(!text.contains('\u{1b}'), "{text}");
        }
    }

    #[test]
    fn a_mass_delete_question_names_the_limit_it_crossed_and_lists_at_most_20_items() {
        let text = question_text(&held((1_001, 1_001, 5_000), &[Limit::Deletions], 25));
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[..6],
            [
                "q1  space s1  held 1001 deletions that would remove 1001 of 5000 files, more than 1000 deletions in 10 minutes",
                "  Delete these files on the server and other devices:",
                "    koi resolve q1 delete",
                "  Download these files to this device again:",
                "    koi resolve q1 restore",
                "  old/00 (1 file)",
            ]
        );
        assert_eq!(lines.len(), 5 + ITEMS_SHOWN + 1);
        assert_eq!(lines[24], "  old/19 (1 file)");
        assert_eq!(lines[25], "  and 5 more; koi resolve --json lists them all");

        let text = question_text(&held((1, 15, 20), &[Limit::Share], 1));
        assert_eq!(
            text.lines().next(),
            Some(
                "q1  space s1  held 1 deletions that would remove 15 of 20 files, more than half of the space in 10 minutes"
            )
        );
        assert_eq!(text.lines().count(), 6);
    }

    #[test]
    fn status_shows_a_decision_under_its_folder_with_its_first_path_and_one_full_command_per_answer()
     {
        assert_eq!(
            question_brief(&held((30, 30, 40), &[Limit::Share], 3)),
            [
                "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00",
                "  Delete these files on the server and other devices:",
                "    koi resolve q1 delete",
                "  Download these files to this device again:",
                "    koi resolve q1 restore",
                "",
            ]
            .join("\n")
        );
        assert_eq!(
            question_brief(&held((30, 30, 40), &[Limit::Share], 0))
                .lines()
                .next(),
            Some(
                "  q1: held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes"
            )
        );
    }

    #[test]
    fn json_output_and_the_commands_that_show_decisions_get_no_banner() {
        let shows = |args: &[&str]| {
            Cli::try_parse_from(args)
                .unwrap()
                .command
                .unwrap()
                .shows_banner()
        };
        assert!(shows(&["koi", "trash", "."]));
        assert!(shows(&["koi", "device", "list"]));
        assert!(shows(&["koi", "link", "dir", "--space", "s"]));
        for args in [
            &["koi", "trash", ".", "--json"][..],
            &["koi", "device", "list", "--json"],
            &["koi", "space", "list", "--json"],
            &["koi", "usage", "--json"],
            &["koi", "status"],
            &["koi", "resolve"],
            &["koi", "daemon"],
        ] {
            assert!(!shows(args), "{args:?}");
        }
    }

    #[test]
    fn the_banner_counts_the_pending_decisions_and_names_resolve() {
        assert_eq!(banner(&[]), None);
        assert_eq!(
            banner(&[held((30, 30, 40), &[Limit::Share], 1)]).as_deref(),
            Some("koi: 1 sync decision is waiting; run koi resolve")
        );
        assert_eq!(
            banner(&[
                held((30, 30, 40), &[Limit::Share], 1),
                held((2_000, 2_000, 9_000), &[Limit::Deletions], 1)
            ])
            .as_deref(),
            Some("koi: 2 sync decisions are waiting; run koi resolve")
        );
    }

    #[test]
    fn a_notification_names_the_decision_its_first_path_and_resolve() {
        assert_eq!(
            question_notice(&held((30, 30, 40), &[Limit::Share], 2)),
            "held 30 deletions that would remove 30 of 40 files, more than half of the space in 10 minutes; first path: old/00. Run koi resolve."
        );
    }

    #[test]
    fn resolve_lists_with_no_arguments_and_answers_with_an_id_and_an_answer() {
        let parse = |args: &[&str]| Cli::try_parse_from(args).map(|cli| cli.command);
        assert!(matches!(
            parse(&["koi", "resolve"]),
            Ok(Some(Command::Resolve {
                id: None,
                answer: None,
                json: false
            }))
        ));
        assert!(matches!(
            parse(&["koi", "resolve", "q1", "restore"]),
            Ok(Some(Command::Resolve {
                id: Some(_),
                answer: Some(Answer::Restore),
                ..
            }))
        ));
        assert!(parse(&["koi", "resolve", "q1"]).is_err());
        assert!(parse(&["koi", "resolve", "q1", "later"]).is_err());
        assert!(parse(&["koi", "resolve", "q1", "delete", "--json"]).is_err());
        assert!(parse(&["koi", "questions"]).is_err());
    }

    #[tokio::test]
    async fn a_build_without_the_server_bundle_stops_before_the_cloudflare_login() {
        if option_env!("KOIDRIVE_SERVER_BUNDLE").is_some() {
            return;
        }
        for args in [
            vec!["koi", "server", "deploy", "--no-browser"],
            vec!["koi", "server", "add-device", "--no-browser"],
            vec!["koi", "server", "upgrade", "--no-browser"],
            vec!["koi", "server", "uninstall", "--no-browser"],
        ] {
            let Some(Command::Server { action }) = Cli::try_parse_from(&args).unwrap().command
            else {
                unreachable!("{args:?} is a server command");
            };
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::under(dir.path());
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                setup::server(&paths, action),
            )
            .await
            .unwrap_or_else(|_| panic!("{args:?} waited for a login"));
            assert!(
                outcome
                    .unwrap_err()
                    .contains("built without a server bundle"),
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_question_names_every_limit_crossed_and_a_notice_takes_dismiss() {
        let both = [Limit::Deletions, Limit::Share];
        let text = question_text(&held((1_001, 3_000, 5_000), &both, 1));
        assert_eq!(
            text.lines().next(),
            Some(
                "q1  space s1  held 1001 deletions that would remove 3000 of 5000 files, more than 1000 deletions in 10 minutes and more than half of the space in 10 minutes"
            )
        );

        let notice = Question {
            kind: QuestionKind::RemoteMassDelete {
                deletions: 1,
                affected_files: 15,
                space_files: 20,
                exceeded: vec![Limit::Share],
            },
            ..held((1, 15, 20), &[Limit::Share], 0)
        };
        let text = question_text(&notice);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "q1  space s1  applied 1 deletions from other devices that removed 15 of 20 files, more than half of the space in 10 minutes",
                "  Close this notice:",
                "    koi resolve q1 dismiss",
            ]
        );
    }

    #[test]
    fn a_first_link_question_reads_as_a_sentence_with_its_answers() {
        let mut question = held((0, 0, 0), &[], 1);
        question.kind = QuestionKind::Recovery {
            group: RecoveryGroup::CreatedOnThisDevice,
        };
        let text = question_text(&question);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "q1  space s1  files on this device that the space does not hold",
                "  Send this device's version to the server:",
                "    koi resolve q1 publish",
                "  Move this device's copies to the local trash and follow the server:",
                "    koi resolve q1 discard",
                "  old/00 (1 file)",
            ]
        );
    }

    #[test]
    fn a_name_longer_than_255_bytes_is_refused_before_anything_is_sent() {
        let longest = "가".repeat(85);
        let too_long = format!("{longest}a");
        for args in [
            vec!["koi", "space", "create", &too_long],
            vec![
                "koi",
                "join",
                "K7QM-2XPD@files.example.com",
                "--name",
                &too_long,
            ],
            vec![
                "koi",
                "server",
                "claim",
                "files.example.com",
                "ABCD",
                "--user",
                &too_long,
            ],
        ] {
            let error = Cli::try_parse_from(&args)
                .err()
                .expect("a 256-byte name is refused");
            assert!(
                error
                    .to_string()
                    .contains("a name is at most 255 bytes of UTF-8, and this one has 256"),
                "{error}"
            );
        }
        assert!(Cli::try_parse_from(["koi", "space", "create", &longest]).is_ok());
    }

    #[test]
    fn a_device_id_is_a_uuid_and_nothing_that_could_name_another_path() {
        assert_eq!(device_id(ID).as_deref(), Ok(ID));
        assert_eq!(device_id(&ID.to_uppercase()).as_deref(), Ok(ID));
        for text in [
            "",
            "../spaces",
            "0190a0b0-0000-7000-8000-00000000000",
            &format!("{ID}/x"),
            "0190a0b0x0000-7000-8000-000000000001",
        ] {
            assert!(device_id(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn names_and_platforms_cannot_send_terminal_controls_or_forge_rows() {
        let forged = DeviceInfo {
            device_id: ID.into(),
            name: "\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}".into(),
            platform: "linux\nFORGED ROW\u{9b}".into(),
            ..Default::default()
        };
        let table = device_table(&[forged], 0);
        assert_eq!(table.lines().count(), 2, "{table}");
        assert!(
            !table.chars().any(|c| c.is_control() && c != '\n'),
            "{table:?}"
        );
        assert!(
            table.contains(r"\u{1b}[2J\u{1b}[Hsafe-looking\u{202e}  linux\u{a}FORGED ROW\u{9b}"),
            "{table}"
        );
        assert_eq!(
            device_json(&DeviceInfo {
                name: "a\nb".into(),
                ..Default::default()
            })["name"],
            "a\nb"
        );
    }

    #[test]
    fn times_read_as_how_long_ago() {
        let now = 1_000 * 86_400_000;
        assert_eq!(ago(0, now), "never");
        assert_eq!(ago(now - 59_000, now), "just now");
        assert_eq!(ago(now + 5_000, now), "just now");
        assert_eq!(ago(now - 5 * 60_000, now), "5 min ago");
        assert_eq!(ago(now - 47 * 3_600_000, now), "47 h ago");
        assert_eq!(ago(now - 3 * 86_400_000, now), "3 days ago");
    }

    #[test]
    fn sizes_read_in_decimal_units() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(999), "999 B");
        assert_eq!(size(1_000), "1.0 kB");
        assert_eq!(size(999_949), "999.9 kB");
        assert_eq!(size(999_950), "1.0 MB");
        assert_eq!(size(1_500_000_000), "1.5 GB");
        assert_eq!(size(u64::MAX), "18446.7 PB");
    }

    #[test]
    fn the_usage_report_lists_each_space_and_its_total() {
        let space = SpaceInfo {
            space_id: ID.into(),
            name: "Home\u{1b}[2J".into(),
            ..Default::default()
        };
        let usage = SpaceUsage {
            live_files: 1_204,
            live_bytes: 3_200_000_000,
            trashed_files: 1,
            trash_bytes: 40_100_000,
            old_versions: 30,
            old_version_bytes: 12_000_000,
            unreferenced_bytes: 1_500_000,
        };
        assert_eq!(
            usage_report(&[(space.clone(), usage)]),
            format!(
                "Home\\u{{1b}}[2J ({ID})\n\
                 \x20 Live files        1204 files     3.2 GB\n\
                 \x20 Trash                 1 file    40.1 MB\n\
                 \x20 Old versions     30 versions    12.0 MB\n\
                 \x20 Unreferenced                     1.5 MB\n\
                 \x20 Total                            3.3 GB\n\
                 \n\
                 Each stored chunk counts once, under the first of live files, trash and old versions that holds it.\n\
                 Unreferenced chunks are uploads not committed yet and chunks waiting out the grace period before deletion.\n"
            )
        );
        assert_eq!(usage_report(&[]), "No spaces.\n");
        let json = usage_json(&space, &usage);
        assert_eq!(json["total_bytes"], 3_253_600_000u64);
        assert_eq!(json["trashed_files"], 1);
    }

    #[test]
    fn the_space_list_shows_each_space_by_id_and_name() {
        let spaces = [SpaceInfo {
            space_id: ID.into(),
            name: "Notes\u{1b}[2J".into(),
            ..SpaceInfo::default()
        }];
        assert_eq!(
            space_table(&spaces),
            format!("ID                                    NAME\n{ID}  Notes\\u{{1b}}[2J\n")
        );
        assert_eq!(
            space_table(&[]),
            "No spaces. Create one with koi space create <name>.\n"
        );
    }

    #[test]
    fn usage_output_shows_only_the_spaces_that_answered() {
        let answered = [(
            SpaceInfo {
                space_id: ID.into(),
                name: "Home".into(),
                ..Default::default()
            },
            SpaceUsage {
                live_files: 2,
                live_bytes: 300,
                ..Default::default()
            },
        )];
        let parse = |text: String| -> serde_json::Value { serde_json::from_str(&text).unwrap() };

        assert_eq!(usage_output(&[], 0, false), "No spaces.\n");
        assert_eq!(
            usage_output(&[], 0, true),
            "[]\n",
            "an account with no spaces"
        );

        assert_eq!(usage_output(&[], 2, false), "", "every space refused");
        assert_eq!(usage_output(&[], 2, true), "", "every space refused");

        let partial = parse(usage_output(&answered, 1, true));
        assert_eq!(partial.as_array().unwrap().len(), 1, "{partial:#}");
        assert_eq!(partial[0]["space_id"], ID);
        assert_eq!(partial[0]["live_bytes"], 300);
        assert!(usage_output(&answered, 1, false).starts_with(&format!("Home ({ID})\n")));
        assert_eq!(
            usage_output(&answered, 1, true),
            usage_output(&answered, 0, true),
            "a refused space adds nothing to the JSON"
        );
    }

    #[test]
    fn the_table_marks_this_device_and_revoked_ones() {
        let now = 1_000 * 86_400_000;
        let device = |name: &str, current, last_seen_ms, revoked_at_ms| DeviceInfo {
            device_id: ID.into(),
            name: name.into(),
            platform: "linux".into(),
            kind: Kind::Device as i32,
            last_seen_ms,
            revoked_at_ms,
            current,
            ..Default::default()
        };
        let table = device_table(
            &[
                device("desk", true, now - 120_000, 0),
                device("lost laptop", false, 0, now - 2 * 86_400_000),
                device("phone", false, now - 7_200_000, 0),
            ],
            now,
        );
        let id = ID;
        assert_eq!(
            table,
            format!(
                "ID                                    NAME         PLATFORM  LAST SEEN  STATE\n\
                 {id}  desk         linux     2 min ago  this device\n\
                 {id}  lost laptop  linux     never      revoked 2 days ago\n\
                 {id}  phone        linux     2 h ago    active\n"
            )
        );
    }
}
