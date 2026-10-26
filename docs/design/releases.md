# Releases

Versions are managed with [Intentional](https://github.com/wyrd-company/intentional). The day-to-day commands are in the project README; this page explains the setup.

## Configuration

`.intentional/config.yml` defines one release unit, `xolotl`:

- **Path and version:** the unit covers the whole repository (`.`). Its version is projected into `Cargo.toml`, and the projection is committed.
- **Tags:** the template is `{version}`, giving plain tags like `0.1.0`, as the project README asks.
- **Bump mapping:** `pre-1-0-bump-mapping: component`, so before 1.0 a `minor` intent bumps the minor version (`0.0.0` → `0.1.0`) and is listed under Features. With Intentional's default mapping, getting `0.1.0` would take a `major` intent listed under "Breaking".

The baseline is `0.0.0`, the version in `Cargo.toml`. `intentional tag --baseline` creates it; until the tag exists, `intentional check` (and so a release) fails with `missing-baseline`.

The first release is one intent, `.intentional/intents/lucky-otter-1aef.md` (`xolotl: minor`).

## Release tags

`intentional tag --plan` creates an **annotated** tag whose message is a release record:

```
intentional release record
contract: …
generator: …
plan-digest: …
tag-id: release-unit/xolotl/primary
version: 0.1.0
baseline: false
```

The baseline tag carries a record with `baseline: true`.

Intentional finds tags by name, so the release history survives a squash merge of this branch.

`intentional apply` updates `Cargo.toml` and `CHANGELOG.md` but not `Cargo.lock`, so the release steps run `cargo update -w --offline` before committing. Otherwise `--locked` builds fail.

## CI (`.github/workflows/ci.yml`)

On pushes to `main` and on pull requests:
- `cargo fmt --check`, `clippy -D warnings` and `cargo test`;
- the web UI: `pnpm test` and `pnpm build` (which type-checks);
- a Docker build per architecture, each on a native runner (`amd64` on `ubuntu-latest`, `arm64` on `ubuntu-24.04-arm`), with a separate build cache per architecture.

Intentional is not checked here. `intentional check` only validates the config and intent files (it doesn't require a change to carry an intent), so it runs once, as the release gate. A malformed intent therefore shows up when a release is cut, not on the PR that added it.

## Release workflow (`.github/workflows/release.yml`)

Triggered by any `X.Y.Z` tag. A `guard` job decides whether the tag is a real release:

1. Fetch the annotated tag object explicitly (checkout doesn't reliably fetch it) and run `intentional check`.
2. Refuse (fail) unless the tag:
   - is annotated, and its message starts with `intentional release record`;
   - has `tag-id: release-unit/xolotl/primary` and `version` equal to the tag name.
3. If the record has `baseline: true`, stop without failing; nothing is released.
4. Refuse unless the record has a plan digest, `Cargo.toml` has the tag's version, and `CHANGELOG.md` has a `## X.Y.Z` section. Together these prove the applied release was committed.

Only when the guard outputs `release=true` do the other jobs run:
- tests;
- a Linux x86_64 binary on a GitHub release, with notes from the CHANGELOG section;
- the GHCR image `ghcr.io/wyrd-company/xolotl` for amd64 and arm64, tagged `X.Y.Z`, `X.Y` and `latest`;
- after the binary and the image, the code unit is published to wyrd.foo by calling the wyrd.foo v2 reusable workflow (`publish-docs.yml@v2`) with the tag as `ref`.

The code unit publishes from this workflow, not from the tag push on its own: wyrd.foo reads the GitHub release of the tag, the release is created by the binary job, and a release created with `GITHUB_TOKEN` starts no other workflows. The reusable workflow needs the org secrets `WYRD_TOOLS_DOCS_PUBLISHER_APP_ID` and `WYRD_TOOLS_DOCS_PUBLISHER_PRIVATE_KEY`; without them the `docs` job fails after the binary and the image are released.

The guard script was tested locally against a baseline tag, a release tag, a hand-made annotated tag and a lightweight tag.

## Docs publish workflow (`.github/workflows/publish-docs.yml`)

Calls the same reusable workflow when a docs unit tag (`xolotl-docs@X.Y.Z`) is pushed, and on a manual run. The manual run takes the tag or commit as `ref`, and the optional `feature*` inputs. A push to `main` does not publish. To publish again without a new release, run it by hand: **Actions → Publish to wyrd.foo → Run workflow** with the tag, or `gh workflow run publish-docs.yml -f ref=<tag>`.
