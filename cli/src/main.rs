// SPDX-License-Identifier: Apache-2.0

//! The `flarebox` command: pairs this machine, links folders, shows status and questions, and runs the sync daemon.
//! Commands run in the daemon through its socket when it is running, and on their own otherwise.

mod ipc;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use flarebox_driver::{
    Answer, Driver, DriverError, FolderStatus, Paths, Question, QuestionKind, Status,
};
use tokio::signal::unix::{SignalKind, signal};
use tracing_subscriber::EnvFilter;

use crate::ipc::{Request, Response};

#[derive(Parser)]
#[command(
    name = "flarebox",
    version,
    about = "Flarebox sync client. Not affiliated with Cloudflare, Inc."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Pair this machine with a server, using a code from a device that is already paired.
    Pair {
        /// The server's address, such as https://files.example.com.
        #[arg(long)]
        server: String,
        /// The pairing code shown on the paired device.
        #[arg(long)]
        code: String,
        /// A name for this device, shown on the others.
        #[arg(long)]
        name: String,
    },
    /// Link a folder to a space and start syncing it.
    Link {
        folder: PathBuf,
        /// The space's id or name.
        #[arg(long)]
        space: String,
    },
    /// Stop syncing a folder. Its files stay where they are.
    Unlink { folder: PathBuf },
    /// Show this device and its linked folders.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// List the questions sync is waiting on, or answer one.
    Questions {
        #[arg(long)]
        json: bool,
        #[command(subcommand)]
        action: Option<QuestionAction>,
    },
    /// Run the sync daemon in the foreground until SIGTERM or SIGINT.
    Daemon {
        #[arg(long, value_enum, default_value_t = LogFormat::Text)]
        log_format: LogFormat,
    },
}

#[derive(Subcommand)]
enum QuestionAction {
    /// Answer a question with one of the answers it lists.
    Answer {
        id: String,
        #[arg(value_parser = |text: &str| text.parse::<Answer>())]
        answer: Answer,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Text,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        println!("flarebox {}", env!("CARGO_PKG_VERSION"));
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
    let outcome = runtime.block_on(async move {
        match command {
            Command::Daemon { .. } => daemon(paths).await,
            command => client(paths, command).await,
        }
    });
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(message),
    }
}

fn fail(message: String) -> ExitCode {
    eprintln!("flarebox: {message}");
    ExitCode::FAILURE
}

/// Logs go to stderr: structured key-value text by default, one JSON object per line with `--log-format json`. The
/// level comes from `FLAREBOX_LOG` (`info` for the daemon, `warn` for other commands). No log line carries a
/// credential, an access token or a pairing code.
fn init_logging(daemon: Option<LogFormat>) {
    let default = if daemon.is_some() { "info" } else { "warn" };
    let filter =
        EnvFilter::try_from_env("FLAREBOX_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match daemon {
        Some(LogFormat::Json) => builder.json().with_current_span(false).init(),
        _ => builder.init(),
    }
}

async fn daemon(paths: Paths) -> Result<(), String> {
    let driver = Driver::open(paths).map_err(|error| match error {
        DriverError::Busy(_) => {
            "another Flarebox daemon or command is using this profile".to_owned()
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
    let _ = std::fs::remove_file(&socket);
    driver.stop().await;
    tracing::info!("the daemon stopped");
    Ok(())
}

async fn client(paths: Paths, command: Command) -> Result<(), String> {
    let (request, json) = match command {
        Command::Pair { server, code, name } => (Request::Pair { server, code, name }, false),
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
        Command::Status { json } => (Request::Status, json),
        Command::Questions { json, action: None } => (Request::Questions, json),
        Command::Questions {
            action: Some(QuestionAction::Answer { id, answer }),
            ..
        } => (
            Request::Answer {
                question_id: id,
                answer,
            },
            false,
        ),
        Command::Daemon { .. } => unreachable!("the daemon is not a client command"),
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
            if folder.state == flarebox_driver::FolderState::Stopped {
                println!("Run flarebox daemon to sync it.");
            }
            Ok(())
        }
        Response::Unlinked { space_id, path } => {
            println!(
                "Unlinked {} from space {space_id}. Its files and its .flarebox folder stay.",
                path.display()
            );
            Ok(())
        }
        Response::Status { status } if json => print_json(&status),
        Response::Status { status } => {
            print_status(&status);
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
            println!("Answered.");
            Ok(())
        }
    }
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

fn print_status(status: &Status) {
    match &status.account {
        Some(account) => println!(
            "Device: {} ({}) on {}",
            account.device_name, account.device_id, account.server
        ),
        None => println!("Device: not paired; run flarebox pair"),
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
        println!("No linked folders; run flarebox link.");
    }
    for folder in &status.folders {
        print_folder(folder);
    }
}

fn print_folder(folder: &FolderStatus) {
    let seq = match (folder.last_seq, folder.head_seq) {
        (Some(last), Some(head)) => format!("seq {last} of {head}"),
        (Some(last), None) => format!("seq {last}"),
        _ => "not read yet".to_owned(),
    };
    println!(
        "{}  space {} ({})  {:?}  {seq}, {} remote nodes{}, {} questions",
        folder.path,
        folder.space_name,
        folder.space_id,
        folder.state,
        folder.remote_nodes,
        folder
            .local_entries
            .map(|entries| format!(", {entries} local entries"))
            .unwrap_or_default(),
        folder.questions
    );
    if let Some(problem) = &folder.problem {
        println!("  {problem}");
    }
}

fn print_questions(questions: &[Question]) {
    if questions.is_empty() {
        println!("No questions.");
    }
    for question in questions {
        let what = match &question.kind {
            QuestionKind::MassDelete {
                deletions,
                affected_files,
                space_files,
            } => format!(
                "{deletions} deletions would remove {affected_files} of {space_files} files"
            ),
            QuestionKind::AmbiguousDisappearance => {
                "files disappeared while changes were not tracked".to_owned()
            }
            QuestionKind::Recovery { group } => format!("recovery: {group:?}"),
        };
        let [first, second] = question.kind.answers();
        println!(
            "{}  space {}  {what}\n  answer with: flarebox questions answer {} {first}|{second}",
            question.id, question.space_id, question.id
        );
        for item in &question.items {
            println!("  {} ({} files)", item.path, item.files);
        }
    }
}
