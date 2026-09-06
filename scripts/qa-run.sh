#!/usr/bin/env bash
# Build this checkout, then run with isolated persistent development state.
set -euo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$root/target}
# Cargo resolves relative target directories from the checkout, not runtime.
if [[ $CARGO_TARGET_DIR != /* ]]; then export CARGO_TARGET_DIR="$root/$CARGO_TARGET_DIR"; fi
runtime=${BOOKFORGE_QA_RUNTIME:-$root/.qa/runtime}
mkdir -p "$runtime"
(cd "$root" && cargo build -p bookforge-cli --locked)
cd "$runtime"
exec "$CARGO_TARGET_DIR/debug/bookforge" "$@"
