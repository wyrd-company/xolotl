//! Keep the human parity assessment exhaustive as the clap command tree changes.
use std::collections::{BTreeMap, BTreeSet};

use clap::{Arg, ArgAction, Args, Command, CommandFactory};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

const INVENTORY: &str = include_str!("../docs/design/web-ui.md");
type Rows = BTreeMap<String, String>;

fn remote() -> Command {
    let mut command = xolotl::cli::Remote::augment_args(Command::new("remote").disable_help_flag(true));
    command.build();
    command
}

fn parser_control(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version)
}

fn shared_option(arg: &Arg, remote: &Command) -> bool {
    arg.is_global_set()
        && remote.get_arguments().any(|shared| {
            arg.get_id() == shared.get_id()
                && arg.get_long() == shared.get_long()
                && arg.get_short() == shared.get_short()
                && arg.get_env() == shared.get_env()
                && arg.get_default_values() == shared.get_default_values()
                && std::mem::discriminant(arg.get_action()) == std::mem::discriminant(shared.get_action())
        })
}

fn surface(command: &Command, path: &str, remote: &Command, inherited_remote: bool, rows: &mut Rows) {
    let remote_scope =
        inherited_remote || command.get_groups().any(|group| xolotl::cli::Remote::group_id().as_ref() == Some(group.get_id()));
    let mut args = command
        .get_arguments()
        .filter(|arg| !parser_control(arg) && !(remote_scope && shared_option(arg, remote)))
        .map(|arg| {
            (arg.get_index().unwrap_or(0), arg.get_long().map(|long| format!("--{long}")).unwrap_or_else(|| format!("<{}>", arg.get_id())))
        })
        .collect::<Vec<_>>();
    args.sort();
    rows.insert(path.to_string(), args.into_iter().map(|(_, name)| name).collect::<Vec<_>>().join(" "));
    for child in command.get_subcommands().filter(|child| child.get_name() != "help") {
        surface(child, &format!("{path} {}", child.get_name()), remote, remote_scope, rows);
    }
}

/// Read rendered tables only: fenced examples, comments and prose cannot satisfy coverage.
fn documented(source: &str) -> (Rows, Rows, Vec<String>) {
    let (mut commands, mut shared, mut errors) = (Rows::new(), Rows::new(), Vec::new());
    let (mut heading, mut cell) = (String::new(), String::new());
    let mut row = Vec::new();
    let (mut in_inventory, mut shared_table, mut in_heading, mut in_cell) = (false, false, false, false);
    for event in Parser::new_ext(source, Options::ENABLE_TABLES) {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                heading.clear();
                in_heading = true;
            }
            Event::End(TagEnd::Heading(level)) => {
                in_heading = false;
                if level == HeadingLevel::H2 {
                    in_inventory = heading == "CLI parity";
                }
                shared_table = heading == "Shared options and feedback";
            }
            Event::Start(Tag::TableHead | Tag::TableRow) => row.clear(),
            Event::Start(Tag::TableCell) => {
                cell.clear();
                in_cell = true;
            }
            Event::End(TagEnd::TableCell) => {
                row.push(cell.trim().to_string());
                in_cell = false;
            }
            Event::Text(text) | Event::Code(text) => {
                if in_heading {
                    heading.push_str(&text);
                }
                if in_cell {
                    cell.push_str(&text);
                }
            }
            Event::End(TagEnd::TableRow) if in_inventory => {
                if shared_table && row.len() == 2 && row[0].starts_with('-') {
                    if row[1].is_empty() || row[1] == "TBD" {
                        errors.push(format!("shared {}: assessment is empty or TBD", row[0]));
                    }
                    for option in row[0].split_whitespace().filter(|word| word.starts_with('-')) {
                        let option = option.trim_end_matches(',').to_string();
                        if shared.insert(option.clone(), row[1].clone()).is_some() {
                            errors.push(format!("duplicate shared option: {option}"));
                        }
                    }
                } else if row.first().is_some_and(|command| command.starts_with("xolotl")) {
                    if row.len() != 4 {
                        errors.push(format!("{}: expected command, arguments, UI equivalent and feedback cells", row[0]));
                        continue;
                    }
                    if row[2].is_empty() || row[2] == "TBD" {
                        errors.push(format!("{}: UI equivalent is empty or TBD", row[0]));
                    }
                    if !["Equal:", "Partial:", "Gap:", "Excluded:"]
                        .iter()
                        .any(|status| row[3].strip_prefix(status).is_some_and(|detail| !detail.trim().is_empty() && detail.trim() != "TBD"))
                    {
                        errors.push(format!("{}: feedback must start with Equal:, Partial:, Gap: or Excluded: and explain it", row[0]));
                    }
                    let args = if row[1] == "—" { String::new() } else { row[1].clone() };
                    if commands.insert(row[0].clone(), args).is_some() {
                        errors.push(format!("duplicate command: {}", row[0]));
                    }
                }
            }
            _ => {}
        }
    }
    (commands, shared, errors)
}

