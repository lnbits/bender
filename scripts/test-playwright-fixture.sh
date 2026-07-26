#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
fixture="$root/tests/fixtures/playwright"
cd "$fixture"

if [ ! -d node_modules ]; then
  npm ci
fi
if command -v google-chrome >/dev/null 2>&1; then
  BENDER_PLAYWRIGHT_CHROMIUM=$(command -v google-chrome)
  export BENDER_PLAYWRIGHT_CHROMIUM
else
  npx playwright install chromium
fi

rm -f fixed.txt
rm -rf test-results proof
mkdir -p proof
node server.mjs >proof/server.log 2>&1 &
server_pid=$!
cleanup() {
  kill "$server_pid" 2>/dev/null || true
  wait "$server_pid" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

attempt=0
until curl -fsS http://127.0.0.1:41739/health >/dev/null 2>&1; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 50 ] || {
    echo "Playwright fixture did not become healthy." >&2
    exit 1
  }
  sleep 0.1
done

if npm test >proof/first-report.json 2>proof/first-stderr.log; then
  echo "The deliberate first-attempt Playwright bug unexpectedly passed." >&2
  exit 1
fi
find test-results -type f -print | sort >proof/first-artifacts.txt
grep -Eq 'test-failed-1.png|trace.zip' proof/first-artifacts.txt
cp -R test-results proof/first-failure

: > fixed.txt
npm test >proof/final-report.json 2>proof/final-stderr.log
grep -q '"expected": 1' proof/final-report.json
test ! -s test-results/browser-events.jsonl

echo "Playwright fixture proved fail → artifacts → repair → pass."
