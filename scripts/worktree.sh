#!/usr/bin/env bash
# Create a clean sibling checkout. Never copy another agent's uncommitted files.
set -euo pipefail
if [[ $# -lt 1 || $# -gt 2 || ! $1 =~ ^[a-zA-Z0-9][a-zA-Z0-9_-]*$ ]]; then
  echo "Usage: $0 NAME [BASE_REF] (default: freshly fetched origin/main)" >&2; exit 2
fi
root=$(git -C "$(dirname "${BASH_SOURCE[0]}")/.." rev-parse --show-toplevel)
name=$1
base=${2:-origin/main}
git -C "$root" fetch origin
sha=$(git -C "$root" rev-parse --verify "$base^{commit}")
dest="$(dirname "$root")/$(basename "$root")-$name"
git -C "$root" worktree add -b "work/$name" "$dest" "$sha"
mkdir -p "$dest/.qa/runtime" "$dest/.qa/cache" "$dest/.qa/output"
printf '%s\n' "$sha" > "$dest/.qa/base-sha"
# %q supports spaces and shell metacharacters in checkout paths.
{
  printf 'export CARGO_TARGET_DIR=%q\n' "$dest/target"
  printf 'export BOOKFORGE_QA_RUNTIME=%q\n' "$dest/.qa/runtime"
  printf 'export BOOKFORGE_QA_CACHE=%q\n' "$dest/.qa/cache"
  printf 'export BOOKFORGE_QA_OUTPUT=%q\n' "$dest/.qa/output"
} > "$dest/.qa/environment.sh"
printf 'Created %s at %s\nActivate: source %q\n' "$dest" "$sha" "$dest/.qa/environment.sh"
