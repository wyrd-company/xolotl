# Xolotl

LLM subscription proxy and AI gateway backed by OpenBao.

## Orient

For CLI and browser changes, start with [the CLI](src/cli.rs),
[admin API](src/server/admin.rs), [UI pages](ui/src/pages), and the
[CLI parity inventory](docs/design/web-ui.md#cli-parity).

## Thar be Dragons

When adding or changing an operator CLI capability, deliver its browser workflow
with equivalent status, results and actionable error details. Update the CLI
parity inventory with every option and its feedback assessment. When browser work
is outside the agreed task scope, mark the row Gap (or Partial for an incomplete
workflow) and link the follow-up issue/task that owns closing it. For process
startup, local file validation, connection selection and output formatting,
record the reason for a browser exclusion there. Verify affected browser workflows
against the standard container deployment using the browser alone.

Run `cargo test --locked --test cli_parity` to check command and option coverage.
The check detects added or removed commands and option names; review the inventory
when defaults, accepted values, response fields or behavior change because those
need a human assessment of equivalent browser behavior and feedback.
