#!/usr/bin/env bash
# Regenerate the task-page PNGs listed in docs/web-ui-demo.md.
set -euo pipefail

: "${XOLOTL_DEMO_URL:?Start this script with docs/web-ui-demo.sh}"
output=${1:?Pass the output directory}
mkdir -p -- "$output"
output=$(cd -- "$output" && pwd)

export AGENT_BROWSER_SOCKET_DIR
AGENT_BROWSER_SOCKET_DIR=$(mktemp -d /tmp/xolotl-capture.XXXXXX)
export AGENT_BROWSER_DEFAULT_TIMEOUT=10000
session="xolotl-capture-${AGENT_BROWSER_SOCKET_DIR##*.}"

browser() {
  agent-browser --session "$session" "$@"
}
cleanup() {
  browser close >/dev/null 2>&1 || true
  rm -rf -- "$AGENT_BROWSER_SOCKET_DIR"
}
trap cleanup EXIT

click() {
  browser find role "$1" click --name "$2" --exact
}
choose() {
  click combobox "$1"
  click option "$2"
  browser wait '[role="listbox"]' --state detached
}
page() {
  browser open "$XOLOTL_DEMO_URL/#$1"
  browser wait --text "$2"
}
wait_for_animations() {
  browser wait --fn 'document.getAnimations().every(animation => animation.playState !== "running")'
}
capture() {
  local name=$1
  shift
  browser screenshot "$@" -- "$output/$name.png"
}

if [[ -n ${XOLOTL_CAPTURE_CDP_PORT:-} ]]; then
  browser connect "$XOLOTL_CAPTURE_CDP_PORT"
fi
browser open "$XOLOTL_DEMO_URL/"
browser set viewport 1440 745
browser set media light reduced-motion
browser wait --text 'Sign in with your identity provider'
click link 'Sign in'
browser wait --text 'Reload from OpenBao'
click button 'Light theme'

click button 'Reload from OpenBao'
browser wait --text 'Reload completed'
capture deployment-reload

browser set viewport 1440 900
page /accounts 'Team subscription'
capture accounts-list main
browser set viewport 1440 1170
page /accounts/add 'Start Claude login'
capture add-account
browser wait '#provider-kind'
choose Kind OpenAI-compatible
capture add-provider-account 'section[aria-labelledby="add-providers"] [data-slot="card"]'

browser set viewport 1440 900
page /policies 'New policy'
click link 'New policy'
browser wait '#p-name:enabled'
browser find label Name fill build-agents --exact
browser find label Description fill 'Build and test the project' --exact
browser wait --text 'egress (forward proxy)'
browser find role checkbox check --name 'local gateway' --exact
browser wait 'form button[type="submit"]:enabled'
# The enabled button can still be transitioning out of its disabled colors.
wait_for_animations
capture new-policy
browser find role checkbox check --name codex --exact
click checkbox 'Use Team subscription'
browser wait --text 'No accounts checked, so keys under this policy cannot use codex.'
capture policy-no-accounts '[data-slot="card"]:has(button[aria-label="Use Team subscription"])'

page /keys 'Issue a key'
browser wait --text 'devcontainer/workstation'
click button Rotate…
choose 'Grace period' Custom
browser find label 'Custom grace' fill 45m --exact
wait_for_animations
capture client-key-rotate '[role="dialog"]'
click button Cancel
browser wait '[role="dialog"]' --state detached

choose Policy devcontainer
browser find label 'Client name' fill laptop --exact
browser find label 'Description (optional)' fill 'Laptop devcontainer' --exact
choose Expires Custom
browser find label 'Custom expiry' fill 36h --exact
click button 'Issue key'
browser wait --text 'Issued devcontainer/laptop'
browser wait --text 'Grant it this Vault policy'
browser set viewport 1440 710
capture client-key-issued

browser set viewport 1440 900
page /mcp 'Find notes from the demo project'
click button 'Show tools'
browser wait --text 'Results include the title and a short excerpt.'
capture mcp-header-secret '[data-slot="card"]:has(h2):has(button[aria-expanded="true"])'
choose 'Group by' Tool
browser find label 'Look back' fill 36h --exact
click button 'Apply period'
browser wait --text 'Last 36h'
browser wait '[data-slot="card"]:has(#mcp-look-back) tbody tr'
capture mcp-tool-calls '[data-slot="card"]:has(#mcp-look-back)'

browser set viewport 1440 980
page /usage 'Usage'
browser wait 'main > [data-slot="card"] tbody tr'
browser wait 'section[aria-labelledby="sessions-heading"] tbody tr'
browser wait 'section[aria-labelledby="tools-heading"] tbody tr'
capture usage-sessions

browser set viewport 1440 900
page /models 'qwen3-coder'
browser wait --text 'Pricing refresh succeeded'
capture models-catalog main
