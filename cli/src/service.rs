// SPDX-License-Identifier: Apache-2.0

//! The background service that runs `koi _daemon` for this profile: a systemd user unit on Linux and a launch agent on
//! macOS (docs/guide/install.md#background-sync). The commands that link or unlink a folder or pair this device, and
//! `koi service start`, bring it in line with the profile through [`reconcile`]: installed and running while the
//! profile links a folder, removed once it links none. Only `koi service start` loads a launch agent that the person
//! switched off in Login Items. `background = false` in config.toml's `[service]` table leaves it alone.

use std::fs;
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use koidrive_driver::{Config, Paths};
use serde::{Deserialize, Serialize};

use crate::ipc::{self, Request, Response};

/// The unit that koi fills in with this profile's binary and directories.
const UNIT: &str = include_str!("../systemd/koidrive.service");

/// How long a command waits for a service it started to answer on the socket.
const STARTUP: Duration = Duration::from_secs(10);

/// How long a started service may take to show as running. launchd spawns a job a moment after `launchctl bootstrap`
/// returns, so the first read can still find it not running.
const SPAWN: Duration = Duration::from_secs(5);

/// One profile's service: what the unit or the launch agent runs, and the names they go by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// A hash of the state directory, for a profile outside the default directories, so that each profile has a
    /// service of its own.
    suffix: Option<String>,
    binary: String,
    home: PathBuf,
    state_dir: PathBuf,
    /// The XDG variables `koi _daemon` needs to open this profile and its socket, with `None` to unset one.
    env: Vec<(&'static str, Option<String>)>,
}

impl Service {
    /// The service of the profile at `paths`, run by `binary`. Paths that a unit or a property list cannot carry
    /// exactly, because they are not UTF-8 or hold control characters, are refused.
    pub fn new(paths: &Paths, home: &Path, binary: &Path) -> Result<Self, String> {
        let text = |path: &Path| match path.to_str() {
            Some(text) if !text.chars().any(char::is_control) => Ok(text.to_owned()),
            _ => Err(format!(
                "{} cannot be written into a service file",
                path.display()
            )),
        };
        let parent = |path: &Path| path.parent().map(Path::to_path_buf).unwrap_or_default();
        let config_home = parent(&parent(&paths.config_file));
        let state_home = parent(&paths.state_dir);
        let runtime = match paths.runtime_dir == paths.state_dir {
            true => None,
            false => Some(text(&parent(&paths.runtime_dir))?),
        };
        let default = paths.state_dir == home.join(".local/state/koidrive")
            && paths.config_file == home.join(".config/koidrive/config.toml");
        let suffix = (!default).then(|| {
            let hash = (paths.state_dir.as_os_str().as_bytes().iter())
                .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
                });
            format!("{:08x}", hash >> 32)
        });
        Ok(Service {
            suffix,
            binary: text(binary)?,
            home: home.to_path_buf(),
            state_dir: paths.state_dir.clone(),
            env: vec![
                ("XDG_CONFIG_HOME", Some(text(&config_home)?)),
                ("XDG_STATE_HOME", Some(text(&state_home)?)),
                ("XDG_RUNTIME_DIR", runtime),
            ],
        })
    }

    pub fn unit_name(&self) -> String {
        match &self.suffix {
            None => "koidrive.service".to_owned(),
            Some(suffix) => format!("koidrive-{suffix}.service"),
        }
    }

    pub fn label(&self) -> String {
        match &self.suffix {
            None => "dev.koidrive.koi".to_owned(),
            Some(suffix) => format!("dev.koidrive.koi.{suffix}"),
        }
    }

    /// The unit lives in the profile's state directory, and `systemctl --user enable` links it into the unit path.
    fn unit_file(&self) -> PathBuf {
        self.state_dir.join("systemd").join(self.unit_name())
    }

    fn plist_file(&self) -> PathBuf {
        (self.home.join("Library/LaunchAgents")).join(format!("{}.plist", self.label()))
    }

    pub fn log_file(&self) -> PathBuf {
        (self.home.join("Library/Logs/Koidrive")).join(format!("{}.log", self.label()))
    }

    /// The systemd user unit: cli/systemd/koidrive.service with this binary and this profile's directories.
    pub fn unit(&self) -> String {
        let mut unit = String::new();
        for line in UNIT.lines() {
            match line.strip_prefix("ExecStart=") {
                Some(_) => unit.push_str(&format!(
                    "ExecStart={} _daemon\n",
                    systemd_quoted(&self.binary)
                )),
                None => unit.push_str(&format!("{line}\n")),
            }
            if line == "[Service]" {
                for (name, value) in &self.env {
                    unit.push_str(&match value {
                        Some(value) => format!(
                            "Environment={}\n",
                            systemd_quoted(&format!("{name}={value}"))
                        ),
                        None => format!("UnsetEnvironment={name}\n"),
                    });
                }
            }
        }
        unit
    }

    /// The launch agent's property list. A plist cannot unset a variable that launchd's own environment sets, so an
    /// unset one is written empty, which the daemon reads as unset.
    pub fn plist(&self) -> String {
        let mut env = String::from("\t\t<key>KOIDRIVE_LOG</key>\n\t\t<string>info</string>\n");
        for (name, value) in &self.env {
            env.push_str(&format!(
                "\t\t<key>{name}</key>\n\t\t<string>{}</string>\n",
                xml(value.as_deref().unwrap_or_default())
            ));
        }
        let log = xml(&self.log_file().display().to_string());
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{label}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
		<string>_daemon</string>
	</array>
	<key>EnvironmentVariables</key>
	<dict>
{env}	</dict>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#,
            label = self.label(),
            binary = xml(&self.binary),
        )
    }

    /// Where the service's log is.
    fn log(&self, manager: &str) -> String {
        match manager {
            "launchd" => self.log_file().display().to_string(),
            _ => format!("journalctl --user -u {}", self.unit_name()),
        }
    }
}

