// SPDX-License-Identifier: Apache-2.0

//! The CLI's side of telemetry (docs/guide/telemetry.md): the question in `koi setup`, `koi telemetry ...`, the
//! command and setup-step events of one run, and the daemon's loop that moves the sums to disk and sends them.

use std::future::Future;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use koidrive_driver::Paths;
use koidrive_telemetry::schema::{self, Client, Counter, Outcome, SetupFlow, SetupStep, magnitude};
use koidrive_telemetry::{
    Environment, Event, InstallId, Mode, Recorder, Sent, Source, Store, Uploader,
    install_panic_hook, should_ask,
};
use serde::Serialize;

use crate::error::CliError;
use crate::report::Done;

/// How often the daemon moves its sums to disk and sends what is due.
const TICK: Duration = Duration::from_secs(10 * 60);
/// How long a setup command waits at exit to send its events; the daemon sends them later otherwise.
const EXIT_SEND: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(clap::Subcommand)]
pub enum TelemetryAction {
    /// Show whether usage reports are sent, why, and what waits to be sent.
    Status,
    /// Send anonymous usage reports.
    On,
    /// Stop sending usage reports, and delete the install id and everything not sent yet.
    Off,
    /// Print the reports that wait to be sent, as the daemon would send them. Sends nothing.
    Preview,
    /// Draw a new install id and delete everything not sent yet.
    Reset,
}

/// The recorder of this run, the install id its sums belong to and the flow its setup steps belong to.
struct Run {
    store: Store,
    recorder: Recorder,
    /// The profile's install id at the last flush. Sums recorded under it are dropped when it changed meanwhile.
    id: Mutex<Option<InstallId>>,
    /// The setup flow this run is in, once it is known: `koi setup` learns it from the person's choice.
    flow: Mutex<Option<SetupFlow>>,
}

static RUN: OnceLock<Run> = OnceLock::new();

fn var(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(name)
}

/// Applies `--telemetry`, asks the `koi setup` question when `ask` (a guided setup) and nobody decided yet, and
/// starts recording this run when telemetry is on. A failure here never stops the command.
pub async fn start(
    paths: &Paths,
    switch: Option<Switch>,
    ask: bool,
    command: schema::Command,
    flow: Option<SetupFlow>,
) {
    let store = Store::new(&paths.state_dir);
    if let Some(switch) = switch
        && let Err(error) = store.set(switch == Switch::On)
    {
        tracing::warn!(%error, "the telemetry choice could not be stored");
    }
    if ask {
        match store.settings() {
            Ok(settings) if should_ask(var, settings.enabled) => {
                if let Some(on) = question().await
                    && let Err(error) = store.set(on)
                {
                    tracing::warn!(%error, "the telemetry choice could not be stored");
                }
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "the telemetry settings cannot be read"),
        }
    }
    let on = matches!(
        store.status(var).map(|status| status.mode),
        Ok(Mode::On | Mode::Debug)
    );
    let recorder = if on { Recorder::on() } else { Recorder::off() };
    recorder.count(Counter::Command(command), 1);
    let id = on
        .then(|| {
            (store.install_id())
                .inspect_err(
                    |error| tracing::debug!(%error, "the telemetry install id cannot be stored"),
                )
                .ok()
        })
        .flatten();
    if on {
        install_panic_hook(store.clone());
    }
    let _ = RUN.set(Run {
        store,
        recorder,
        id: Mutex::new(id),
        flow: Mutex::new(flow),
    });
}

/// Sets the flow this run's setup steps belong to, for `koi setup` once the person chose one.
pub fn set_flow(flow: SetupFlow) {
    if let Some(run) = RUN.get()
        && let Ok(mut current) = run.flow.lock()
    {
        *current = Some(flow);
    }
}

fn flow() -> Option<SetupFlow> {
    RUN.get()
        .and_then(|run| run.flow.lock().ok().and_then(|flow| *flow))
}

/// The question, until it gets an answer: Enter or y is yes, n is no. None when stdin closes.
async fn question() -> Option<bool> {
    println!("Send anonymous usage reports to the Koidrive project?");
    println!("Reports hold which commands ran, where setup failed, crash locations, and your OS");
    println!("and Koidrive version. They never hold file names, paths, contents, your server");
    println!("address or your account.");
    println!("See a report: koi telemetry preview. Turn off: koi telemetry off.");
    loop {
        let answer = crate::setup::prompt("Send reports? [Y/n] ").await.ok()?;
        match answer.to_ascii_lowercase().as_str() {
            "" | "y" | "yes" => return Some(true),
            "n" | "no" => return Some(false),
            _ => {}
        }
    }
}

