//! Keep the human parity assessment exhaustive as the clap command tree changes.
use std::collections::BTreeMap;

use clap::{ArgAction, Command, CommandFactory};

const INVENTORY: &str = include_str!("../docs/design/web-ui.md");

fn surface(command: &Command, path: &str, rows: &mut BTreeMap<String, String>) {
    let mut args = command
        .get_arguments()
        .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version" | "url" | "token" | "json"))
        .map(|arg| {
            (arg.get_index().unwrap_or(0), arg.get_long().map(|long| format!("--{long}")).unwrap_or_else(|| format!("<{}>", arg.get_id())))
        })
        .collect::<Vec<_>>();
    args.sort();
    rows.insert(path.to_string(), args.into_iter().map(|(_, name)| name).collect::<Vec<_>>().join(" "));
    for child in command.get_subcommands().filter(|child| child.get_name() != "help") {
        surface(child, &format!("{path} {}", child.get_name()), rows);
    }
}

fn documented(source: &str) -> BTreeMap<String, String> {
    let mut rows = BTreeMap::new();
    for line in source.lines().filter(|line| line.starts_with("| `xolotl")) {
        let columns = line.split('|').collect::<Vec<_>>();
        assert!(columns.len() >= 6, "include command, arguments, UI equivalent and feedback assessment: {line}");
        let command = columns[1].trim().trim_matches('`').to_string();
        let args = columns[2].trim().trim_matches('`');
        assert!(!columns[3].trim().is_empty() && !columns[4].trim().is_empty(), "{command}: empty parity assessment");
        assert!(rows.insert(command.clone(), if args == "—" { String::new() } else { args.to_string() }).is_none(), "duplicate {command}");
    }
    rows
}

#[test]
fn every_cli_command_and_option_has_a_parity_assessment() {
    let mut cli = xolotl::cli::Cli::command();
    cli.build();
    let mut expected = BTreeMap::new();
    surface(&cli, "xolotl", &mut expected);
    // clap's generated help command dispatches to the same help text at any depth.
    expected.insert("xolotl help".into(), "<command>".into());
    assert_eq!(
        documented(INVENTORY),
        expected,
        "Update the CLI parity table in docs/design/web-ui.md; assess behavior and feedback as AGENTS.md requires"
    );

    // These shared options and parser actions are assessed once, outside the rows.
    for argument in cli.get_subcommands().find(|c| c.get_name() == "models").unwrap().get_arguments() {
        if matches!(argument.get_id().as_str(), "url" | "token" | "json") {
            assert!(INVENTORY.contains(&format!("`--{}`", argument.get_long().unwrap())));
        }
    }
    assert!(cli.get_arguments().any(|arg| matches!(arg.get_action(), ArgAction::Help)));
    assert!(cli.get_arguments().any(|arg| matches!(arg.get_action(), ArgAction::Version)));
}

#[test]
fn print_recursive_help_listing() {
    fn visit(command: &mut Command, path: &str) {
        println!("=== {path} --help ===\n{}", command.render_long_help());
        for child in command.get_subcommands_mut().filter(|c| c.get_name() != "help") {
            visit(child, &format!("{path} {}", child.get_name()));
        }
    }
    let mut cli = xolotl::cli::Cli::command();
    cli.build();
    visit(&mut cli, "xolotl");
}
