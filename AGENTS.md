# CLI and browser parity

When adding or changing an operator CLI capability, deliver its browser workflow
with equivalent status, results and actionable error details. Update the CLI
parity section of [the web UI design](docs/design/web-ui.md), including every
option and the feedback assessment. For process startup, local file validation,
connection selection and output formatting, record the reason for a browser
exclusion there. Verify the affected workflow against the standard container
deployment using the browser alone.

Run `cargo test --locked --test cli_parity` to check command and option coverage.
The check detects added or removed commands and option names; review the inventory
when defaults, accepted values, response fields or behavior change because those
need a human assessment of equivalent browser behavior and feedback.
