#!/usr/bin/env bash
# Copyright (C) 2026, Ava Labs, Inc.
# See the file LICENSE for licensing terms.
#
# Validates that all .gitmodules URLs are in the allow-list, then initialises
# submodules. Exits non-zero if any disallowed URL is found.

set -euo pipefail

allowed=(
  "https://github.com/foundry-rs/forge-std"
  "https://github.com/foundry-rs/foundry"
)

# --get-regexp, not --get-all: --get-all matches a literal key name, so the
# pattern silently returned nothing and every URL went unchecked.
urls=$(git config --file .gitmodules --get-regexp '^submodule\..*\.url$' | cut -d' ' -f2-)

# .gitmodules always declares at least one submodule, so an empty list means
# the extraction broke rather than that there is nothing to check.
if [[ -z "$urls" ]]; then
  echo "error: no submodule URLs found in .gitmodules" >&2
  exit 1
fi

while IFS= read -r url; do
  ok=false
  for a in "${allowed[@]}"; do
    [[ "$url" == "$a" ]] && ok=true && break
  done
  if [[ "$ok" == false ]]; then
    echo "Disallowed submodule URL: $url" >&2
    exit 1
  fi
done <<< "$urls"

git submodule update --init --recursive
