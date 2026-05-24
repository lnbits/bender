#!/usr/bin/env sh
set -eu

# Consume stdin so callers can always send JSON input, even though this demo
# tool does not need any fields yet.
while IFS= read -r _line; do
  :
done

printf '%s\n' '{"ok":true,"message":"Hello from a drop-in Bender tool."}'