/// A quoted word of a unit file: `%` starts a specifier, and quotes and backslashes need escapes.
fn systemd_quoted(text: &str) -> String {
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// What the service manager reports for a service, or what keeps koi from using it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    /// Installed, but not running: it failed.
    Stopped,
    /// The launch agent is installed but not loaded: the person switched koi off in Login Items, which unloads it.
    SwitchedOff,
    NotInstalled,
    /// This machine has no service manager koi can use, such as a Linux system without a systemd user session.
    Unavailable,
    /// config.toml turns the service off.
    Off,
}

/// A service manager. [`Manager::state`] reports `Running`, `Stopped` or `NotInstalled`, or why it cannot be used.
pub trait Manager {
    fn kind(&self) -> &'static str;
    fn name(&self, service: &Service) -> String;
    fn state(&self, service: &Service) -> Result<State, String>;
    /// Installs the service unless the installed one is the same, and starts it, or restarts it when it changed.
    fn start(&self, service: &Service) -> Result<(), String>;
    /// Stops and removes the service. Removing a service that is not installed changes nothing.
    fn remove(&self, service: &Service) -> Result<(), String>;
    /// Whether the service keeps running without a login session, where that is a setting.
    fn lingers(&self) -> Option<bool> {
        None
    }
    fn linger(&self) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Start,
    Remove,
    Leave,
}

/// What the service needs while the profile links `folders` folders. Starting is idempotent, so a running service is
/// started again: that installs a changed unit, for example after the binary moved. A launch agent switched off in
/// Login Items starts only when the person `asked` for it with `koi service start`.
pub fn plan(folders: usize, state: State, asked: bool) -> Action {
    match (folders, state) {
        (_, State::Unavailable | State::Off) => Action::Leave,
        (0, State::NotInstalled) => Action::Leave,
        (0, _) => Action::Remove,
        (_, State::SwitchedOff) if !asked => Action::Leave,
        _ => Action::Start,
    }
}

/// A folder that the daemon may not read, as its `access` request finds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blocked {
    pub folder: String,
    /// The read still waits after two seconds, usually because macOS is asking the person. Otherwise macOS refused it.
    pub waiting: bool,
}

/// The service as a command left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub manager: &'static str,
    pub name: String,
    pub state: State,
    /// Why the service cannot run, or did not change as it should.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lingers: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocked: Vec<Blocked>,
    #[serde(skip)]
    started: bool,
    #[serde(skip)]
    removed: bool,
    #[serde(skip)]
    folders: usize,
    #[serde(skip)]
    log: String,
}

impl Report {
    fn new(manager: &dyn Manager, name: String, state: State) -> Self {
        Report {
            manager: manager.kind(),
            name,
            state,
            problem: None,
            lingers: None,
            blocked: Vec::new(),
            started: false,
            removed: false,
            folders: 0,
            log: String::new(),
        }
    }

    fn unavailable(manager: &dyn Manager, problem: String) -> Self {
        Report {
            problem: Some(problem),
            ..Report::new(manager, String::new(), State::Unavailable)
        }
    }

    /// What a command prints about the service: a change, a problem, or nothing when it runs as before.
    pub fn text(&self) -> String {
        let mut text = String::new();
        let launchd = self.manager == "launchd";
        let problem = (self.problem.as_deref()).map(crate::printable);
        match self.state {
            State::Off if self.folders > 0 => text.push_str(
                "Background sync is off in config.toml, so folders sync only while koi _daemon runs.\n",
            ),
            State::Unavailable if self.folders > 0 => {
                text.push_str(&format!(
                    "Koidrive can't sync in the background: {}.\n",
                    problem.as_deref().unwrap_or("no service manager")
                ));
                text.push_str(match launchd {
                    true => "Log in to this Mac's desktop, then run: koi service start\n",
                    false => "Log in as this user directly rather than through su or sudo, or run sudo loginctl enable-linger \"$USER\" and log in again. Then run: koi service start\nWithout systemd, keep koi _daemon running with your own service manager.\n",
                });
            }
            State::Running if self.started => {
                text.push_str(&format!("Syncing in the background ({}).\n", self.name));
                text.push_str(match (launchd, self.lingers) {
                    (true, _) => "It starts again when you log in. If macOS shows \"Background Items Added\", leave koi switched on in System Settings > General > Login Items.\n",
                    (false, Some(false)) => "It stops when you log out and starts at your next login. To keep it running without a login, run: sudo loginctl enable-linger \"$USER\"\n",
                    (false, Some(true)) => "It keeps running after you log out and starts again after a restart.\n",
                    (false, None) => "",
                });
            }
            State::Stopped if self.folders > 0 => text.push_str(&format!(
                "The background service {} is installed but not running. Its log: {}\nTo start it again, run: koi service start\n",
                self.name, self.log
            )),
            State::SwitchedOff if self.folders > 0 => text.push_str(SWITCHED_OFF),
            State::NotInstalled if self.removed => {
                text.push_str("Stopped and removed the background service.\n")
            }
            _ => {}
        }
        if let Some(problem) = problem.filter(|_| self.state != State::Unavailable) {
            text.push_str(&format!("The background service: {problem}\n"));
        }
        text.push_str(&blocked_text(&self.blocked));
        text
    }

