#!/usr/bin/env bash
# Shared local/CI checks. Every run keeps its evidence, including failures.
set -euo pipefail
ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
mode=${1:-quick}
case "$mode" in quick|full|fmt|clippy|test|features|browser) ;; *) echo "Usage: $0 [quick|full|fmt|clippy|test|features|browser]" >&2; exit 2;; esac
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
mkdir -p "$ROOT/.qa/runs"
run=$(mktemp -d "$ROOT/.qa/runs/$(date -u +%Y%m%dT%H%M%SZ)-XXXXXX")
export BOOKFORGE_QA_ARTIFACTS="$run"
exec > >(tee "$run/verify.log") 2>&1
trap 'status=$?; echo "exit=$status" > "$run/result.txt"; echo "Evidence: $run"' EXIT
printf 'mode=%s\n' "$mode"
git rev-parse HEAD
# Includes tracked edits and hashes of untracked source files, without copying contents.
git diff HEAD --binary | git hash-object --stdin > "$run/patch-hash.txt"
git status --short > "$run/status.txt"
git ls-files --others --exclude-standard -z | while IFS= read -r -d '' file; do
  printf '%s  %s\n' "$(git hash-object -- "$file")" "$file"
done > "$run/untracked-hashes.txt"
rustc -vV
cargo --version
node --version
npm --version
step() { printf '\nRunning:'; printf ' %q' "$@"; printf '\n'; "$@"; }
fmt() { step bash -n scripts/verify.sh scripts/worktree.sh scripts/qa-run.sh scripts/qa-doctor.sh; step cargo fmt --all --check; step git diff --check; step bash scripts/validate-release-security.sh; }
clippy() { step cargo clippy --workspace --all-targets --all-features --locked -- -D warnings; }
tests() { step node --test crates/bookforge-cli/tests/dashboard/runtime.test.cjs; step cargo test --workspace --locked; step cargo test --workspace --examples --locked; }
features() { step cargo check -p bookforge-cli --no-default-features --locked; }
browser() {
  step cargo build -p bookforge-cli --locked
  # Resolve the binary from Cargo's target directory, never from PATH.
  export BOOKFORGE_BIN="$(cd "$CARGO_TARGET_DIR/debug" && pwd)/bookforge"
  step npm --prefix qa/browser test
}
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings"
case "$mode" in
  fmt) fmt;; clippy) clippy;; test) tests;; features) features;; browser) browser;;
  quick) fmt; step node --test crates/bookforge-cli/tests/dashboard/runtime.test.cjs; features;;
  full) fmt; clippy; tests; features; browser;;
esac