/// Runs one setup step and records how it ended and how long it took. A step dropped before it ends, by Ctrl-C or
/// a signal, is recorded as aborted.
pub async fn step<T, E>(step: SetupStep, work: impl Future<Output = Result<T, E>>) -> Result<T, E> {
    let mut running = Running {
        step,
        started: Instant::now(),
        outcome: Outcome::Aborted,
    };
    let result = work.await;
    running.outcome = if result.is_ok() {
        Outcome::Ok
    } else {
        Outcome::Failed
    };
    result
}

/// A step in progress, which records itself when it is dropped.
struct Running {
    step: SetupStep,
    started: Instant,
    outcome: Outcome,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(run) = RUN.get()
            && let Some(flow) = flow()
        {
            run.recorder.event(Event::SetupStep {
                day: koidrive_telemetry::today(),
                flow,
                step: self.step,
                outcome: self.outcome,
                cloudflare_code: None,
                seconds_magnitude: magnitude(self.started.elapsed().as_secs()),
            });
        }
    }
}

/// Moves what this run recorded to disk. A setup command also tries to send it, for at most two seconds.
pub async fn finish() {
    let Some(run) = RUN.get() else { return };
    flush();
    if flow().is_some() && run.recorder.is_on() {
        let _ = tokio::time::timeout(EXIT_SEND, send(&run.store, Client::CliCommand)).await;
    }
}

/// Moves what this run recorded to disk, now, under the install id it was recorded under: after `koi telemetry
/// reset`, or off and then on, those sums are dropped and the next ones belong to the new id. A command calls it
/// before it exits on a signal.
pub fn flush() {
    let Some(run) = RUN.get() else { return };
    let pending = run.recorder.take();
    let Ok(mut id) = run.id.lock() else { return };
    if let Some(recorded) = id.as_ref()
        && matches!(
            run.store.status(var).map(|status| status.mode),
            Ok(Mode::On | Mode::Debug)
        )
        && let Err(error) = run.store.absorb_as(pending, recorded)
    {
        tracing::debug!(%error, "telemetry could not be written");
    }
    if let Ok(settings) = run.store.settings() {
        *id = settings.install_id;
    }
}

async fn send(store: &Store, client: Client) -> Option<Sent> {
    let mode = store.status(var).ok()?.mode;
    let uploader = Uploader::from_env(var)
        .inspect_err(|error| tracing::debug!(%error, "telemetry cannot be sent"))
        .ok()?;
    let environment = Environment::detect(client, env!("CARGO_PKG_VERSION"));
    koidrive_telemetry::send(
        store,
        uploader.as_ref(),
        &environment,
        mode,
        koidrive_telemetry::today(),
    )
    .await
    .inspect_err(|error| tracing::debug!(%error, "telemetry could not be read or written"))
    .ok()
}

/// The daemon's loop: every ten minutes it moves the sums to disk and sends what is due. When it stops it only moves
/// them to disk, so that stopping never waits on the network; a later run sends them. It reads the choice again each time, so `koi telemetry off` stops it without a restart.
pub async fn run_daemon(mut stop: tokio::sync::oneshot::Receiver<()>) {
    let Some(run) = RUN.get() else { return };
    loop {
        let stopping = tokio::select! {
            _ = tokio::time::sleep(TICK) => false,
            _ = &mut stop => true,
        };
        flush();
        if !stopping {
            send(&run.store, Client::CliDaemon).await;
        }
        if stopping {
            return;
        }
    }
}

/// `koi telemetry ...`'s result as JSON data.
#[derive(Serialize)]
struct StatusData {
    /// `on`, `off` or `debug`.
    mode: &'static str,
    /// What decided the mode: `do_not_track`, `variable`, `choice` or `unset`.
    source: &'static str,
    install_id: Option<InstallId>,
    waiting_days: usize,
    waiting_events: usize,
    /// Whether this build can send: it has a project token.
    sends: bool,
}

#[derive(Serialize)]
struct ChoiceData {
    enabled: bool,
    install_id: Option<InstallId>,
}

