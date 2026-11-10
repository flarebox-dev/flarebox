// SPDX-License-Identifier: Apache-2.0

//! The `flarebox` command: pairs this machine, links folders, shows status and questions, manages the account's devices,
//! shows what each space stores, and runs the sync daemon. Commands run in the daemon through its socket when it is
//! running, and on their own otherwise. The device and usage commands read only the credential and talk to the server
//! directly.

mod ipc;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use flarebox_driver::{
    Answer, Credential, Driver, DriverError, FolderStatus, Paths, Question, QuestionKind, Status,
};
use flarebox_net::{ApiError, Auth};
use flarebox_proto::v1::{DeviceInfo, SpaceInfo, SpaceUsage, device_info::Kind};
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
    /// List the devices paired with the server, or revoke a lost one.
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Show how many bytes each space stores: in live files, in the trash and in old versions.
    Usage {
        #[arg(long)]
        json: bool,
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

#[derive(Subcommand)]
enum DeviceAction {
    /// List the devices paired with the server, revoked ones included.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Revoke another device, such as a lost one. It stops syncing at once, and the files on it stay there.
    Revoke {
        /// The device's ID, as flarebox device list shows it.
        #[arg(value_parser = device_id)]
        id: String,
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
            Command::Device { action } => device(paths, action).await,
            Command::Usage { json } => usage(paths, json).await,
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
        Command::Daemon { .. } | Command::Device { .. } | Command::Usage { .. } => {
            unreachable!("the daemon, device and usage commands do not go through the socket")
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

/// This profile's credential, for the commands that talk to the server directly. They write nothing to the profile, so
/// they run the same whether or not the daemon holds it.
fn signed_in(paths: &Paths) -> Result<Auth, String> {
    let credential = Credential::load(&paths.credential())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| DriverError::NotPaired.to_string())?;
    Ok(Auth::new(
        credential.api().map_err(|error| error.to_string())?,
        credential.net(),
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
        DeviceAction::Revoke { id } => {
            if id == credential.device_id {
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

/// What `flarebox usage` prints on stdout: the spaces that answered, as text or as a JSON array. It prints nothing when
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
        Err("not a device ID; flarebox device list shows them".to_owned())
    }
}

fn refusal(error: ApiError) -> String {
    if error.is_unauthenticated() {
        return "the server refused this device's credential: another device revoked it, or the credential is wrong. Pair it again to sync.".to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0190a0b0-0000-7000-8000-000000000001";

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
