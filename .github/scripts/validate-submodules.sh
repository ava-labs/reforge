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

while IFS= read -r url; do
  ok=false
  for a in "${allowed[@]}"; do
    [[ "$url" == "$a" ]] && ok=true && break
  done
  if [[ "$ok" == false ]]; then
    echo "Disallowed submodule URL: $url" >&2
    exit 1
  fi
done < <(git config --file .gitmodules --get-all submodule.*.url)

git submodule update --init --recursive