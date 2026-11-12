// SPDX-License-Identifier: Apache-2.0

use clap::{ArgAction, CommandFactory};

use crate::report::{ArgumentInfo, Auth, CommandInfo, OptionInfo, Output};

fn auth(command: &str) -> Auth {
    match command {
        "server deploy" | "server add-device" | "server upgrade" | "server delete" => {
            Auth::Cloudflare
        }
        "folder link" | "history trash" | "history versions" | "history restore" => Auth::Device,
        _ if command.starts_with("space ") || command.starts_with("device ") => Auth::Device,
        _ => Auth::None,
    }
}

fn info(command: &clap::Command, path: &str) -> CommandInfo {
    let text =
        |help: Option<&clap::builder::StyledStr>| help.map(ToString::to_string).unwrap_or_default();
    let shown = (command.get_arguments())
        .filter(|arg| !arg.is_hide_set() && !matches!(arg.get_id().as_str(), "help" | "version"));
    let (positional, flags): (Vec<_>, Vec<_>) = shown.partition(|arg| arg.is_positional());
    let values = |arg: &clap::Arg| match arg.get_value_names() {
        Some([name, ..]) => name.to_string(),
        _ => arg.get_id().as_str().to_uppercase(),
    };
    CommandInfo {
        command: path.to_owned(),
        about: text(command.get_about()),
        aliases: command.get_visible_aliases().map(str::to_owned).collect(),
        arguments: (positional.into_iter())
            .map(|arg| ArgumentInfo {
                name: values(arg),
                required: arg.is_required_set(),
                repeated: matches!(arg.get_action(), ArgAction::Append)
                    || arg
                        .get_num_args()
                        .is_some_and(|range| range.max_values() > 1),
                about: text(arg.get_help()),
            })
            .collect(),
        options: (flags.into_iter())
            .filter_map(|arg| {
                Some(OptionInfo {
                    long: arg.get_long()?.to_owned(),
                    short: arg.get_short(),
                    value: arg.get_action().takes_values().then(|| values(arg)),
                    values: (arg.get_possible_values().iter())
                        .filter(|value| !value.is_hide_set())
                        .map(|value| value.get_name().to_owned())
                        .collect(),
                    required: arg.is_required_set(),
                    global: arg.is_global_set(),
                    about: text(arg.get_help()),
                })
            })
            .collect(),
        subcommands: (command.get_subcommands())
            .map(|sub| sub.get_name().to_owned())
            .filter(|name| name != "help")
            .collect(),
        auth: auth(path),
        output: match path {
            "_daemon" => Output::Lines,
            _ => Output::Envelope,
        },
        schema: file_name(path),
    }
}

/// Every command at `path` and below it, from the CLI's own definition.
pub fn commands(path: &str) -> Vec<CommandInfo> {
    fn walk(command: &clap::Command, path: &str, all: &mut Vec<CommandInfo>) {
        all.push(info(command, path));
        for sub in (command.get_subcommands()).filter(|sub| sub.get_name() != "help") {
            walk(sub, format!("{path} {}", sub.get_name()).trim_start(), all);
        }
    }
    let mut root = crate::Cli::command();
    root.build();
    let mut all = Vec::new();
    walk(&root, "", &mut all);
    all.extend(root.find_subcommand("help").map(|help| info(help, "help")));
    all.retain(|info| {
        path.is_empty() || info.command == path || info.command.starts_with(&format!("{path} "))
    });
    all
}

pub fn file_name(command: &str) -> String {
    match command {
        "" => "koi.json".to_owned(),
        command => format!("koi-{}.json", command.replace(' ', "-")),
    }
}
