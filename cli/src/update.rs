// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, SystemTime};

use koidrive_driver::Paths;
use koidrive_driver::update::{self, Release};
use koidrive_ipc::Code;
use semver::Version;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use url::Url;

use crate::error::CliError;
use crate::report::{Done, UpdateResult, UpdatedData, Via};
use crate::service;

const BREW: Duration = Duration::from_secs(15 * 60);
const INSTALLER: Duration = Duration::from_secs(5 * 60);
const RECONCILE: Duration = Duration::from_secs(7 * 60);
const SCAN: Duration = Duration::from_secs(60 * 60);
const INSTALL_SCRIPT: &str = "https://koidrive.com/install.sh";
const INSTALL_SCRIPT_VARIABLE: &str = "KOIDRIVE_INSTALL_SCRIPT_URL";

const BREW_BUSY: [&str; 2] = ["process is already running", "process has already locked"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installs {
    homebrew: Vec<PathBuf>,
    script: Option<PathBuf>,
    app: PathBuf,
}

impl Installs {
    pub fn of_this_machine() -> Installs {
        let absolute = |name| {
            std::env::var_os(name)
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        Installs::under(
            absolute("HOMEBREW_PREFIX").as_deref(),
            absolute("HOME").as_deref(),
        )
    }

    fn under(homebrew_prefix: Option<&Path>, home: Option<&Path>) -> Installs {
        let mut homebrew: Vec<PathBuf> = Vec::new();
        let prefixes = [
            homebrew_prefix,
            Some(Path::new("/opt/homebrew")),
            Some(Path::new("/usr/local")),
            Some(Path::new("/home/linuxbrew/.linuxbrew")),
        ];
        for prefix in prefixes.into_iter().flatten() {
            let koi = prefix.join("opt/koidrive/bin/koi");
            if !homebrew.contains(&koi) {
                homebrew.push(koi);
            }
        }
        Installs {
            homebrew,
            script: home.map(|home| home.join(".local/bin/koi")),
            app: PathBuf::from("/Applications/Koidrive.app/Contents/Helpers/koi"),
        }
    }

    fn all(&self) -> Vec<PathBuf> {
        let mut all = self.homebrew.clone();
        all.extend(self.script.clone());
        all.push(self.app.clone());
        all
    }

    fn pick(&self, exists: impl Fn(&Path) -> bool, running: &[&Path]) -> Option<Install> {
        let homebrew = self
            .homebrew
            .iter()
            .cloned()
            .chain(running.iter().filter_map(|koi| homebrew_link(koi)));
        if let Some(koi) = homebrew.into_iter().find(|koi| exists(koi)) {
            let prefix = koi.ancestors().nth(4)?.to_path_buf();
            return Some(Install::Homebrew { prefix });
        }
        let by_script = (self.script.iter().map(PathBuf::as_path))
            .filter(|koi| exists(koi))
            .chain(running.iter().copied().filter(|koi| !self.owned(koi)))
            .next();
        if let Some(dir) = by_script.and_then(Path::parent) {
            return Some(Install::Script {
                dir: dir.to_path_buf(),
            });
        }
        exists(&self.app).then_some(Install::App)
    }

    fn owned(&self, koi: &Path) -> bool {
        *koi == self.app
            || self.homebrew.iter().any(|homebrew| homebrew == koi)
            || homebrew_link(koi).is_some()
            || (koi.components()).any(|part| part.as_os_str().to_string_lossy().ends_with(".app"))
    }
}

pub fn homebrew_link(binary: &Path) -> Option<PathBuf> {
    if binary.ends_with("opt/koidrive/bin/koi") {
        return Some(binary.to_path_buf());
    }
    if !binary.ends_with("bin/koi") {
        return None;
    }
    let version = binary.parent()?.parent()?;
    let formula = version.parent()?;
    let cellar = formula.parent()?;
    let prefix = cellar.parent()?;
    let named = |path: &Path, name: &str| path.file_name().is_some_and(|file| file == name);
    (named(formula, "koidrive") && named(cellar, "Cellar") && version.file_name().is_some())
        .then(|| prefix.join("opt/koidrive/bin/koi"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Install {
    Homebrew { prefix: PathBuf },
    Script { dir: PathBuf },
    App,
}

impl Install {
    fn koi(&self, installs: &Installs) -> PathBuf {
        match self {
            Install::Homebrew { prefix } => prefix.join("opt/koidrive/bin/koi"),
            Install::Script { dir } => dir.join("koi"),
            Install::App => installs.app.clone(),
        }
    }

    fn via(&self) -> Via {
        match self {
            Install::Homebrew { .. } => Via::Homebrew,
            Install::Script { .. } => Via::Installer,
            Install::App => Via::App,
        }
    }
}

fn refuse_root(root: bool) -> Result<(), CliError> {
    match root {
        true => Err(CliError::usage(
            "koi update upgrades Koidrive for one user; run it as your own user, not as root",
        )),
        false => Ok(()),
    }
}

struct Echo {
    json: bool,
    log: Option<File>,
}

#[derive(Clone, Copy)]
enum Child {
    Progress,
    Reconcile,
}

impl Echo {
    fn open(path: &Path, json: bool) -> Echo {
        let log = (path.parent().map(fs::create_dir_all).transpose().ok())
            .and_then(|_| File::create(path).ok());
        Echo { json, log }
    }

    fn line(&mut self, line: &str, stderr: bool) {
        match self.json || stderr {
            true => eprintln!("{line}"),
            false => println!("{line}"),
        }
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "{line}");
        }
    }

    async fn run(
        &mut self,
        mut command: Command,
        within: Duration,
        kind: Child,
    ) -> io::Result<Option<(ExitStatus, Vec<u8>, String)>> {
        let shown = format!("{:?}", command.as_std());
        if let Some(log) = &mut self.log {
            let _ = writeln!(log, "$ {shown}");
        }
        let kill_on_drop = match kind {
            Child::Progress => true,
            Child::Reconcile => {
                command.process_group(0);
                false
            }
        };
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(kill_on_drop)
            .spawn()?;
        let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped")).lines();
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr is piped")).lines();
        let finished = tokio::time::timeout(within, async {
            let (mut printed, mut errors) = (Vec::new(), String::new());
            let (mut out_open, mut err_open) = (true, true);
            while out_open || err_open {
                tokio::select! {
                    line = stdout.next_line(), if out_open => match line? {
                        Some(line) => {
                            match kind {
                                Child::Progress => self.line(&line, false),
                                Child::Reconcile => {
                                    if let Some(log) = &mut self.log {
                                        let _ = writeln!(log, "{line}");
                                    }
                                }
                            }
                            printed.extend_from_slice(line.as_bytes());
                            printed.push(b'\n');
                        }
                        None => out_open = false,
                    },
                    line = stderr.next_line(), if err_open => match line? {
                        Some(line) => {
                            self.line(&line, true);
                            errors.push_str(&line);
                            errors.push('\n');
                        }
                        None => err_open = false,
                    },
                }
            }
            let status = child.wait().await?;
            io::Result::Ok((status, printed, errors))
        })
        .await;
        match finished {
            Ok(finished) => finished.map(Some),
            Err(_) => {
                if let Some(log) = &mut self.log {
                    let _ = writeln!(log, "did not finish within {} seconds", within.as_secs());
                }
                Ok(None)
            }
        }
    }
}

fn failed(why: String) -> CliError {
    CliError::new(Code::Failed, why)
}

fn minutes(within: Duration) -> u64 {
    within.as_secs().div_ceil(60)
}

async fn homebrew(prefix: &Path, echo: &mut Echo) -> Result<(), CliError> {
    let brew = prefix.join("bin/brew");
    if !brew.is_file() {
        return Err(failed(format!(
            "Homebrew is not at {}, where koi expected it",
            brew.display()
        )));
    }
    let path = format!(
        "{}:/usr/bin:/bin:/usr/sbin:/sbin",
        prefix.join("bin").display()
    );
    for args in [&["update"][..], &["upgrade", "koidrive/tap/koidrive"]] {
        let mut command = Command::new(&brew);
        command
            .args(args)
            .env("PATH", &path)
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK", "1");
        let shown = format!("brew {}", args.join(" "));
        let (status, _, errors) = (echo.run(command, BREW, Child::Progress).await)
            .map_err(|error| failed(format!("{shown} did not run: {error}")))?
            .ok_or_else(|| {
                failed(format!(
                    "{shown} did not finish within {} minutes",
                    minutes(BREW)
                ))
            })?;
        if status.success() {
            continue;
        }
        if BREW_BUSY.iter().any(|busy| errors.contains(busy)) {
            return Err(failed(
                "Homebrew is busy with another command; try again when it finishes.".to_owned(),
            ));
        }
        return Err(failed(format!("{shown} failed ({})", exit(status))));
    }
    Ok(())
}

fn exit(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit status {code}"),
        (None, Some(signal)) => format!("signal {signal}"),
        _ => status.to_string(),
    }
}

fn script_url() -> Result<(Url, bool), CliError> {
    let text = std::env::var(INSTALL_SCRIPT_VARIABLE).unwrap_or_else(|_| INSTALL_SCRIPT.to_owned());
    let url = Url::parse(&text).map_err(|_| failed(format!("{text} is not a URL")))?;
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    match url.scheme() {
        "https" => Ok((url, true)),
        "http" if loopback => Ok((url, false)),
        _ => Err(failed(format!("{text} is not an https URL"))),
    }
}

async fn installer(
    (url, https): (Url, bool),
    dir: &Path,
    version: &Version,
    echo: &mut Echo,
) -> Result<(), CliError> {
    let unreachable = |error: reqwest::Error| {
        CliError::new(
            Code::Unavailable,
            format!("install.sh cannot be downloaded from {url}: {error}"),
        )
    };
    let http = reqwest::Client::builder()
        .https_only(https)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(unreachable)?;
    let script = (http.get(url.clone()).send().await)
        .and_then(reqwest::Response::error_for_status)
        .map_err(unreachable)?
        .bytes()
        .await
        .map_err(unreachable)?;
    let file = tempfile::Builder::new()
        .prefix("koidrive-install")
        .suffix(".sh")
        .tempfile()
        .and_then(|mut file| file.write_all(&script).map(|()| file))
        .map_err(|error| failed(format!("install.sh cannot be saved: {error}")))?;
    let mut command = Command::new("/bin/sh");
    command
        .arg(file.path())
        .env("KOIDRIVE_INSTALL_DIR", dir)
        .env("KOIDRIVE_VERSION", format!("v{version}"));
    let (status, _, _) = (echo.run(command, INSTALLER, Child::Progress).await)
        .map_err(|error| failed(format!("install.sh did not run: {error}")))?
        .ok_or_else(|| {
            failed(format!(
                "install.sh did not finish within {} minutes",
                minutes(INSTALLER)
            ))
        })?;
    match status.success() {
        true => Ok(()),
        false => Err(failed(format!("install.sh failed ({})", exit(status)))),
    }
}

fn log_path(paths: &Paths) -> PathBuf {
    let home = (std::env::var_os("HOME").map(PathBuf::from)).filter(|home| home.is_absolute());
    match (cfg!(target_os = "macos"), home) {
        (true, Some(home)) => home.join("Library/Logs/Koidrive/update.log"),
        _ => paths.state_dir.join("update.log"),
    }
}

pub async fn run(paths: &Paths, json: bool) -> Result<Done, CliError> {
    refuse_root(rustix::process::geteuid().is_root())?;
    let release = update::check_now(paths)
        .await
        .map_err(|error| CliError::new(Code::Unavailable, error.to_string()))?
        .ok_or_else(|| failed("GitHub lists no Koidrive release for this system".to_owned()))?;
    let Release { version, url } = release;
    let to = Version::parse(&version).map_err(|_| {
        failed(format!(
            "GitHub names the release {version}, which is not a version"
        ))
    })?;
    let manager = service::system();
    let service = service::this_service(paths);
    let installed = (service.as_ref().ok())
        .and_then(|service| manager.installed(service))
        .map(PathBuf::from);
    let this_koi = std::env::current_exe().ok();
    let running: Vec<&Path> = installed
        .iter()
        .chain(&this_koi)
        .map(PathBuf::as_path)
        .collect();
    let installs = Installs::of_this_machine();
    let install = installs.pick(Path::is_file, &running);
    let from = (install.as_ref())
        .and_then(|install| service::version_of(&install.koi(&installs)))
        .unwrap_or_else(service::this_version);
    let upgrading = to.cmp_precedence(&from).is_gt();
    let download = || failed(format!("Download Koidrive {to} from {url}"));
    let mut echo = Echo::open(&log_path(paths), json);
    if upgrading {
        match &install {
            Some(Install::Homebrew { prefix }) => homebrew(prefix, &mut echo).await?,
            Some(Install::Script { dir }) => installer(script_url()?, dir, &to, &mut echo).await?,
            Some(Install::App) | None => return Err(download()),
        }
    }
    let Some(install) = install else {
        let text = format!("Koidrive {from} is the newest release.\n");
        return Ok(Done::ok(
            UpdatedData {
                from: from.to_string(),
                to: from.to_string(),
                via: None,
                result: UpdateResult::Unchanged,
                service: None,
            },
            text,
        )
        .unchanged());
    };
    let koi = install.koi(&installs);
    let actual = service::version_of(&koi).ok_or_else(|| {
        failed(format!(
            "{} is missing or did not report a Koidrive version after the update",
            koi.display()
        ))
    })?;
    if actual.cmp_precedence(&to).is_lt() {
        return Err(failed(format!(
            "{} is still Koidrive {actual}; the requested release is {to}. Try koi update again when the install source has that release.",
            koi.display()
        )));
    }
    let service = service.map_err(failed)?;
    let args = ["_reconcile", "--json"];
    let mut command = Command::new(&koi);
    command.args(args);
    let output = (echo.run(command, RECONCILE, Child::Reconcile).await)
        .map(|output| output.map(|(status, printed, _)| (status, printed)));
    let report =
        service::delegated_report(manager.as_ref(), &service, &koi, &args, RECONCILE, output);
    let summary = match upgrading {
        true => format!("Updated Koidrive from {from} to {actual}."),
        false => format!("Koidrive {actual} is the newest release."),
    };
    let text = format!(
        "{summary}\n{}",
        match report.text() {
            text if text.is_empty() => report.status_line(),
            text => text,
        }
    );
    let failed_service = report.failed();
    let done = Done::ok(
        UpdatedData {
            from: from.to_string(),
            to: actual.to_string(),
            via: Some(install.via()),
            result: if upgrading {
                UpdateResult::Updated
            } else {
                UpdateResult::Unchanged
            },
            service: Some(report),
        },
        text,
    );
    if failed_service {
        let reason = done.text.lines().last().unwrap_or_default().to_owned();
        return Err(CliError {
            partial: Some(Box::new(done)),
            ..CliError::new(Code::Failed, reason)
        });
    }
    Ok(if upgrading { done } else { done.unchanged() })
}

fn scan(
    seen: &mut HashMap<PathBuf, (SystemTime, Option<Version>)>,
    candidates: &[PathBuf],
    runs: &Version,
) -> Option<PathBuf> {
    (candidates.iter())
        .filter_map(|koi| {
            let Ok(modified) = fs::metadata(koi).and_then(|metadata| metadata.modified()) else {
                seen.remove(koi);
                return None;
            };
            let cached = seen
                .entry(koi.clone())
                .or_insert_with(|| (modified, service::version_of(koi)));
            if cached.0 != modified {
                *cached = (modified, service::version_of(koi));
            }
            Some((cached.1.clone()?, koi))
        })
        .filter(|(version, _)| version.cmp_precedence(runs).is_gt())
        .max_by(|(a, _), (b, _)| a.cmp_precedence(b))
        .map(|(_, koi)| koi.clone())
}

fn take_service(koi: &Path) {
    let mut command = Command::new(koi);
    command
        .arg("_reconcile")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::warn!(koi = %koi.display(), %error, "a newer koi is installed, but it did not run");
            return;
        }
    };
    tracing::info!(koi = %koi.display(), "a newer koi is installed; it takes the background service");
    let koi = koi.to_path_buf();
    tokio::spawn(async move {
        match tokio::time::timeout(RECONCILE, child.wait()).await {
            Ok(Ok(status)) if status.success() => {}
            Ok(Ok(status)) => {
                tracing::warn!(koi = %koi.display(), %status, "the newer koi did not take the background service")
            }
            Ok(Err(error)) => {
                tracing::warn!(koi = %koi.display(), %error, "the newer koi did not take the background service")
            }
            Err(_) => {
                tracing::warn!(koi = %koi.display(), "the newer koi did not take the background service within seven minutes")
            }
        }
    });
}