fn compare(label: &str, documented: &Rows, expected: &Rows, errors: &mut Vec<String>) {
    for (name, args) in expected {
        match documented.get(name) {
            None => errors.push(format!("missing {label} assessment: {name}")),
            Some(actual) if label == "command" && actual != args => {
                errors.push(format!("{name}: expected arguments [{args}], documented [{actual}]"));
            }
            _ => {}
        }
    }
    for name in documented.keys().filter(|name| !expected.contains_key(*name)) {
        errors.push(format!("unknown {label} assessment: {name}"));
    }
}

fn problems(source: &str, mut cli: Command, remote: &Command) -> Vec<String> {
    cli.build();
    let mut expected = Rows::new();
    surface(&cli, "xolotl", remote, false, &mut expected);
    // clap-generated help dispatches to the same help text at any depth.
    expected.insert("xolotl help".into(), "<command>".into());
    let mut shared_options = BTreeSet::new();
    for arg in remote.get_arguments().chain(cli.get_arguments().filter(|arg| parser_control(arg))) {
        shared_options.extend(arg.get_long().map(|long| format!("--{long}")));
        shared_options.extend(arg.get_short().map(|short| format!("-{short}")));
    }
    let shared_options = shared_options.into_iter().map(|name| (name, String::new())).collect();
    let (commands, shared, mut errors) = documented(source);
    compare("command", &commands, &expected, &mut errors);
    compare("shared option", &shared, &shared_options, &mut errors);
    errors
}

#[test]
fn every_cli_command_and_option_has_a_parity_assessment() {
    let errors = problems(INVENTORY, xolotl::cli::Cli::command(), &remote());
    assert!(errors.is_empty(), "Update docs/design/web-ui.md and assess behavior/feedback as AGENTS.md requires:\n{}", errors.join("\n"));
}

#[test]
fn local_options_with_shared_or_parser_names_need_local_coverage() {
    for name in ["url", "token", "json", "version", "help"] {
        for global in [false, true] {
            let cli = xolotl::cli::Cli::command()
                .mut_subcommand("serve", |command| command.disable_help_flag(true).arg(Arg::new(name).long(name).global(global)));
            let errors = problems(INVENTORY, cli, &remote());
            assert_eq!(errors.len(), 1, "{name} global={global}: {errors:?}");
            assert!(errors[0].contains("xolotl serve: expected arguments") && errors[0].contains(&format!("--{name}")), "{errors:?}");
        }
    }
}

#[test]
fn shared_rows_and_their_assessments_are_required() {
    for prefix in ["| `--url`", "| `--token`", "| `--json`", "| `-h`", "| `-V`"] {
        let source = INVENTORY.lines().filter(|line| !line.starts_with(prefix)).collect::<Vec<_>>().join("\n");
        assert!(
            problems(&source, xolotl::cli::Cli::command(), &remote()).iter().any(|e| e.starts_with("missing shared option")),
            "{prefix}"
        );
        let source = INVENTORY
            .lines()
            .map(|line| {
                if line.starts_with(prefix) {
                    format!("{}| |", line.rsplit_once('|').unwrap().0.rsplit_once('|').unwrap().0)
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(problems(&source, xolotl::cli::Cli::command(), &remote()).iter().any(|e| e.contains("assessment is empty")), "{prefix}");
    }
}

#[test]
fn new_remote_option_needs_one_shared_row() {
    let added = Arg::new("new-remote").long("new-remote").global(true);
    let mut remote = remote().arg(added.clone());
    remote.build();
    let cli = xolotl::cli::Cli::command().mut_subcommand("models", |command| command.arg(added));
    assert_eq!(problems(INVENTORY, cli.clone(), &remote), ["missing shared option assessment: --new-remote"]);
    let assessed = INVENTORY.replacen("|---|---|\n", "|---|---|\n| `--new-remote` | Excluded: connection selection. |\n", 1);
    assert!(problems(&assessed, cli, &remote).is_empty());
}

#[test]
fn non_table_and_placeholder_rows_do_not_satisfy_coverage() {
    let row = INVENTORY.lines().find(|line| line.starts_with("| `xolotl reload`")).unwrap();
    for hidden in [format!("```text\n{row}\n```"), format!("<!--\n{row}\n-->"), format!("Prose: {row}")] {
        let source = INVENTORY.replace(row, &hidden);
        assert!(problems(&source, xolotl::cli::Cli::command(), &remote()).contains(&"missing command assessment: xolotl reload".into()));
    }
    let placeholder = "| `xolotl reload` | — | TBD | TBD |";
    assert!(
        problems(&INVENTORY.replace(row, placeholder), xolotl::cli::Cli::command(), &remote())
            .iter()
            .any(|e| e.contains("feedback must start"))
    );
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
