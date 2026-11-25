# Account form browser acceptance

These scripts exercise the embedded UI from the repository Dockerfile. All accounts,
OIDC claims and credentials are synthetic. Admin mutations use browser controls.
The fixture requires Docker, Node, Python 3, and unused ports 35200, 35317, 35444,
and 35445. If those ports are busy, change them in all fixture scripts first.

Create a scratch directory with `mktemp -d /tmp/agent.XXXXXXXX`, record it and all
resources in your ledger, and copy this directory's scripts there. Set
`ACCOUNT_FORM_FIXTURE_DIR` to that scratch path. From the repository:

1. Build `docker build -t xolotl-account-form:acceptance .`.
2. Start OpenBao:

   ```sh
   docker run -d --name xolotl-account-form-vault --network host \
     -e BAO_DEV_ROOT_TOKEN_ID=fixture-root \
     -e BAO_DEV_LISTEN_ADDRESS=127.0.0.1:35200 \
     openbao/openbao:latest server -dev
   ```

3. Run `node "$ACCOUNT_FORM_FIXTURE_DIR/fixture.mjs"` and
   `python3 "$ACCOUNT_FORM_FIXTURE_DIR/vault-proxy.py"` in separate processes.
   Run `node "$ACCOUNT_FORM_FIXTURE_DIR/setup.mjs"` once the vault is ready.
4. Start the app:

   ```sh
   docker run -d --name xolotl-account-form-app --network host \
     -v "$ACCOUNT_FORM_FIXTURE_DIR/config.toml:/etc/xolotl/config.toml:ro" \
     xolotl-account-form:acceptance
   ```

5. Start the browser and install its test dependencies:

   ```sh
   docker run -d --name xolotl-account-form-browser --network host --ipc=host \
     -v "$ACCOUNT_FORM_FIXTURE_DIR:/evidence" \
     mcr.microsoft.com/playwright:v1.58.2-noble tail -f /dev/null
   docker exec xolotl-account-form-browser npm install --prefix /runner \
     playwright@1.58.2 @axe-core/playwright
   docker exec xolotl-account-form-browser node /evidence/signin.cjs
   docker exec xolotl-account-form-browser node /evidence/acceptance.cjs
   docker exec xolotl-account-form-browser node /evidence/a11y.cjs
   docker exec xolotl-account-form-browser node /evidence/capture.cjs
   ```

For the account recovery and usage workflow, run
`docker exec xolotl-account-form-browser node /evidence/recovery.cjs` after
sign-in. It imports a synthetic Codex account through the UI, polls usage while
disabled, checks the returned diagnostics and upstream errors, and triggers an
OAuth refresh while fixture OpenBao writes are denied. It writes
`recovery-result.json`, desktop and phone captures, `account-recovery.png`, and
`account-recovery.webm`. Use the FFmpeg command below with `account-recovery`
in place of `account-forms`. Copy `account-recovery.png` and
`account-recovery.gif` to `docs/user/assets/`.

`acceptance-result.json` starts failed and becomes passed only after all assertions.
`a11y-result.json` records axe violations for the changed import form and priority
dialog in light/dark themes. `capture.cjs` writes `account-import.png`,
`account-priority.png`, `account-priority-error.png`, and `account-forms.webm`.
Convert the recording for the user docs:

```sh
ffmpeg -y -i "$ACCOUNT_FORM_FIXTURE_DIR/account-forms.webm" \
  -vf 'fps=6,scale=880:-1:flags=lanczos,split[a][b];[a]palettegen[p];[b][p]paletteuse' \
  "$ACCOUNT_FORM_FIXTURE_DIR/account-forms.gif"
```

Copy `account-import.png`, `account-priority.png`, and `account-forms.gif` into
`docs/user/assets/` to regenerate the account documentation's media.

Run `node tests/browser/account-form/mutations.mjs <absolute-scratch-output-dir>`
to prove the browser range check, server decimal grammar and minimum-priority
selection regression tests detect broken guards. `CARGO_TARGET_DIR` may point at
an existing build cache. The script restores each source file in `finally` and
writes a fail-first `mutation-result.json` with per-mutant logs. Run the ordinary
checks afterwards on the restored commit.

The session file holds ephemeral browser authentication: keep it local. Screenshots
and the video contain only synthetic fixture data. Replacing the app invalidates
the session; rerun `signin.cjs`. Acceptance can rerun against its own seeded fixture.
To start clean, recreate only this fixture's vault and app and rerun setup.

Cleanup: remove the three named containers (`docker rm -fv` for the app to remove
its anonymous data volume), stop the exact fixture/proxy processes you started,
and remove the scratch directory. Do not remove another task's resources.