    /// Whether `koi service start` failed to leave the service running.
    pub fn failed(&self) -> bool {
        self.problem.is_some()
            || matches!(
                self.state,
                State::Stopped | State::SwitchedOff | State::Unavailable
            )
    }

    /// The `koi status` line.
    pub fn status_line(&self) -> String {
        let state = match self.state {
            State::Running => format!("running ({})", self.name),
            State::Stopped => format!("installed, not running ({}); log: {}", self.name, self.log),
            State::SwitchedOff => format!("switched off in Login Items ({})", self.name),
            State::NotInstalled => "not installed; it starts when you link a folder".to_owned(),
            State::Unavailable => format!(
                "unavailable: {}",
                crate::printable(self.problem.as_deref().unwrap_or_default())
            ),
            State::Off => "off in config.toml".to_owned(),
        };
        let mut line = format!("Background service: {state}\n");
        if self.state == State::SwitchedOff {
            line.push_str(&format!("  {SWITCHED_OFF}"));
        }
        if self.lingers == Some(false) {
            line.push_str("  It stops when you log out. To keep it running, run: sudo loginctl enable-linger \"$USER\"\n");
        }
        line.push_str(&blocked_text(&self.blocked));
        line
    }
}

/// What to do about a launch agent switched off in Login Items.
const SWITCHED_OFF: &str = "koi is switched off in System Settings > General > Login Items > Allow in the Background, so the folders don't sync. To sync them in the background, switch it on, then run: koi service start\n";

fn blocked_text(blocked: &[Blocked]) -> String {
    let mut text = String::new();
    for blocked in blocked {
        let folder = crate::printable(&blocked.folder);
        let word = crate::pairing::shell_word(&folder);
        text.push_str(&match blocked.waiting {
            true => format!("koi is still waiting to read {folder}. If macOS asks whether koi may access it, click Allow, so that it syncs in the background.\n"),
            false => format!("macOS doesn't let koi read {folder} in the background. Open System Settings > Privacy & Security > Files & Folders and switch the folder on under koi, or add koi under Full Disk Access. Then run: koi folder restart {word}\n"),
        });
    }
    text
}

/// Brings the service in line with `folders` folders, or leaves it alone when config.toml turns it `off`. `asked` is
/// true for `koi service start`.
pub fn converge(
    manager: &dyn Manager,
    service: &Service,
    folders: usize,
    off: bool,
    asked: bool,
) -> Report {
    let name = manager.name(service);
    let mut report = match manager.state(service) {
        _ if off => Report::new(manager, name, State::Off),
        Err(problem) => Report::unavailable(manager, problem),
        Ok(before) => {
            let action = plan(folders, before, asked);
            let outcome = match action {
                Action::Leave => Ok(()),
                Action::Start => manager.start(service),
                Action::Remove => manager.remove(service),
            };
            let mut after = manager.state(service).unwrap_or(before);
            let spawned = Instant::now() + SPAWN;
            while action == Action::Start
                && outcome.is_ok()
                && after != State::Running
                && Instant::now() < spawned
            {
                std::thread::sleep(Duration::from_millis(100));
                after = manager.state(service).unwrap_or(after);
            }
            let installed = after != State::NotInstalled;
            if installed && manager.lingers() == Some(false) {
                let _ = manager.linger();
            }
            Report {
                problem: outcome.err(),
                lingers: installed.then(|| manager.lingers()).flatten(),
                started: before != State::Running && after == State::Running,
                removed: before != State::NotInstalled && !installed,
                ..Report::new(manager, name, after)
            }
        }
    };
    report.folders = folders;
    report.log = service.log(manager.kind());
    report
}

/// launchd on macOS, systemd everywhere else.
pub fn system() -> Box<dyn Manager> {
    let uid = rustix::process::getuid().as_raw();
    match cfg!(target_os = "macos") {
        true => Box::new(Launchd { uid }),
        false => Box::new(Systemd { uid }),
    }
}

/// This profile's service, run by this binary.
pub fn this_service(paths: &Paths) -> Result<Service, String> {
    let home = (std::env::var_os("HOME").map(PathBuf::from))
        .filter(|home| home.is_absolute())
        .ok_or("HOME is not set to an absolute path")?;
    let binary =
        std::env::current_exe().map_err(|error| format!("cannot find the koi binary: {error}"))?;
    Service::new(paths, &home, &binary)
}

