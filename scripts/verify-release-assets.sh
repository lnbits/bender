#!/bin/sh
set -eu

directory=${1:-dist}
mode=${2:-}
assets="
bender-linux-x86_64
bender-linux-aarch64
bender-macos-x86_64
bender-macos-aarch64
bender-windows-x86_64.exe
"

for asset in $assets; do
  [ -f "$directory/$asset" ] || {
    echo "Missing required release asset: $asset" >&2
    exit 1
  }
done

if [ "$mode" = "--generate" ]; then
  : > "$directory/SHA256SUMS"
  for asset in $assets; do
    (cd "$directory" && sha256sum "$asset") >> "$directory/SHA256SUMS"
  done
fi

[ -f "$directory/SHA256SUMS" ] || {
  echo "Missing required release asset: SHA256SUMS" >&2
  exit 1
}

for asset in $assets; do
  awk -v asset="$asset" '$2 == asset || $2 == "*" asset { found = 1 } END { exit !found }' \
    "$directory/SHA256SUMS" || {
      echo "SHA256SUMS is missing $asset" >&2
      exit 1
    }
done

(cd "$directory" && sha256sum -c SHA256SUMS)
