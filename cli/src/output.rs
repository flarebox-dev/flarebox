// SPDX-License-Identifier: Apache-2.0

use std::io::{self, IsTerminal};

use cliclack::Theme as _;

use crate::report::Outcome;

#[derive(Clone, Copy, Debug, Default)]
pub enum Presentation {
    #[default]
    Text,
    Value,
    Literal,
}

pub enum Note<'a> {
    Information(&'a str),
    Error(&'a str),
    Usage(&'a str),
}

struct Theme;

impl cliclack::Theme for Theme {}

pub fn rich(command: &str) -> bool {
    !matches!(command, "update" | "_daemon" | "_reconcile")
        && !crate::terminal::plain_requested()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
}

pub fn print(
    command: &str,
    presentation: Presentation,
    outcome: Outcome,
    exit: u8,
    text: &str,
    note: Note<'_>,
    pending: Option<&str>,
) {
    let (note, error, prefix) = match note {
        Note::Information(text) => (text, false, ""),
        Note::Error(text) => (text, true, "koi: "),
        Note::Usage(text) => (text, true, ""),
    };
    if !rich(command) {
        print!("{text}");
        if !note.is_empty() {
            eprint!("{prefix}{note}");
        }
        if let Some(pending) = pending {
            eprintln!("{pending}");
        }
        return;
    }
    crate::terminal::colors(false);
    let theme = Theme;
    let result_symbol = match (outcome, exit) {
        (Outcome::Ok, 0) => theme.submit_symbol(),
        _ => theme.info_symbol(),
    };
    match presentation {
        Presentation::Text if !text.is_empty() => {
            print!(
                "{}",
                theme.format_log_with_spacing(text.trim_end(), &result_symbol, false)
            );
        }
        _ => print!("{text}"),
    }
    if !note.is_empty() {
        let symbol = match error {
            true => theme.error_symbol(),
            false => theme.info_symbol(),
        };
        eprint!(
            "{}",
            theme.format_log_with_spacing(note.trim_end(), &symbol, false)
        );
    }
    if let Some(pending) = pending {
        eprint!(
            "{}",
            theme.format_log_with_spacing(pending, &theme.warning_symbol(), false)
        );
    }
}
