#!/usr/bin/env bash
# Capture real container workflows. Setup is in README.md beside this file.
set -euo pipefail
: "${MODELS_EVIDENCE_DIR:?Set an absolute output directory outside the checkout}"
mkdir -p "$MODELS_EVIDENCE_DIR"
printf '{"status":"failed","reason":"capture did not finish"}\n' > "$MODELS_EVIDENCE_DIR/result.json"
ab() { agent-browser --session models-pricing-evidence "$@"; }
control() { curl -fsS "http://127.0.0.1:18888/control?$1"; }
ab --cdp "${MODELS_BROWSER_CDP:-http://127.0.0.1:19222}" open http://127.0.0.1:28317
ab set viewport 1200 1000
ab wait h1
if [[ "$(ab get text body)" == *'Sign in with your identity provider'* ]]; then
  ab find role link click --name 'Sign in' --exact
  ab wait --text 'Continue as demo operator'
  ab find role link click --name 'Continue as demo operator'
  ab wait --text 'Overview'
fi
ab find role link click --name Models --exact
ab wait --text 'Provider discovery'
ab find role button click --name 'Light theme' --exact
control 'status=200&delay=5000' > /dev/null
ab record start "$MODELS_EVIDENCE_DIR/models-pricing.webm"
ab find role button click --name 'Refresh models' --exact
ab wait --text 'Refreshing prices…'
ab screenshot "$MODELS_EVIDENCE_DIR/refreshing.png"
ab eval --stdin <<'JS'
if (!Array.from(document.querySelectorAll('button')).some(b => b.disabled && b.textContent.includes('Refreshing models'))) throw new Error('refresh button accepted duplicate submissions');
JS
ab wait --text 'Pricing refresh succeeded'
ab eval --stdin <<'JS'
if (!document.body.innerText.includes('$0.005')) throw new Error('small price lost precision');
JS
ab screenshot "$MODELS_EVIDENCE_DIR/success.png"
control 'status=503&delay=0' > /dev/null
ab find role button click --name 'Refresh models' --exact
ab wait --text 'Latest pricing refresh failed'
ab wait --text '503 Service Unavailable'
ab wait --text 'Cached registry prices are'
ab screenshot "$MODELS_EVIDENCE_DIR/manual-failure.png"
ab record stop
ab find role link click --name Overview --exact
control 'status=502' > /dev/null
before=$(control '' | node -pe 'JSON.parse(require("fs").readFileSync(0,"utf8")).registryRequests')
echo 'Waiting for the real catalog maintenance timer (60 seconds maximum).'
for attempt in $(seq 1 65); do
  current=$(control '' | node -pe 'JSON.parse(require("fs").readFileSync(0,"utf8")).registryRequests')
  if ((current > before)); then break; fi
  sleep 1
done
if ((current <= before)); then echo 'Scheduled refresh never reached registry' >&2; exit 1; fi
ab find role link click --name Models --exact
ab wait --text '502 Bad Gateway'
ab wait --text 'Cached registry prices are'
ab screenshot "$MODELS_EVIDENCE_DIR/scheduled-failure.png"
ab get text body > "$MODELS_EVIDENCE_DIR/scheduled-failure.txt"
ab set viewport 390 844
ab screenshot --full "$MODELS_EVIDENCE_DIR/phone-failure.png"
ab eval --stdin <<'JS'
if (document.documentElement.scrollWidth > innerWidth) throw new Error('page overflows phone viewport');
JS
ab errors > "$MODELS_EVIDENCE_DIR/browser-errors.txt"
printf '{"status":"passed","manual_success":true,"manual_failure":503,"scheduled_failure":502,"small_price":"$0.005","disabled_while_pending":true,"phone_overflow":false}\n' > "$MODELS_EVIDENCE_DIR/result.json"
ab close
