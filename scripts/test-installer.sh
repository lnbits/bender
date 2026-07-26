#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cargo test --locked --test installer
if command -v shellcheck >/dev/null 2>&1; then
  shellcheck "$root/install.sh" "$root/scripts/test-installer.sh"
else
  echo "shellcheck not installed; syntax and behavior are covered by installer tests." >&2
fi
