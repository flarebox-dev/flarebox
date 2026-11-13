// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeSet;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{ArgAction, CommandFactory};

use crate::report::{ArgumentInfo, Auth, CommandInfo, GroupInfo, OptionInfo, Output};

fn auth(command: &str) -> Auth {
    match command {
        "server deploy" | "server add-device" | "server upgrade" | "server delete" => {
            Auth::Cloudflare
        }
        "folder link" | "history trash" | "history versions" | "history restore" => Auth::Device,
        _ if ["space ", "device ", "share "]
            .iter()
            .any(|group| command.starts_with(group)) =>
        {
            Auth::Device
        }
        _ => Auth::None,
    }
}

fn shown(arg: &clap::Arg) -> bool {
    !arg.is_hide_set()
        && !matches!(
            arg.get_action(),
            ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
        )
}

fn value_name(arg: &clap::Arg) -> String {
    match arg.get_value_names() {
        Some([name, ..]) => name.to_string(),
        _ => arg.get_id().as_str().to_uppercase(),
    }
}

fn name(arg: &clap::Arg) -> String {
    match arg.get_long() {
        Some(long) if !arg.is_positional() => format!("--{long}"),
        _ => value_name(arg),
    }
}

fn sample(arg: &clap::Arg) -> String {
    let possible = (arg.get_possible_values().first()).map(|value| value.get_name().to_owned());
    let default =
        (arg.get_default_values().first()).map(|value| value.to_string_lossy().into_owned());
    let parses = |value: &&str| {
        let value_arg = clap::Arg::new("value").value_parser(arg.get_value_parser().clone());
        let probe = clap::Command::new("probe")
            .no_binary_name(true)
            .arg(value_arg);
        probe.try_get_matches_from([value]).is_ok()
    };
    let fits = ["x", "1", "0190f0a0-0000-7000-8000-00000000000a"]
        .into_iter()
        .find(parses);
    (possible.or(default))
        .or(fits.map(str::to_owned))
        .unwrap_or_else(|| "x".to_owned())
}

fn words(arg: &clap::Arg) -> Vec<String> {
    if arg.is_positional() {
        return vec![sample(arg)];
    }
    let flag = name(arg);
    match arg.get_action().takes_values() {
        false => vec![flag],
        true if arg.is_require_equals_set() => vec![format!("{flag}={}", sample(arg))],
        true => vec![flag, sample(arg)],
    }
}

fn requires(command: &clap::Command, path: &str, arg: &clap::Arg) -> Vec<String> {
    let missing = |extra: Option<&clap::Arg>| {
        let mut argv = vec!["koi".to_owned()];
        argv.extend(path.split_whitespace().map(str::to_owned));
        let mut positional: Vec<_> = (command.get_arguments())
            .filter(|arg| arg.is_positional())
            .filter(|arg| {
                arg.is_required_set()
                    || extra.is_some_and(|extra| {
                        extra.is_positional() && arg.get_index() <= extra.get_index()
                    })
            })
            .collect();
        positional.sort_by_key(|arg| arg.get_index());
        argv.extend(positional.into_iter().flat_map(words));
        argv.extend(
            extra
                .filter(|extra| !extra.is_positional())
                .into_iter()
                .flat_map(words),
        );
        match crate::Cli::command().try_get_matches_from(&argv) {
            Err(error) if error.kind() == ErrorKind::MissingRequiredArgument => {
                match error.get(ContextKind::InvalidArg) {
                    Some(ContextValue::Strings(names)) => (names.iter())
                        .filter_map(|name| name.split_whitespace().next())
                        .map(|name| name.trim_matches(['<', '>']).to_owned())
                        .collect(),
                    _ => BTreeSet::new(),
                }
            }
            _ => BTreeSet::new(),
        }
    };
    missing(Some(arg))
        .difference(&missing(None))
        .cloned()
        .collect()
}