fn background_off(paths: &Paths) -> Result<bool, String> {
    Config::load(&paths.config_file)
        .map(|config| !config.service.background)
        .map_err(|error| error.to_string())
}

/// Brings this profile's service in line with the folders it links, after a command that may have changed them or
/// the pairing, or after `koi service start`, which `asked` for it. When the service runs, it waits for the daemon to
/// answer, and on macOS asks it which folders it may not read, so the person sees the privacy prompt while the command
/// runs.
pub async fn reconcile(paths: &Paths, asked: bool) -> Report {
    let manager = system();
    let folders = match ipc::run(paths, Request::Status).await {
        Ok(Response::Status { status }) => status.folders.len(),
        Ok(_) => return Report::unavailable(manager.as_ref(), "the daemon sent no status".into()),
        Err(error) => return Report::unavailable(manager.as_ref(), error.message),
    };
    let unavailable = |problem| Report {
        folders,
        ..Report::unavailable(manager.as_ref(), problem)
    };
    let service = match this_service(paths) {
        Ok(service) => service,
        Err(problem) => return unavailable(problem),
    };
    let off = match background_off(paths) {
        Ok(off) => off,
        Err(problem) => return unavailable(problem),
    };
    let mut report = converge(manager.as_ref(), &service, folders, off, asked);
    if report.state != State::Running || folders == 0 {
        return report;
    }
    if !answers(paths, STARTUP).await {
        report.problem = Some(format!(
            "it started, but its daemon did not answer within {} seconds; its log: {}",
            STARTUP.as_secs(),
            report.log
        ));
        return report;
    }
    report.blocked = blocked(paths).await;
    report
}

/// Whether the daemon answers a status request `within` the time, counting a daemon that takes the connection and
/// holds its answer.
async fn answers(paths: &Paths, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let answer = tokio::time::timeout(left, ipc::ask(paths, &Request::Status)).await;
        if matches!(answer, Ok(Ok(Some(_)))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// The service as `koi status` shows it, without changing it.
pub async fn inspect(paths: &Paths, daemon_running: bool) -> Report {
    let manager = system();
    let service = match this_service(paths) {
        Ok(service) => service,
        Err(problem) => return Report::unavailable(manager.as_ref(), problem),
    };
    let name = manager.name(&service);
    if background_off(paths).unwrap_or(false) {
        return Report::new(manager.as_ref(), name, State::Off);
    }
    let mut report = match manager.state(&service) {
        Err(problem) => Report::unavailable(manager.as_ref(), problem),
        Ok(state) => Report {
            lingers: (state != State::NotInstalled)
                .then(|| manager.lingers())
                .flatten(),
            ..Report::new(manager.as_ref(), name, state)
        },
    };
    report.log = service.log(manager.kind());
    if daemon_running {
        report.blocked = blocked(paths).await;
    }
    report
}

/// The folders the running daemon may not read. Only macOS keeps a background process out of folders that the person
/// can read, so nothing is asked elsewhere. A daemon too old to know the request answers with an error, read as none.
async fn blocked(paths: &Paths) -> Vec<Blocked> {
    if !cfg!(target_os = "macos") {
        return Vec::new();
    }
    match ipc::ask(paths, &Request::Access).await {
        Ok(Some(Response::Access { blocked })) => blocked,
        _ => Vec::new(),
    }
}

/// Removes this profile's service after `koi server delete` deleted the server at `deleted`, when it is the server this
/// device syncs with. The folders stay linked to it.
pub fn forget(paths: &Paths, deleted: &url::Url) -> Option<Report> {
    let credential = koidrive_driver::Credential::load(&paths.credential()).ok()??;
    let server = url::Url::parse(&credential.server).ok()?;
    if server.origin() != deleted.origin() {
        return None;
    }
    let manager = system();
    let service = this_service(paths).ok()?;
    Some(converge(
        manager.as_ref(),
        &service,
        0,
        background_off(paths).unwrap_or(false),
        false,
    ))
}

/// Writes `text` to `path` unless it already holds it, and returns whether it wrote.
fn write(path: &Path, text: &str) -> Result<bool, String> {
    if fs::read(path).is_ok_and(|old| old == text.as_bytes()) {
        return Ok(false);
    }
    let failed = |error: io::Error| format!("cannot write {}: {error}", path.display());
    let dir = path.parent().unwrap_or(Path::new("/"));
    fs::create_dir_all(dir).map_err(failed)?;
    let mut file = tempfile::NamedTempFile::new_in(dir).map_err(failed)?;
    file.write_all(text.as_bytes()).map_err(failed)?;
    file.persist(path).map_err(|error| failed(error.error))?;
    Ok(true)
}

fn remove_file(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            Err(format!("cannot remove {}: {error}", path.display()))
        }
        _ => Ok(()),
    }
}

fn run(program: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(program)
        .args(args)
        .output()
        .map_err(|error| match error.kind() {
            io::ErrorKind::NotFound => format!("{program} is not installed"),
            _ => format!("cannot run {program}: {error}"),
        })
}