/// `koi telemetry ...`.
pub fn command(paths: &Paths, action: TelemetryAction) -> Result<Done, CliError> {
    let store = Store::new(&paths.state_dir);
    let failed = |error: std::io::Error| {
        CliError::from(format!(
            "the telemetry files in {}: {error}",
            paths.state_dir.display()
        ))
    };
    let overridden = |choice: bool| -> Result<&'static str, CliError> {
        let status = store.status(var).map_err(failed)?;
        Ok(match (status.mode != Mode::Off) == choice {
            true => "",
            false if status.source == Source::DoNotTrack => {
                "DO_NOT_TRACK keeps reports off until it is unset.\n"
            }
            false => "KOIDRIVE_TELEMETRY overrides this choice until it is unset.\n",
        })
    };
    match action {
        TelemetryAction::Status => {
            let settings = store.settings().map_err(failed)?;
            let status = store.status(var).map_err(failed)?;
            let on = status.mode != Mode::Off;
            let (source, why) = match status.source {
                Source::DoNotTrack => ("do_not_track", "DO_NOT_TRACK is set"),
                Source::Variable => ("variable", "KOIDRIVE_TELEMETRY is set"),
                Source::Stored => ("choice", "your choice"),
                Source::Unset => ("unset", "not asked yet"),
            };
            let (mode, shown) = match status.mode {
                Mode::Off => ("off", "off"),
                Mode::On => ("on", "on"),
                Mode::Debug => ("debug", "debug (printed to stderr, never sent)"),
            };
            let queued = store.peek().map_err(failed)?;
            let sends = Uploader::from_env(var).ok().flatten().is_some();
            let mut text = format!("Usage reports: {shown} ({why})\n");
            if let Some(id) = &settings.install_id {
                text.push_str(&format!("Install id: {id}\n"));
            }
            text.push_str(&format!(
                "Waiting to send: {} days, {} events\n",
                queued.days.len(),
                queued.events.len()
            ));
            if status.mode == Mode::On && !sends {
                text.push_str(
                    "This build has no project token, so reports wait on this machine and are not sent.\n",
                );
            }
            text.push_str(if on {
                "Turn off: koi telemetry off\n"
            } else {
                "Turn on: koi telemetry on\n"
            });
            let data = StatusData {
                mode,
                source,
                install_id: settings.install_id,
                waiting_days: queued.days.len(),
                waiting_events: queued.events.len(),
                sends,
            };
            Ok(Done::ok(data, text))
        }
        TelemetryAction::On => {
            let settings = store.set(true).map_err(failed)?;
            let data = ChoiceData {
                enabled: true,
                install_id: settings.install_id,
            };
            let text = format!(
                "Usage reports are on. See what is sent with koi telemetry preview.\n{}",
                overridden(true)?
            );
            Ok(Done::ok(data, text).then(&["koi", "telemetry", "preview"]))
        }
        TelemetryAction::Off => {
            store.set(false).map_err(failed)?;
            let data = ChoiceData {
                enabled: false,
                install_id: None,
            };
            let text = format!(
                "Usage reports are off. The install id and the reports not sent yet are deleted.\n{}",
                overridden(false)?
            );
            Ok(Done::ok(data, text))
        }
        TelemetryAction::Preview => {
            let queued = store.peek().map_err(failed)?;
            let id = store.settings().map_err(failed)?.install_id;
            let (Some(id), false) = (id, queued.is_empty()) else {
                return Ok(Done::ok(
                    serde_json::Value::Null,
                    "Nothing waits to be sent.\n",
                ));
            };
            let token = Uploader::from_env(var)
                .ok()
                .flatten()
                .map(|uploader| uploader.token().to_owned())
                .unwrap_or_default();
            let environment = Environment::detect(Client::CliDaemon, env!("CARGO_PKG_VERSION"));
            let body = koidrive_telemetry::batch(&token, &id, &environment, &queued, &[]);
            let text = format!(
                "{}\n",
                serde_json::to_string_pretty(&body).map_err(|error| error.to_string())?
            );
            Ok(Done::ok(body, text))
        }
        TelemetryAction::Reset => match store.reset().map_err(failed)? {
            Some(id) => {
                let text = format!("New install id: {id}. The reports not sent yet are deleted.\n");
                let data = ChoiceData {
                    enabled: true,
                    install_id: Some(id),
                };
                Ok(Done::ok(data, text))
            }
            None => {
                let data = ChoiceData {
                    enabled: false,
                    install_id: None,
                };
                let text = "Usage reports are off, so there is no install id to reset.\n";
                Ok(Done::ok(data, text))
            }
        },
    }
}
