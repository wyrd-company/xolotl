#!/usr/bin/env bash
# Decides whether a pushed docs unit tag (RELEASE_TAG, xolotl-docs@X.Y.Z)
# publishes its docs. Writes publish=true|false to GITHUB_OUTPUT when set, and
# to stdout. Reads GitHub Releases through gh and GH_TOKEN.
#
# The wyrd.foo publisher reads the GitHub Release of the code unit's tag, and
# release.yml creates that Release late, after the tests and every binary. So
# a code release tag on the same commit with no published Release gives
# publish=false: release.yml's docs job publishes the docs of this commit after
# it creates the Release. Every other docs tag gives publish=true.
#
# The working tree must be the tagged commit, with the annotated tag objects.
set -euo pipefail

scripts=$(cd "$(dirname "$0")" && pwd)
tag=${RELEASE_TAG:?}

fail() {
  echo "::error::$1" >&2
  exit 1
}

# release_state <tag>: absent or published. A draft counts as absent, since
# the publisher cannot read it either. Any other answer from GitHub fails, so
# that a failed read does not count as a missing Release.
release_state() {
  local answer
  if answer=$(gh release view "$1" --json isDraft --jq .isDraft 2>&1); then
    case $answer in
      false) echo published ;;
      true) echo absent ;;
      *) fail "unexpected answer for the GitHub Release of $1: $answer" ;;
    esac
  elif [ "$answer" = "release not found" ]; then
    echo absent
  else
    fail "cannot read the GitHub Release of $1: $answer"
  fi
}

# is_code_release <tag>: release-tag.sh decides, as it does for release.yml.
# A tag that release.yml refuses or skips gets no Release from it.
is_code_release() {
  local decision
  decision=$(RELEASE_TAG=$1 GITHUB_OUTPUT='' "$scripts/release-tag.sh" 2>/dev/null) || return 1
  [ "$decision" = "release=true" ]
}

decision=true
for code in $(git tag --points-at "refs/tags/${tag}^{commit}" --list '[0-9]*.[0-9]*.[0-9]*'); do
  is_code_release "$code" || continue
  state=$(release_state "$code")
  if [ "$state" != published ]; then
    echo "::notice::the GitHub Release of $code is not published yet; release.yml publishes these docs after it creates the Release"
    decision=false
  fi
done
if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "publish=$decision" >>"$GITHUB_OUTPUT"; fi
echo "publish=$decision"
