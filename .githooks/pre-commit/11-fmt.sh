#!/usr/bin/env bash

set -euo pipefail

changed_files=$(cargo fmt --message-format short)
if [[ -n "$changed_files" ]]; then
    echo "The following files are not properly formatted:"
    echo "$changed_files"
    printf '%s\n' "$changed_files" | tr '\n' '\0' | xargs -0 git add -u --
    if git diff --cached --quiet; then
      echo "No changes to commit after cargo fmt."
      exit 1
    fi
fi