/// Runs a command that must succeed, and returns its output, or an error with what it printed on stderr.
fn ok(program: &str, args: &[&str]) -> Result<String, String> {
    let output = run(program, args)?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned()),
        false => Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// The systemd user instance. Lingering keeps it running without a login session, from boot on.
pub struct Systemd {
    uid: u32,
}

fn systemctl(args: &[&str]) -> Result<String, String> {
    let args: Vec<&str> = ["--user"].iter().chain(args).copied().collect();
    ok("systemctl", &args)
}

impl Manager for Systemd {
    fn kind(&self) -> &'static str {
        "systemd"
    }

    fn name(&self, service: &Service) -> String {
        service.unit_name()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        systemctl(&["show-environment"]).map_err(|error| {
            format!("there is no systemd user session to start it in ({error})")
        })?;
        let active = run(
            "systemctl",
            &["--user", "is-active", "--quiet", &service.unit_name()],
        )?;
        Ok(match active.status.success() {
            true => State::Running,
            false if service.unit_file().exists() => State::Stopped,
            false => State::NotInstalled,
        })
    }

    fn start(&self, service: &Service) -> Result<(), String> {
        let file = service.unit_file();
        let changed = write(&file, &service.unit())?;
        systemctl(&["enable", &file.display().to_string()])?;
        systemctl(&["daemon-reload"])?;
        let verb = if changed { "restart" } else { "start" };
        systemctl(&[verb, &service.unit_name()]).map(drop)
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        let name = service.unit_name();
        if run("systemctl", &["--user", "cat", &name])?
            .status
            .success()
        {
            systemctl(&["disable", "--now", &name])?;
        }
        remove_file(&service.unit_file())?;
        systemctl(&["daemon-reload"]).map(drop)
    }

    fn lingers(&self) -> Option<bool> {
        let uid = self.uid.to_string();
        let linger = ok(
            "loginctl",
            &["show-user", &uid, "--property=Linger", "--value"],
        )
        .ok()?;
        match linger.as_str() {
            "yes" => Some(true),
            "no" => Some(false),
            _ => None,
        }
    }

    /// Turns lingering on where polkit allows it without a password, as it does by default in a local session.
    fn linger(&self) -> Result<(), String> {
        ok("loginctl", &["--no-ask-password", "enable-linger"]).map(drop)
    }
}

/// launchd's GUI domain of the user, which runs agents while the user is logged in and starts them at each login.
pub struct Launchd {
    uid: u32,
}

impl Launchd {
    fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    fn target(&self, service: &Service) -> String {
        format!("{}/{}", self.domain(), service.label())
    }

    /// What `launchctl print` says about the loaded job, or `None` when it is not loaded.
    fn loaded(&self, service: &Service) -> Result<Option<String>, String> {
        let output = run("launchctl", &["print", &self.target(service)])?;
        Ok((output.status.success()).then(|| String::from_utf8_lossy(&output.stdout).into_owned()))
    }
}

