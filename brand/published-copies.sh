#!/usr/bin/env bash
# The brand files published to wyrd.foo with the user docs, and their sources.
# `check` fails when a copy differs from its source, and reads committed files
# only, so CI runs it without agent-svgtools or ImageMagick. `write` makes each
# copy from its source; build.sh runs it.
set -euo pipefail
cd "$(dirname "$0")/.."

# Source and copy, relative to the repository root.
copies=(
  "brand/xolotl-icon.svg docs/user/assets/logo.svg"
  "brand/xolotl-icon-dark.svg docs/user/assets/logo-dark.svg"
  "ui/public/favicon.svg docs/user/assets/favicon.svg"
)

check() {
  local pair source copy failed=0
  for pair in "${copies[@]}"; do
    read -r source copy <<<"$pair"
    if ! cmp -- "$source" "$copy"; then
      echo "$copy must match $source. Run brand/published-copies.sh write." >&2
      failed=$((failed + 1))
    fi
  done
  if ((failed > 0)); then
    echo "$failed of ${#copies[@]} published copies do not match." >&2
    return 1
  fi
  echo "All ${#copies[@]} published copies match."
}

write() {
  local pair source copy
  for pair in "${copies[@]}"; do
    read -r source copy <<<"$pair"
    cp -- "$source" "$copy"
  done
}

case "${1:-}" in
  check) check ;;
  write) write ;;
  *)
    echo "usage: $0 check|write" >&2
    exit 2
    ;;
esac
