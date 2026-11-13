// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, BufRead, IsTerminal, Write};
use std::os::fd::OwnedFd;
use std::process::{ExitCode, Stdio};

use koidrive_ipc::Code;
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::error::CliError;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Interaction {
    Direct,
    Plain,
    Rich,
}

#[derive(Serialize, Deserialize)]
enum Prompt {
    Input {
        question: String,
        secret: bool,
    },
    Select {
        question: String,
        items: Vec<String>,
    },
    Confirm {
        question: String,
        default: bool,
    },
}

#[derive(Serialize, Deserialize)]
enum Answer {
    Text(String),
    Selection(usize),
    Confirmation(bool),
}

#[derive(Debug, Serialize, Deserialize)]
pub enum PromptFailure {
    Cancelled,
    Closed,
    Io(String),
}

impl From<io::Error> for PromptFailure {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::Interrupted => Self::Cancelled,
            io::ErrorKind::UnexpectedEof => Self::Closed,
            _ => Self::Io(error.to_string()),
        }
    }
}

impl From<PromptFailure> for CliError {
    fn from(error: PromptFailure) -> Self {
        match error {
            PromptFailure::Cancelled => {
                CliError::new(Code::Interrupted, "cancelled on the terminal")
            }
            PromptFailure::Closed => CliError::from("stdin closed before an answer"),
            PromptFailure::Io(message) => CliError::from(message),
        }
    }
}

struct ActivePrompt {
    child: Child,
    saved: SavedTerminal,
}

struct SavedTerminal {
    input: OwnedFd,
    original: Termios,
}

impl Drop for SavedTerminal {
    fn drop(&mut self) {
        let _ = tcsetattr(&self.input, OptionalActions::Now, &self.original);
    }
}

pub struct Terminal {
    interaction: Interaction,
    active: Mutex<Option<ActivePrompt>>,
}

