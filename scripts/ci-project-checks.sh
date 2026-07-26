#!/bin/sh
set -eu

if [ -f package.json ]; then
  npm ci
  for script in format:check lint test test:browser; do
    if npm run | grep -Eq "^[[:space:]]+${script}([[:space:]]|$)"; then
      npm run "$script"
    fi
  done
fi