pub async fn follow_newest(paths: Paths) {
    let mut seen = HashMap::new();
    let mut every = tokio::time::interval(SCAN);
    loop {
        every.tick().await;
        let paths = paths.clone();
        let looked = tokio::task::spawn_blocking(move || {
            let manager = service::system();
            let installed = (service::this_service(&paths).ok())
                .and_then(|service| manager.installed(&service))
                .map(PathBuf::from);
            let newer = installed.and_then(|installed| {
                let mut candidates = Installs::of_this_machine().all();
                let mut running = vec![installed];
                running.extend(std::env::current_exe().ok());
                candidates.extend(running.iter().filter_map(|koi| homebrew_link(koi)));
                candidates.extend(running);
                scan(&mut seen, &candidates, &service::this_version())
            });
            (seen, newer)
        })
        .await;
        let Ok((kept, newer)) = looked else { return };
        seen = kept;
        if let Some(koi) = newer {
            take_service(&koi);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::CommandExt;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn executable(path: &Path, script: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn the_install_to_upgrade_is_homebrew_then_install_sh_then_the_app() {
        let installs = Installs::under(None, Some(Path::new("/home/ada")));
        let pick = |present: &[&str], running: &[&str]| {
            let running: Vec<&Path> = running.iter().map(Path::new).collect();
            installs.pick(
                |path| present.iter().any(|item| Path::new(item) == path),
                &running,
            )
        };
        let app = "/Applications/Koidrive.app/Contents/Helpers/koi";
        let script = "/home/ada/.local/bin/koi";
        assert_eq!(
            pick(&["/usr/local/opt/koidrive/bin/koi", script, app], &[app]),
            Some(Install::Homebrew {
                prefix: "/usr/local".into()
            })
        );
        assert_eq!(
            pick(&["/home/linuxbrew/.linuxbrew/opt/koidrive/bin/koi"], &[]),
            Some(Install::Homebrew {
                prefix: "/home/linuxbrew/.linuxbrew".into()
            })
        );
        assert_eq!(
            pick(&[script, app], &["/srv/tools/koi"]),
            Some(Install::Script {
                dir: "/home/ada/.local/bin".into()
            })
        );
        assert_eq!(
            pick(&[app], &[app, "/srv/tools/koi"]),
            Some(Install::Script {
                dir: "/srv/tools".into()
            }),
            "a koi the service or this command runs outside Homebrew and the app is install.sh's"
        );
        for owned in [
            app,
            "/opt/homebrew/Cellar/koidrive/0.1.0/bin/koi",
            "/opt/homebrew/opt/koidrive/bin/koi",
            "/Users/ada/Applications/Koidrive.app/Contents/Helpers/koi",
        ] {
            assert_eq!(pick(&[app], &[owned]), Some(Install::App), "{owned}");
        }
        assert_eq!(pick(&[], &[]), None);

        let custom = Installs::under(Some(Path::new("/srv/brew")), None);
        assert_eq!(
            custom.pick(
                |path| path == Path::new("/srv/brew/opt/koidrive/bin/koi"),
                &[]
            ),
            Some(Install::Homebrew {
                prefix: "/srv/brew".into()
            })
        );
        assert_eq!(
            Installs::under(Some(Path::new("/opt/homebrew")), None)
                .homebrew
                .len(),
            3
        );
    }

    #[test]
    fn a_homebrew_keg_maps_to_its_opt_link_under_every_prefix() {
        for prefix in [
            "/opt/homebrew",
            "/usr/local",
            "/home/linuxbrew/.linuxbrew",
            "/srv/brew",
        ] {
            let opt = Path::new(prefix).join("opt/koidrive/bin/koi");
            assert_eq!(homebrew_link(&opt), Some(opt.clone()));
            for version in ["0.1.0", "0.1.0-alpha.3", "0.1.2_1"] {
                let keg = format!("{prefix}/Cellar/koidrive/{version}/bin/koi");
                assert_eq!(
                    homebrew_link(Path::new(&keg)),
                    Some(Path::new(prefix).join("opt/koidrive/bin/koi")),
                    "{keg}"
                );
            }
        }
        for other in [
            "/opt/homebrew/bin/koi",
            "/opt/homebrew/Cellar/other/0.1.0/bin/koi",
            "/opt/homebrew/Cellar/koidrive/0.1.0/libexec/koi",
            "/opt/homebrew/Cellar/koidrive/0.1.0/bin/koi2",
            "/home/ada/.local/bin/koi",
        ] {
            assert_eq!(homebrew_link(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn koi_update_refuses_to_run_as_root() {
        let refused = refuse_root(true).unwrap_err();
        assert_eq!(refused.code, Code::Usage);
        assert!(
            refused.message.contains("run it as your own user"),
            "{}",
            refused.message
        );
        assert!(refuse_root(false).is_ok());
    }

    const BREW: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$0.calls"
printf 'PATH=%s AUTO=%s HINTS=%s DEPENDENTS=%s\n' "$PATH" "$HOMEBREW_NO_AUTO_UPDATE" "$HOMEBREW_NO_ENV_HINTS" "$HOMEBREW_NO_INSTALLED_DEPENDENTS_CHECK" >> "$0.env"
echo "==> brew $1"
"#;

    fn echo(dir: &Path) -> Echo {
        Echo::open(&dir.join("logs/update.log"), true)
    }

    #[test]
    fn reconcile_subprocess() {
        let Some(dir) = std::env::var_os("KOI_RECONCILE_TEST_DIR").map(PathBuf::from) else {
            return;
        };
        let mode = std::env::var("KOI_RECONCILE_TEST_MODE").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut command = Command::new(dir.join("koi"));
            command.arg(&dir);
            let mut echo = echo(&dir);
            if mode == "cancel" {
                assert!(
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        echo.run(command, Duration::from_secs(10), Child::Reconcile)
                    )
                    .await
                    .is_err()
                );
            } else {
                let within = if mode == "timeout" {
                    Duration::from_millis(100)
                } else {
                    Duration::from_secs(10)
                };
                let result = echo.run(command, within, Child::Reconcile).await.unwrap();
                assert!(result.is_none());
            }
        });
    }

    fn surviving_reconcile(mode: &str) {
        let dir = tempfile::tempdir().unwrap();
        executable(
            &dir.path().join("koi"),
            "#!/bin/sh\ntouch \"$1/ready\"\ni=0\nwhile [ ! -f \"$1/continue\" ] && [ $i -lt 250 ]; do sleep 0.02; i=$((i+1)); done\n[ -f \"$1/continue\" ] && touch \"$1/completed\"\n",
        );
        let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
        parent
            .args([
                "--exact",
                "update::tests::reconcile_subprocess",
                "--nocapture",
            ])
            .env("KOI_RECONCILE_TEST_DIR", dir.path())
            .env("KOI_RECONCILE_TEST_MODE", mode)
            .process_group(0);
        let mut parent = parent.spawn().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !dir.path().join("ready").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(dir.path().join("ready").exists());
        if mode == "sigint" {
            assert!(
                std::process::Command::new("/bin/kill")
                    .args(["-INT", "--", &format!("-{}", parent.id())])
                    .status()
                    .unwrap()
                    .success()
            );
        }
        loop {
            if let Some(status) = parent.try_wait().unwrap() {
                if mode == "sigint" {
                    assert!(!status.success());
                } else {
                    assert!(status.success());
                }
                break;
            }
            if std::time::Instant::now() >= deadline {
                parent.kill().unwrap();
                parent.wait().unwrap();
                panic!("foreground reconcile wait did not end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        fs::write(dir.path().join("continue"), "").unwrap();
        while !dir.path().join("completed").exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            dir.path().join("completed").exists(),
            "reconciliation must finish after {mode}"
        );
    }

    #[test]
    fn reconciliation_survives_foreground_process_group_interrupt() {
        surviving_reconcile("sigint");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_timeout() {
        surviving_reconcile("timeout");
    }

    #[test]
    fn reconciliation_survives_foreground_wait_cancellation() {
        surviving_reconcile("cancel");
    }

    #[tokio::test]
    async fn cancellable_children_are_killed_when_the_wait_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/bin/sh");
        let pid_file = dir.path().join("pid");
        command
            .args(["-c", "echo $$ > \"$1\"; exec sleep 30", "sh"])
            .arg(&pid_file);
        let result = echo(dir.path())
            .run(command, Duration::from_millis(100), Child::Progress)
            .await
            .unwrap();
        assert!(result.is_none());
        let pid = fs::read_to_string(pid_file).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            if !std::process::Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "cancellable child is still alive"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn homebrew_upgrades_only_the_koidrive_formula_with_its_own_brew() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        let brew = prefix.join("bin/brew");
        executable(&brew, BREW);
        homebrew(&prefix, &mut echo(dir.path())).await.unwrap();
        assert_eq!(
            fs::read_to_string(brew.with_extension("calls")).unwrap(),
            "update\nupgrade koidrive/tap/koidrive\n"
        );
        let env = format!(
            "PATH={}/bin:/usr/bin:/bin:/usr/sbin:/sbin AUTO=1 HINTS=1 DEPENDENTS=1\n",
            prefix.display()
        );
        assert_eq!(
            fs::read_to_string(brew.with_extension("env")).unwrap(),
            env.repeat(2)
        );
        let log = fs::read_to_string(dir.path().join("logs/update.log")).unwrap();
        assert!(
            log.contains("==> brew update\n") && log.contains("==> brew upgrade\n"),
            "{log}"
        );
    }

    #[tokio::test]
    async fn homebrew_without_brew_names_the_path_it_expected() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        let error = homebrew(&prefix, &mut echo(dir.path())).await.unwrap_err();
        assert_eq!(
            error.message,
            format!(
                "Homebrew is not at {}/bin/brew, where koi expected it",
                prefix.display()
            )
        );
    }

    #[tokio::test]
    async fn homebrew_busy_with_another_command_says_to_try_again() {
        let busy = [
            (
                "update",
                "Error: Another \u{60}brew update\u{60} process is already running.\nPlease wait for it to finish or terminate it to continue.",
            ),
            (
                "upgrade",
                "Error: A \u{60}brew upgrade koidrive/tap/koidrive\u{60} process has already locked /opt/homebrew/var/homebrew/locks/koidrive.formula.lock.\nPlease wait for it to finish or terminate it to continue.",
            ),
        ];
        for (command, message) in busy {
            let dir = tempfile::tempdir().unwrap();
            let prefix = dir.path().join("brew");
            let script = format!(
                "#!/bin/sh\n[ \"$1\" = {command} ] || exit 0\nprintf '%s\\n' '{}' >&2\nexit 1\n",
                message.replace('\'', "'\\''")
            );
            executable(&prefix.join("bin/brew"), &script);
            let error = homebrew(&prefix, &mut echo(dir.path())).await.unwrap_err();
            assert_eq!(
                error.message, "Homebrew is busy with another command; try again when it finishes.",
                "{command}"
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("brew");
        executable(
            &prefix.join("bin/brew"),
            "#!/bin/sh\necho 'Error: koidrive: no bottle' >&2\nexit 1\n",
        );
        let error = homebrew(&prefix, &mut echo(dir.path())).await.unwrap_err();
        assert_eq!(error.message, "brew update failed (exit status 1)");
    }

    async fn serve_once(listener: TcpListener, body: &'static str) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        while !request.ends_with(b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            assert!(read > 0);
            request.extend_from_slice(&buffer[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn the_installer_runs_install_sh_into_the_kois_directory_for_the_release() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("my bin");
        fs::create_dir_all(&bin).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/install.sh",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let served = tokio::spawn(serve_once(
            listener,
            "printf '%s|%s\\n' \"$KOIDRIVE_INSTALL_DIR\" \"$KOIDRIVE_VERSION\" > \"$KOIDRIVE_INSTALL_DIR/installed\"\necho 'Installed koi'\n",
        ));
        let version = Version::parse("0.1.2").unwrap();
        installer((url, false), &bin, &version, &mut echo(dir.path()))
            .await
            .unwrap();
        served.await.unwrap();
        assert_eq!(
            fs::read_to_string(bin.join("installed")).unwrap(),
            format!("{}|v0.1.2\n", bin.display())
        );
        let log = fs::read_to_string(dir.path().join("logs/update.log")).unwrap();
        assert!(log.contains("Installed koi\n"), "{log}");

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "http://{}/install.sh",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let served = tokio::spawn(serve_once(
            listener,
            "echo 'checksum mismatch' >&2\nexit 3\n",
        ));
        let error = installer((url, false), &bin, &version, &mut echo(dir.path()))
            .await
            .unwrap_err();
        served.await.unwrap();
        assert_eq!(error.message, "install.sh failed (exit status 3)");
    }

    #[test]
    fn the_daemon_hands_the_service_to_the_newest_koi_once_per_change() {
        let dir = tempfile::tempdir().unwrap();
        let koi = |name: &str, version: &str| {
            let path = dir.path().join(name).join("koi");
            executable(&path, &format!("#!/bin/sh\necho 'koi {version}'\n"));
            path
        };
        let brew = koi("brew", "0.1.2");
        let app = koi("app", "0.1.3");
        let script = koi("script", "0.1.0");
        let missing = dir.path().join("gone/koi");
        let candidates = [brew.clone(), app.clone(), script.clone(), missing];
        let runs = Version::parse("0.1.0").unwrap();
        let mut seen = HashMap::new();

        assert_eq!(scan(&mut seen, &candidates, &runs), Some(app.clone()));
        assert_eq!(
            scan(&mut seen, &candidates, &runs),
            Some(app.clone()),
            "retry the handoff until the daemon changes"
        );

        let later = |seconds| SystemTime::now() + Duration::from_secs(seconds);
        fs::write(&brew, "#!/bin/sh\necho 'koi 0.2.0'\n").unwrap();
        File::options()
            .write(true)
            .open(&brew)
            .unwrap()
            .set_modified(later(60))
            .unwrap();
        assert_eq!(scan(&mut seen, &candidates, &runs), Some(brew.clone()));

        fs::write(&script, "#!/bin/sh\necho 'koi 0.0.9'\n").unwrap();
        File::options()
            .write(true)
            .open(&script)
            .unwrap()
            .set_modified(later(120))
            .unwrap();
        assert_eq!(
            scan(&mut seen, &candidates, &runs),
            Some(brew.clone()),
            "an older koi does not hide the cached newer install"
        );
        assert_eq!(
            scan(
                &mut HashMap::new(),
                &candidates,
                &Version::parse("0.3.0").unwrap()
            ),
            None
        );
    }

    #[test]
    fn handoff_retries_cached_versions_without_running_unchanged_binaries_again() {
        let dir = tempfile::tempdir().unwrap();
        let koi = dir.path().join("koi");
        executable(
            &koi,
            "#!/bin/sh\necho probe >> \"$0.calls\"\necho 'koi 0.2.0'\n",
        );
        let mut seen = HashMap::new();
        let candidates = [koi.clone()];
        for _ in 0..3 {
            assert_eq!(
                scan(&mut seen, &candidates, &Version::parse("0.1.0").unwrap()),
                Some(koi.clone())
            );
        }
        assert_eq!(
            scan(&mut seen, &candidates, &Version::parse("0.2.0").unwrap()),
            None
        );
        assert_eq!(
            fs::read_to_string(koi.with_extension("calls")).unwrap(),
            "probe\n"
        );
    }

    #[test]
    fn homebrew_without_an_environment_prefix_is_found_from_the_running_koi() {
        let dir = tempfile::tempdir().unwrap();
        let opt = dir.path().join("custom/opt/koidrive/bin/koi");
        executable(&opt, "#!/bin/sh\nexit 0\n");
        let installs = Installs::under(None, None);
        for running in [
            opt.clone(),
            dir.path().join("custom/Cellar/koidrive/0.1.0/bin/koi"),
        ] {
            assert_eq!(
                installs.pick(|path| path == opt && path.is_file(), &[&running]),
                Some(Install::Homebrew {
                    prefix: dir.path().join("custom")
                })
            );
        }
        fs::remove_file(&opt).unwrap();
        assert_eq!(installs.pick(Path::is_file, &[&opt]), None);
    }
}
