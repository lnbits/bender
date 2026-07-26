#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

if ! cargo fmt --version >/dev/null 2>&1 || ! cargo clippy --version >/dev/null 2>&1; then
  if [ "${BENDER_VALIDATING_IN_NIX:-0}" = 1 ]; then
    echo "rustfmt and clippy are required for release validation." >&2
    exit 1
  fi
  command -v nix >/dev/null 2>&1 || {
    echo "rustfmt and clippy are missing; install them or run validation in the Nix development shell." >&2
    exit 1
  }
  exec nix develop -c env BENDER_VALIDATING_IN_NIX=1 ./scripts/validate-release.sh
fi

echo "==> Rust formatting"
cargo fmt --check
echo "==> Clippy"
cargo clippy --all-targets --all-features --locked -- -D warnings
echo "==> Complete Rust test suite"
cargo test --all-targets --all-features --locked
echo "==> Optional frontend and browser project checks"
./scripts/ci-project-checks.sh
echo "==> Real Playwright evidence fixture"
./scripts/test-playwright-fixture.sh
echo "==> Deterministic orchestration fixture"
cargo test --locked orchestrator::tests::deterministic_subprocess_repair_lifecycle -- --exact
echo "==> Installer suite"
./scripts/test-installer.sh
echo "==> Local release binary"
cargo build --release --locked

package_version=$(cargo metadata --no-deps --format-version 1 |
  sed -n 's/.*"name":"bender","version":"\([^"]*\)".*/\1/p')
[ -n "$package_version" ] || {
  echo "Could not read Bender package version." >&2
  exit 1
}
target/release/bender version | grep -F "bender ${package_version}" >/dev/null

validation_tmp=$(mktemp -d)
runtime_pid=
cleanup() {
  if [ -n "$runtime_pid" ]; then
    kill "$runtime_pid" 2>/dev/null || true
    wait "$runtime_pid" 2>/dev/null || true
  fi
  rm -rf "$validation_tmp"
}
trap cleanup EXIT HUP INT TERM

dist="$validation_tmp/dist"
mkdir -p "$dist"
for asset in \
  bender-linux-x86_64 \
  bender-linux-aarch64 \
  bender-macos-x86_64 \
  bender-macos-aarch64 \
  bender-windows-x86_64.exe
do
  cp target/release/bender "$dist/$asset"
done
./scripts/verify-release-assets.sh "$dist" --generate

install_prefix="$validation_tmp/install"
BENDER_DOWNLOAD_BASE_URL="file://$dist" \
  ./install.sh --version "v${package_version}" --prefix "$install_prefix" --non-interactive
"$install_prefix/bin/bender" version | grep -F "bender ${package_version}" >/dev/null

smoke_parent="$validation_tmp/smoke-parent"
smoke_project="$smoke_parent/project"
mkdir -p "$smoke_project" "$smoke_parent/sibling"
printf parent-sentinel > "$smoke_parent/parent-sentinel"
printf sibling-sentinel > "$smoke_parent/sibling/sentinel"
(
  cd "$smoke_project"
  git init -q
  PATH="$root/target/release:$PATH" bender init >/dev/null
  test -d .bender
  test ! -e "$root/parent-sentinel"
)

port=$(node -e "const s=require('net').createServer();s.listen(0,'127.0.0.1',()=>{console.log(s.address().port);s.close()})")
(
  cd "$smoke_project"
  PATH="$root/target/release:$PATH" bender run --bind "127.0.0.1:$port"
) >"$validation_tmp/smoke-server.log" 2>&1 &
runtime_pid=$!
ready=false
attempt=0
while [ "$attempt" -lt 50 ]; do
  if curl -fsS "http://127.0.0.1:$port/api/auth/status" >/dev/null 2>&1; then
    ready=true
    break
  fi
  if ! kill -0 "$runtime_pid" 2>/dev/null; then
    break
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done
[ "$ready" = true ] || {
  echo "Temporary-project web smoke test failed." >&2
  sed -n '1,120p' "$validation_tmp/smoke-server.log" >&2
  exit 1
}
kill "$runtime_pid" 2>/dev/null || true
wait "$runtime_pid" 2>/dev/null || true
runtime_pid=

cargo test --locked workspace::tests::rejects_direct_and_nested_symlink_escapes -- --exact
cargo test --locked jobs::tests::persists_every_required_job_file_and_recovers_interruption -- --exact
cargo test --locked runtime::tests::runtime_process_and_children_are_cleaned_up -- --exact

echo "Release validation passed for bender ${package_version}."