fn nonempty(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn plain_requested() -> bool {
    std::env::var_os("KOI_PLAIN").is_some_and(|value| value == "1")
        || std::env::var_os("TERM").is_some_and(|value| value == "dumb")
        || std::env::args_os()
            .skip(1)
            .take_while(|value| value != "--")
            .any(|value| value == "--plain")
}

pub fn clap_color() -> clap::ColorChoice {
    if nonempty("NO_COLOR") || plain_requested() {
        clap::ColorChoice::Never
    } else {
        clap::ColorChoice::Auto
    }
}

fn colors(plain: bool) {
    let forced =
        std::env::var_os("CLICOLOR_FORCE").is_some_and(|value| !value.is_empty() && value != "0");
    let enabled = !plain
        && !nonempty("NO_COLOR")
        && io::stderr().is_terminal()
        && std::env::var_os("TERM").is_none_or(|value| value != "dumb")
        && (forced || std::env::var_os("CLICOLOR").is_none_or(|value| value != "0"));
    console::set_colors_enabled(enabled);
    console::set_colors_enabled_stderr(enabled);
}

impl Terminal {
    pub fn new(plain: bool, guided: bool) -> Self {
        let width = console::Term::stderr()
            .size_checked()
            .map(|(_, width)| width);
        let interaction = Self::interaction(guided, plain || plain_requested(), width);
        colors(interaction != Interaction::Rich);
        Self {
            interaction,
            active: Mutex::new(None),
        }
    }

    fn interaction(guided: bool, plain: bool, width: Option<u16>) -> Interaction {
        match (guided, plain, width) {
            (false, _, _) => Interaction::Direct,
            (true, false, Some(60..)) => Interaction::Rich,
            _ => Interaction::Plain,
        }
    }

    pub fn guided(&self) -> bool {
        self.interaction != Interaction::Direct
    }

    pub fn message(&self, message: &str) {
        let message = crate::printable(message);
        if self.interaction == Interaction::Rich {
            let _ = cliclack::log::info(message);
        } else {
            eprintln!("{message}");
        }
    }

    pub fn intro(&self) {
        let title = format!("Koidrive {}", env!("CARGO_PKG_VERSION"));
        if self.interaction == Interaction::Rich {
            let _ = cliclack::intro(title);
        } else {
            eprintln!("{title}\n");
        }
    }

    pub async fn input(&self, question: &str, secret: bool) -> Result<String, PromptFailure> {
        match self
            .prompt(Prompt::Input {
                question: question.into(),
                secret,
            })
            .await?
        {
            Answer::Text(text) => Ok(text),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    pub async fn select(&self, question: &str, items: Vec<String>) -> Result<usize, PromptFailure> {
        if items.is_empty() {
            return Err(PromptFailure::Io("there are no choices to select".into()));
        }
        match self
            .prompt(Prompt::Select {
                question: question.into(),
                items,
            })
            .await?
        {
            Answer::Selection(index) => Ok(index),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    pub async fn confirm(&self, question: &str, default: bool) -> Result<bool, PromptFailure> {
        match self
            .prompt(Prompt::Confirm {
                question: question.into(),
                default,
            })
            .await?
        {
            Answer::Confirmation(answer) => Ok(answer),
            _ => Err(PromptFailure::Io(
                "the terminal returned the wrong answer type".into(),
            )),
        }
    }

    async fn prompt(&self, request: Prompt) -> Result<Answer, PromptFailure> {
        if !self.guided() || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            return Err(PromptFailure::Io(
                "this question needs a terminal on stdin and stderr".into(),
            ));
        }
        let mut active = self.active.lock().await;
        let input = rustix::io::dup(io::stdin()).map_err(io::Error::from)?;
        let original = tcgetattr(&input).map_err(io::Error::from)?;
        let saved = SavedTerminal { input, original };
        if self.interaction == Interaction::Rich
            || matches!(&request, Prompt::Input { secret: true, .. })
        {
            let mut hidden = saved.original.clone();
            if self.interaction == Interaction::Rich {
                hidden.make_raw();
                hidden.output_modes = saved.original.output_modes;
            }
            hidden.local_modes.remove(LocalModes::ECHO);
            tcsetattr(&saved.input, OptionalActions::Now, &hidden).map_err(io::Error::from)?;
        }
        let child = tokio::process::Command::new(std::env::current_exe()?)
            .arg("_prompt")
            .arg(if self.interaction == Interaction::Rich {
                "rich"
            } else {
                "plain"
            })
            .arg(serde_json::to_string(&request).expect("prompt requests serialize"))
            .stdin(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        *active = Some(ActivePrompt { child, saved });
        let result = async {
            let running = active.as_mut().expect("the helper was just started");
            let mut output = Vec::new();
            let stdout = running
                .child
                .stdout
                .take()
                .expect("the helper has a stdout pipe")
                .take(64 * 1024 + 1);
            let mut stdout = stdout;
            stdout.read_to_end(&mut output).await?;
            drop(stdout);
            if output.len() > 64 * 1024 {
                return Err(PromptFailure::Io("the terminal answer is too long".into()));
            }
            let status = running.child.wait().await?;
            if !status.success() {
                return Err(PromptFailure::Io(
                    "the terminal helper stopped before returning an answer".into(),
                ));
            }
            serde_json::from_slice::<Result<Answer, PromptFailure>>(&output).map_err(|_| {
                PromptFailure::Io("the terminal helper returned an invalid answer".into())
            })?
        }
        .await;
        self.cleanup(&mut active).await;
        result
    }

    async fn cleanup(&self, active: &mut Option<ActivePrompt>) {
        if let Some(running) = active.as_mut() {
            let _ = running.child.kill().await;
            let _ = running.child.wait().await;
        }
        if let Some(running) = active.take() {
            drop(running.saved);
            if self.interaction == Interaction::Rich {
                let _ = console::Term::stderr().show_cursor();
            }
        }
    }

    pub async fn cancel(&self) {
        self.cleanup(&mut *self.active.lock().await).await;
    }

    pub async fn wait<T>(
        &self,
        message: &str,
        work: impl Future<Output = Result<T, CliError>>,
    ) -> Result<T, CliError> {
        struct Progress(Option<cliclack::ProgressBar>);
        impl Drop for Progress {
            fn drop(&mut self) {
                if let Some(progress) = &self.0 {
                    progress.clear();
                }
            }
        }
        let mut progress =
            Progress((self.interaction == Interaction::Rich).then(cliclack::spinner));
        if let Some(progress) = &progress.0 {
            progress.start(message);
        } else {
            self.message(message);
        }
        let result = work.await;
        if let Some(progress) = progress.0.take() {
            if result.is_ok() {
                progress.stop("Done");
            } else {
                progress.error("Failed");
            }
        }
        result
    }
}

pub(crate) fn read_line(mut input: impl BufRead) -> io::Result<String> {
    let mut text = String::new();
    match input.read_line(&mut text)? {
        0 => Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "stdin closed before an answer",
        )),
        _ => Ok(text),
    }
}

fn line(question: &str, secret: bool) -> Result<String, PromptFailure> {
    let input = io::stdin();
    let original = secret
        .then(|| tcgetattr(&input).map_err(io::Error::from))
        .transpose()?;
    if let Some(original) = &original {
        let mut hidden = original.clone();
        hidden.local_modes.remove(LocalModes::ECHO);
        tcsetattr(&input, OptionalActions::Now, &hidden).map_err(io::Error::from)?;
    }
    let result = (|| {
        io::stderr().write_all(question.as_bytes())?;
        io::stderr().flush()?;
        read_line(input.lock())
    })();
    if let Some(original) = original {
        tcsetattr(&input, OptionalActions::Now, &original).map_err(io::Error::from)?;
        eprintln!();
    }
    Ok(result?.trim().to_owned())
}

fn interact(request: Prompt, rich: bool) -> Result<Answer, PromptFailure> {
    match request {
        Prompt::Input { question, secret } => {
            let question = crate::printable(&question);
            let text = if rich && secret {
                cliclack::password(question).interact()?
            } else if rich {
                cliclack::input(question).interact::<String>()?
            } else {
                line(&format!("{question}: "), secret)?
            };
            Ok(Answer::Text(text))
        }
        Prompt::Select { question, items } => {
            if items.is_empty() {
                return Err(PromptFailure::Io("there are no choices to select".into()));
            }
            let question = crate::printable(&question);
            if rich {
                let mut select = cliclack::select(question);
                for (index, item) in items.iter().enumerate() {
                    select = select.item(index, crate::printable(item), "");
                }
                return Ok(Answer::Selection(select.interact()?));
            }
            eprintln!("{question}");
            for (index, item) in items.iter().enumerate() {
                eprintln!("  {}. {}", index + 1, crate::printable(item));
            }
            loop {
                if let Ok(index) =
                    line(&format!("Choose 1 to {}: ", items.len()), false)?.parse::<usize>()
                    && (1..=items.len()).contains(&index)
                {
                    return Ok(Answer::Selection(index - 1));
                }
                eprintln!("Enter one of the numbers shown above.");
            }
        }
        Prompt::Confirm { question, default } => {
            let question = crate::printable(&question);
            if rich {
                return Ok(Answer::Confirmation(
                    cliclack::confirm(question)
                        .initial_value(default)
                        .interact()?,
                ));
            }
            loop {
                let question = format!("{question} {} ", if default { "[Y/n]" } else { "[y/N]" });
                match line(&question, false)?.to_ascii_lowercase().as_str() {
                    "" => return Ok(Answer::Confirmation(default)),
                    "y" | "yes" => return Ok(Answer::Confirmation(true)),
                    "n" | "no" => return Ok(Answer::Confirmation(false)),
                    _ => eprintln!("Enter yes or no."),
                }
            }
        }
    }
}

pub fn helper(args: &[OsString]) -> Option<ExitCode> {
    if args.get(1).is_none_or(|arg| arg != "_prompt") {
        return None;
    }
    if args.len() != 4
        || args
            .get(2)
            .is_none_or(|value| value != "plain" && value != "rich")
    {
        return Some(ExitCode::from(2));
    }
    let Some(request) = args
        .get(3)
        .and_then(|text| text.to_str())
        .and_then(|text| serde_json::from_str(text).ok())
    else {
        return Some(ExitCode::from(2));
    };
    let rich = args.get(2).is_some_and(|value| value == "rich");
    colors(!rich);
    let result = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(async {
            let _interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let _terminate =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
            interact(request, rich)
        }),
        Err(error) => Err(error.into()),
    };
    let output = serde_json::to_vec(&result).expect("terminal answers serialize");
    Some(if io::stdout().write_all(&output).is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_policy_keeps_direct_and_plain_modes_separate() {
        assert_eq!(
            Terminal::interaction(false, false, Some(100)),
            Interaction::Direct
        );
        assert_eq!(
            Terminal::interaction(false, true, Some(100)),
            Interaction::Direct
        );
        assert_eq!(
            Terminal::interaction(true, true, Some(100)),
            Interaction::Plain
        );
        assert_eq!(Terminal::interaction(true, false, None), Interaction::Plain);
        assert_eq!(
            Terminal::interaction(true, false, Some(59)),
            Interaction::Plain
        );
        assert_eq!(
            Terminal::interaction(true, false, Some(60)),
            Interaction::Rich
        );
    }
}
