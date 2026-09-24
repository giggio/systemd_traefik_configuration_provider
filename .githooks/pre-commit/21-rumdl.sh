#!/usr/bin/env bash

set -euo pipefail

if ! command -v rumdl > /dev/null; then
  echo "rumdl not found, it is in the dev shell (nix develop)."
  exit 1
fi
rumdl check
