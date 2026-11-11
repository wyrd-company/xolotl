#!/usr/bin/env bash
# Decides whether RELEASE_TAG is a release of the xolotl code unit: an
# Intentional release record whose applied release is in the working tree.
# Writes release=true|false to GITHUB_OUTPUT when set, and to stdout.
#
#   release=true   a record with baseline: false; release.yml releases it.
#   release=false  a baseline record; nothing to release.
#   exit 1         any other tag; release.yml refuses it.
#
# The annotated tag object must be in the local repository: a checkout of a
# tag ref does not reliably fetch it.
set -euo pipefail

tag=${RELEASE_TAG:?}

refuse() {
  echo "::error::refusing to deploy ${tag}: $1" >&2
  exit 1
}

decide() {
  if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "release=$1" >>"$GITHUB_OUTPUT"; fi
  echo "release=$1"
}

[[ "$(git cat-file -t "refs/tags/${tag}" 2>/dev/null)" == "tag" ]] || refuse "not an annotated tag"
body="$(git cat-file -p "refs/tags/${tag}" | sed '1,/^$/d')"
[[ "$(printf '%s\n' "$body" | head -n1)" == "intentional release record" ]] || refuse "not an Intentional release record"
field() { printf '%s\n' "$body" | sed -n "s/^$1: //p" | head -n1; }

[[ "$(field tag-id)" == "release-unit/xolotl/primary" ]] || refuse "unexpected tag-id $(field tag-id)"
[[ "$(field version)" == "$tag" ]] || refuse "record version $(field version) does not match the tag"
case "$(field baseline)" in
  true)
    echo "::notice::${tag} is a baseline record; nothing to release."
    decide false
    exit 0
    ;;
  false) ;;
  *) refuse "record has no baseline field" ;;
esac
[[ -n "$(field plan-digest)" ]] || refuse "record has no plan digest"

# The tagged commit must carry the applied release.
version="$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)"
[[ "$version" == "$tag" ]] || refuse "Cargo.toml version $version is not $tag (was intentional apply committed?)"
grep -q "^## ${tag}\$" CHANGELOG.md || refuse "CHANGELOG.md has no ${tag} section"
decide true