impl Manager for Launchd {
    fn kind(&self) -> &'static str {
        "launchd"
    }

    fn name(&self, service: &Service) -> String {
        service.label()
    }

    fn state(&self, service: &Service) -> Result<State, String> {
        ok("launchctl", &["print", &self.domain()]).map_err(|_| {
            "this user is not logged in to the Mac's desktop, so launchd has no session to start it in"
                .to_owned()
        })?;
        Ok(match self.loaded(service)? {
            Some(job) if job.lines().any(|line| line.trim() == "state = running") => State::Running,
            Some(_) => State::Stopped,
            None if service.plist_file().exists() => State::SwitchedOff,
            None => State::NotInstalled,
        })
    }

    fn start(&self, service: &Service) -> Result<(), String> {
        let log = service.log_file();
        let dir = log.parent().unwrap_or(Path::new("/"));
        fs::create_dir_all(dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let plist = service.plist_file();
        let existed = plist.exists();
        let changed = write(&plist, &service.plist())?;
        let target = self.target(service);
        let loaded = self.loaded(service)?.is_some();
        if loaded && !changed {
            return ok("launchctl", &["kickstart", &target]).map(drop);
        }
        if loaded {
            ok("launchctl", &["bootout", &target])?;
        }
        let path = plist.display().to_string();
        let mut attempts = 0;
        loop {
            match ok("launchctl", &["bootstrap", &self.domain(), &path]) {
                Ok(_) => return Ok(()),
                Err(_) if attempts < 3 => {
                    attempts += 1;
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(error) => {
                    if !existed {
                        remove_file(&plist)?;
                    }
                    return Err(format!(
                        "{error}. If koi is switched off in System Settings > General > Login Items > Allow in the Background, switch it on"
                    ));
                }
            }
        }
    }

    fn remove(&self, service: &Service) -> Result<(), String> {
        if self.loaded(service)?.is_some() {
            ok("launchctl", &["bootout", &self.target(service)])?;
        }
        remove_file(&service.plist_file())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;

    fn paths(home: &str, vars: &[(&str, &str)]) -> Paths {
        Paths::from_vars(|name| {
            (vars.iter().chain(&[("HOME", home)]))
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.into())
        })
        .unwrap()
    }

    fn service(vars: &[(&str, &str)]) -> Service {
        let home = "/home/ada";
        let binary = Path::new("/home/ada/.local/bin/koi");
        Service::new(&paths(home, vars), Path::new(home), binary).unwrap()
    }

    #[test]
    fn the_default_profile_has_the_plain_names_and_any_other_one_names_of_its_own() {
        let default = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]);
        assert_eq!(default.unit_name(), "koidrive.service");
        assert_eq!(default.label(), "dev.koidrive.koi");
        let other = service(&[("XDG_STATE_HOME", "/srv/ada/state")]);
        let third = service(&[("XDG_STATE_HOME", "/srv/bob/state")]);
        assert!(
            other.unit_name().starts_with("koidrive-"),
            "{}",
            other.unit_name()
        );
        assert!(
            other.label().starts_with("dev.koidrive.koi."),
            "{}",
            other.label()
        );
        assert_ne!(other.unit_name(), third.unit_name());
        assert_eq!(other, service(&[("XDG_STATE_HOME", "/srv/ada/state")]));
    }

    #[test]
    fn the_unit_runs_this_binary_with_the_profiles_directories() {
        let unit = service(&[("XDG_RUNTIME_DIR", "/run/user/1000")]).unit();
        let lines: Vec<&str> = unit.lines().filter(|line| !line.starts_with('#')).collect();
        for expected in [
            "ExecStart=\"/home/ada/.local/bin/koi\" _daemon",
            "Environment=\"XDG_CONFIG_HOME=/home/ada/.config\"",
            "Environment=\"XDG_STATE_HOME=/home/ada/.local/state\"",
            "Environment=\"XDG_RUNTIME_DIR=/run/user/1000\"",
            "Restart=on-failure",
            "WantedBy=default.target",
        ] {
            assert!(lines.contains(&expected), "{expected} in {unit}");
        }
        let section = lines.iter().position(|line| *line == "[Service]").unwrap();
        let environment = (lines.iter())
            .position(|line| line.starts_with("Environment=\"XDG"))
            .unwrap();
        assert!(environment > section, "{unit}");
    }

    #[test]
    fn a_profile_without_a_runtime_directory_keeps_systemds_out_of_the_daemon() {
        let unit = service(&[]).unit();
        assert!(
            unit.lines()
                .any(|line| line == "UnsetEnvironment=XDG_RUNTIME_DIR"),
            "{unit}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_launch_agent_opens_this_profile_even_when_launchd_sets_a_runtime_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        fs::write(&file, service(&[]).plist()).unwrap();
        let json = Command::new("plutil")
            .args(["-convert", "json", "-o", "-"])
            .arg(&file)
            .output()
            .unwrap();
        assert!(json.status.success());
        let plist: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let mut env = std::collections::BTreeMap::from([
            ("HOME".to_owned(), "/home/ada".to_owned()),
            (
                "XDG_RUNTIME_DIR".to_owned(),
                "/inherited/runtime".to_owned(),
            ),
        ]);
        for (name, value) in plist["EnvironmentVariables"].as_object().unwrap() {
            env.insert(name.clone(), value.as_str().unwrap().to_owned());
        }
        let opened = Paths::from_vars(|name| env.get(name).map(Into::into)).unwrap();
        assert_eq!(opened, paths("/home/ada", &[]));
    }

    #[tokio::test]
    async fn a_daemon_that_takes_the_connection_and_never_answers_does_not_hold_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::under(dir.path());
        fs::create_dir_all(&paths.runtime_dir).unwrap();
        let _held = std::os::unix::net::UnixListener::bind(paths.socket()).unwrap();
        let answered = tokio::time::timeout(
            Duration::from_secs(5),
            answers(&paths, Duration::from_millis(300)),
        )
        .await;
        assert_eq!(answered, Ok(false));
    }

    #[test]
    fn quotes_specifiers_and_dollars_reach_the_daemon_as_written() {
        let home = "/home/ada";
        let paths = paths(home, &[("XDG_STATE_HOME", "/data/100% \"sure\"")]);
        let service = Service::new(&paths, Path::new(home), Path::new("/opt/my $bin/koi")).unwrap();
        let unit = service.unit();
        assert!(
            unit.contains("ExecStart=\"/opt/my $bin/koi\" _daemon\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"XDG_STATE_HOME=/data/100%% \\\"sure\\\"\"\n"),
            "{unit}"
        );
        assert!(
            service
                .plist()
                .contains("<string>/opt/my $bin/koi</string>")
        );
    }

    #[test]
    fn paths_a_service_file_cannot_carry_are_refused() {
        let home = "/home/ada";
        let binary = Path::new("/opt/koi\n[Service]");
        let refused = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap_err();
        assert!(refused.contains("cannot be written"), "{refused}");
    }

    #[test]
    fn the_launch_agent_starts_at_login_stays_alive_and_logs_to_the_library() {
        let plist = service(&[("XDG_STATE_HOME", "/Users/ada/a&b")]).plist();
        for expected in [
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>KeepAlive</key>\n\t<true/>",
            "<string>/home/ada/.local/bin/koi</string>\n\t\t<string>_daemon</string>",
            "<key>XDG_STATE_HOME</key>\n\t\t<string>/Users/ada/a&amp;b</string>",
            "<string>/home/ada/Library/Logs/Koidrive/dev.koidrive.koi.",
        ] {
            assert!(plist.contains(expected), "{expected} in {plist}");
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn plutil_accepts_the_launch_agent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("agent.plist");
        fs::write(
            &file,
            service(&[("XDG_STATE_HOME", "/Users/a <b>")]).plist(),
        )
        .unwrap();
        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            lint.status.success(),
            "{}",
            String::from_utf8_lossy(&lint.stdout)
        );
    }

    #[test]
    fn the_service_runs_while_a_folder_is_linked_and_goes_with_the_last_one() {
        use State::*;
        assert_eq!(plan(1, NotInstalled, false), Action::Start);
        assert_eq!(plan(2, Stopped, false), Action::Start);
        assert_eq!(plan(1, Running, false), Action::Start);
        assert_eq!(plan(0, Running, false), Action::Remove);
        assert_eq!(plan(0, Stopped, false), Action::Remove);
        assert_eq!(plan(0, NotInstalled, false), Action::Leave);
        assert_eq!(plan(1, Unavailable, false), Action::Leave);
        assert_eq!(plan(0, Off, false), Action::Leave);
    }

    #[test]
    fn an_agent_switched_off_in_login_items_starts_only_when_asked_and_goes_with_the_last_folder() {
        use State::*;
        assert_eq!(plan(1, SwitchedOff, false), Action::Leave);
        assert_eq!(plan(1, SwitchedOff, true), Action::Start);
        assert_eq!(plan(0, SwitchedOff, false), Action::Remove);
    }

    /// A service manager in memory: the unit it holds, and whether it runs.
    #[derive(Default)]
    struct Fake {
        unavailable: bool,
        unit: RefCell<Option<String>>,
        running: Cell<bool>,
        restarts: Cell<u32>,
        lingers: Cell<Option<bool>>,
        linger_refused: bool,
        /// How many more reads find a started service not running yet.
        spawning: Cell<u32>,
        /// The person switched it off, as Login Items does: installed, not loaded.
        switched_off: Cell<bool>,
    }

    impl Manager for Fake {
        fn kind(&self) -> &'static str {
            "fake"
        }

        fn name(&self, service: &Service) -> String {
            service.unit_name()
        }

        fn state(&self, _: &Service) -> Result<State, String> {
            if self.unavailable {
                return Err("no user bus".to_owned());
            }
            let spawning = self.spawning.get();
            self.spawning.set(spawning.saturating_sub(1));
            Ok(match (self.unit.borrow().is_some(), self.running.get()) {
                (true, _) if self.switched_off.get() => State::SwitchedOff,
                (true, true) if spawning > 0 => State::Stopped,
                (_, true) => State::Running,
                (true, false) => State::Stopped,
                (false, false) => State::NotInstalled,
            })
        }

        fn start(&self, service: &Service) -> Result<(), String> {
            let unit = Some(service.unit());
            if *self.unit.borrow() != unit && self.running.get() {
                self.restarts.set(self.restarts.get() + 1);
            }
            *self.unit.borrow_mut() = unit;
            self.running.set(true);
            self.switched_off.set(false);
            Ok(())
        }

        fn remove(&self, _: &Service) -> Result<(), String> {
            *self.unit.borrow_mut() = None;
            self.running.set(false);
            Ok(())
        }

        fn lingers(&self) -> Option<bool> {
            self.lingers.get()
        }

        fn linger(&self) -> Result<(), String> {
            if self.linger_refused {
                return Err("interactive authentication required".to_owned());
            }
            self.lingers.set(Some(true));
            Ok(())
        }
    }

    #[test]
    fn linking_starts_the_service_once_and_unlinking_the_last_folder_removes_it() {
        let fake = Fake::default();
        let service = service(&[]);
        let first = converge(&fake, &service, 1, false, false);
        assert_eq!((first.state, first.started), (State::Running, true));
        let text = first.text();
        assert!(
            text.contains("Syncing in the background (koidrive.service)"),
            "{text}"
        );
        assert!(!text.contains("log out"), "lingering is unknown: {text}");
        let second = converge(&fake, &service, 2, false, false);
        assert_eq!((second.state, second.started), (State::Running, false));
        assert_eq!(second.text(), "");
        assert_eq!(fake.restarts.get(), 0);
        assert_eq!(
            converge(&fake, &service, 1, false, false).state,
            State::Running
        );
        let removed = converge(&fake, &service, 0, false, false);
        assert_eq!(
            (removed.state, removed.removed),
            (State::NotInstalled, true)
        );
        assert_eq!(
            removed.text(),
            "Stopped and removed the background service.\n"
        );
        assert_eq!(*fake.unit.borrow(), None);
        let again = converge(&fake, &service, 0, false, false);
        assert_eq!((again.state, again.removed), (State::NotInstalled, false));
        assert_eq!(again.text(), "");
    }

    #[test]
    fn a_service_that_spawns_a_moment_after_it_starts_reads_as_running() {
        let fake = Fake {
            spawning: Cell::new(3),
            ..Fake::default()
        };
        let report = converge(&fake, &service(&[]), 1, false, false);
        assert_eq!((report.state, report.started), (State::Running, true));
        assert!(!report.text().contains("not running"), "{}", report.text());
    }

    #[test]
    fn a_link_leaves_an_agent_switched_off_in_login_items_off_and_says_how_to_switch_it_on() {
        let fake = Fake::default();
        let service = service(&[]);
        converge(&fake, &service, 1, false, false);
        fake.running.set(false);
        fake.switched_off.set(true);
        let linked = converge(&fake, &service, 2, false, false);
        assert_eq!(linked.state, State::SwitchedOff);
        assert!(!fake.running.get());
        let text = linked.text();
        assert!(
            text.contains("Login Items > Allow in the Background"),
            "{text}"
        );
        assert!(
            text.contains("switch it on, then run: koi service start"),
            "{text}"
        );
        let line = linked.status_line();
        assert!(
            line.contains("switched off in Login Items (koidrive.service)"),
            "{line}"
        );
        assert!(linked.failed());
        let asked = converge(&fake, &service, 2, false, true);
        assert_eq!((asked.state, asked.started), (State::Running, true));
        assert!(!asked.failed());
    }

    #[test]
    fn a_moved_binary_restarts_the_running_service_with_the_new_unit() {
        let fake = Fake::default();
        converge(&fake, &service(&[]), 1, false, false);
        let home = "/home/ada";
        let binary = Path::new("/usr/local/bin/koi");
        let moved = Service::new(&paths(home, &[]), Path::new(home), binary).unwrap();
        converge(&fake, &moved, 1, false, false);
        assert_eq!(fake.restarts.get(), 1);
        assert!(
            fake.unit
                .borrow()
                .as_ref()
                .unwrap()
                .contains("/usr/local/bin/koi")
        );
    }

    #[test]
    fn the_config_switch_keeps_koi_from_installing_or_removing_the_service() {
        let fake = Fake::default();
        let off = converge(&fake, &service(&[]), 1, true, false);
        assert_eq!(off.state, State::Off);
        assert_eq!(*fake.unit.borrow(), None);
        assert!(off.text().contains("off in config.toml"), "{}", off.text());
        fake.running.set(true);
        *fake.unit.borrow_mut() = Some(String::new());
        converge(&fake, &service(&[]), 0, true, false);
        assert!(fake.running.get());
    }

    #[test]
    fn without_a_service_manager_the_link_says_so_and_what_to_do() {
        let fake = Fake {
            unavailable: true,
            ..Fake::default()
        };
        let report = converge(&fake, &service(&[]), 1, false, false);
        assert_eq!(report.state, State::Unavailable);
        assert_eq!(report.problem.as_deref(), Some("no user bus"));
        let text = report.text();
        assert!(
            text.starts_with("Koidrive can't sync in the background: no user bus.\n"),
            "{text}"
        );
        assert!(text.contains("koi service start"), "{text}");
        assert_eq!(converge(&fake, &service(&[]), 0, false, false).text(), "");
    }

    #[test]
    fn lingering_is_turned_on_or_the_command_to_turn_it_on_is_named() {
        let allowed = Fake::default();
        allowed.lingers.set(Some(false));
        let report = converge(&allowed, &service(&[]), 1, false, false);
        assert_eq!(report.lingers, Some(true));
        let text = report.text();
        assert!(text.contains("keeps running after you log out"), "{text}");
        let refused = Fake {
            linger_refused: true,
            ..Fake::default()
        };
        refused.lingers.set(Some(false));
        let report = converge(&refused, &service(&[]), 1, false, false);
        assert_eq!(report.lingers, Some(false));
        assert!(
            report.text().contains("sudo loginctl enable-linger"),
            "{}",
            report.text()
        );
        assert!(report.status_line().contains("sudo loginctl enable-linger"));
    }

    #[test]
    fn a_folder_macos_keeps_from_the_daemon_names_the_settings_and_the_restart() {
        let text = blocked_text(&[
            Blocked {
                folder: "/Users/ada/Documents/Work".into(),
                waiting: true,
            },
            Blocked {
                folder: "/Users/ada/Desktop/Notes".into(),
                waiting: false,
            },
        ]);
        assert!(
            text.contains(
                "koi is still waiting to read /Users/ada/Documents/Work. If macOS asks whether koi may access it, click Allow"
            ),
            "{text}"
        );
        assert!(
            text.contains("Privacy & Security > Files & Folders"),
            "{text}"
        );
        assert!(
            text.contains("koi folder restart /Users/ada/Desktop/Notes"),
            "{text}"
        );
    }

    #[test]
    fn the_restart_command_for_a_folder_with_a_space_pastes_as_one_argument() {
        let text = blocked_text(&[Blocked {
            folder: "/Users/ada/Desktop/Work Notes".into(),
            waiting: false,
        }]);
        assert!(
            text.contains("Then run: koi folder restart '/Users/ada/Desktop/Work Notes'\n"),
            "{text}"
        );
    }
}