fn info(command: &clap::Command, path: &str) -> CommandInfo {
    let text =
        |help: Option<&clap::builder::StyledStr>| help.map(ToString::to_string).unwrap_or_default();
    let leaf = !command.has_subcommands() && path != "help";
    let requires = |arg: &clap::Arg| match leaf {
        true => requires(command, path, arg),
        false => Vec::new(),
    };
    let conflicts = |arg: &clap::Arg| -> Vec<String> {
        (command.get_arg_conflicts_with(arg).into_iter())
            .filter(|other| shown(other))
            .map(name)
            .collect()
    };
    let (positional, flags): (Vec<_>, Vec<_>) =
        (command.get_arguments().filter(|arg| shown(arg))).partition(|arg| arg.is_positional());
    let usage = command.clone().render_usage().to_string();
    CommandInfo {
        command: path.to_owned(),
        about: text(command.get_about()),
        usage: usage.trim_start_matches("Usage: ").trim().to_owned(),
        aliases: command.get_visible_aliases().map(str::to_owned).collect(),
        arguments: (positional.into_iter())
            .map(|arg| ArgumentInfo {
                name: value_name(arg),
                required: arg.is_required_set(),
                repeated: matches!(arg.get_action(), ArgAction::Append)
                    || arg
                        .get_num_args()
                        .is_some_and(|range| range.max_values() > 1),
                requires: requires(arg),
                conflicts: conflicts(arg),
                about: text(arg.get_help()),
            })
            .collect(),
        options: (flags.into_iter())
            .filter_map(|arg| {
                let takes = arg.get_action().takes_values();
                Some(OptionInfo {
                    long: arg.get_long()?.to_owned(),
                    short: arg.get_short(),
                    value: takes.then(|| value_name(arg)),
                    value_optional: takes
                        && arg
                            .get_num_args()
                            .is_some_and(|range| range.min_values() == 0),
                    require_equals: arg.is_require_equals_set(),
                    values: (arg.get_possible_values().iter())
                        .filter(|value| !value.is_hide_set())
                        .map(|value| value.get_name().to_owned())
                        .collect(),
                    required: arg.is_required_set(),
                    requires: requires(arg),
                    conflicts: conflicts(arg),
                    global: arg.is_global_set(),
                    about: text(arg.get_help()),
                })
            })
            .collect(),
        groups: (command.get_groups().cloned())
            .filter_map(|mut group| {
                let (required, multiple) = (group.is_required_set(), group.is_multiple());
                (required || !multiple).then(|| GroupInfo {
                    members: (group.get_args())
                        .filter_map(|id| command.get_arguments().find(|arg| arg.get_id() == id))
                        .map(name)
                        .collect(),
                    required,
                    multiple,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(all: &'a [CommandInfo], command: &str) -> &'a CommandInfo {
        all.iter().find(|info| info.command == command).unwrap()
    }

    fn needs(info: &CommandInfo, name: &str) -> Vec<String> {
        let arguments = (info.arguments.iter()).map(|arg| (arg.name.clone(), &arg.requires));
        let options =
            (info.options.iter()).map(|option| (format!("--{}", option.long), &option.requires));
        (arguments.chain(options))
            .find(|(found, _)| found == name)
            .map(|(_, requires)| requires.clone())
            .unwrap_or_default()
    }

    #[test]
    fn the_manifest_alone_builds_an_argv_each_command_parses() {
        let all = commands("");
        let leaves =
            (all.iter()).filter(|info| info.subcommands.is_empty() && info.command != "help");
        let mut built = 0;
        for info in leaves {
            let mut wanted: Vec<String> = (info.arguments.iter())
                .filter(|arg| arg.required)
                .map(|arg| arg.name.clone())
                .collect();
            let options = (info.options.iter()).filter(|option| option.required);
            wanted.extend(options.map(|option| format!("--{}", option.long)));
            let groups = (info.groups.iter()).filter(|group| group.required);
            wanted.extend(groups.map(|group| group.members[0].clone()));
            let mut next = 0;
            while next < wanted.len() {
                for name in needs(info, &wanted[next]) {
                    if !wanted.contains(&name) {
                        wanted.push(name);
                    }
                }
                next += 1;
            }
            let mut argv = vec!["koi".to_owned()];
            argv.extend(info.command.split_whitespace().map(str::to_owned));
            for argument in (info.arguments.iter()).filter(|arg| wanted.contains(&arg.name)) {
                argv.push(argument.name.to_lowercase());
            }
            for option in &info.options {
                let flag = format!("--{}", option.long);
                if wanted.contains(&flag) {
                    argv.push(flag);
                    let value = option.values.first().cloned().unwrap_or("x".to_owned());
                    argv.extend(option.value.as_ref().map(|_| value));
                }
            }
            if let Err(error) = crate::Cli::command().try_get_matches_from(&argv) {
                assert_ne!(
                    error.kind(),
                    ErrorKind::MissingRequiredArgument,
                    "{argv:?}: {error}"
                );
                assert_ne!(
                    error.kind(),
                    ErrorKind::ArgumentConflict,
                    "{argv:?}: {error}"
                );
            }
            built += 1;
        }
        assert!(built > 25, "{built}");
    }

    #[test]
    fn constraints_clap_enforces_are_in_the_manifest() {
        let all = commands("");
        let resolve = find(&all, "resolve");
        assert_eq!(resolve.arguments[0].requires, ["ANSWER"]);
        assert!(resolve.arguments[1].requires.is_empty());
        let retry = find(&all, "folder retry");
        assert_eq!(retry.groups.len(), 1, "{:?}", retry.groups);
        let how = &retry.groups[0];
        assert_eq!((how.required, how.multiple), (true, false));
        assert_eq!(how.members, ["--resend", "--replan"]);
        assert!(
            retry.usage.starts_with("koi folder retry"),
            "{}",
            retry.usage
        );
        let json = (find(&all, "status").options.iter()).find(|option| option.long == "json");
        let json = json.unwrap();
        assert!(json.value_optional && json.require_equals, "{json:?}");
        let restore = find(&all, "history restore");
        assert!(
            restore
                .options
                .iter()
                .any(|option| option.long == "version"),
            "{restore:?}"
        );
        assert!(
            !find(&all, "")
                .options
                .iter()
                .any(|option| option.long == "version")
        );
        let restore = find(&all, "history restore");
        let node = (restore.options.iter())
            .find(|option| option.long == "node")
            .unwrap();
        assert_eq!(node.conflicts, ["--version"]);
        let delete = find(&all, "server delete");
        let jurisdiction = (delete.options.iter()).find(|option| option.long == "jurisdiction");
        assert_eq!(jurisdiction.unwrap().requires, ["--bucket"]);
    }

    #[test]
    fn every_probe_value_passes_its_arguments_parser() {
        let mut root = crate::Cli::command();
        root.build();
        let mut stack = vec![&root];
        while let Some(command) = stack.pop() {
            stack.extend(command.get_subcommands());
            for arg in command
                .get_arguments()
                .filter(|arg| arg.get_action().takes_values())
            {
                let value_arg =
                    clap::Arg::new("value").value_parser(arg.get_value_parser().clone());
                let probe = clap::Command::new("probe")
                    .no_binary_name(true)
                    .arg(value_arg);
                let sample = sample(arg);
                let parsed = probe.try_get_matches_from([&sample]);
                assert!(
                    parsed.is_ok(),
                    "{} --{:?} {sample}",
                    command.get_name(),
                    arg.get_long()
                );
            }
        }
    }
}
