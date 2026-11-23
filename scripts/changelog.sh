#!/usr/bin/env bash
# Preview release notes offline.
set -euo pipefail

if ! command -v git-cliff >/dev/null 2>&1; then
  echo "error: git-cliff not found — install with: brew install git-cliff" >&2
  exit 1
fi

cd "$(git rev-parse --show-toplevel)"

if [ $# -gt 0 ]; then
  git cliff --offline --tag "$1" --unreleased --strip header
else
  git cliff --offline --unreleased --strip header
fi
