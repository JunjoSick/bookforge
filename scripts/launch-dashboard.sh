#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd)"
cd "${REPO_ROOT}"

BIND="${BOOKFORGE_SERVE_BIND:-127.0.0.1:8765}"

try_exec_bookforge() {
  local bin="$1"
  if [[ -x "${bin}" ]] && "${bin}" help serve >/dev/null 2>&1; then
    exec "${bin}" serve --bind "${BIND}" --open
  fi
}

if [[ -n "${BOOKFORGE_BIN:-}" ]]; then
  if "${BOOKFORGE_BIN}" help serve >/dev/null 2>&1; then
    exec "${BOOKFORGE_BIN}" serve --bind "${BIND}" --open
  fi
  echo "BOOKFORGE_BIN does not point to a BookForge binary with the serve command: ${BOOKFORGE_BIN}" >&2
  exit 1
fi

try_exec_bookforge "${REPO_ROOT}/target/release/bookforge"
try_exec_bookforge "${REPO_ROOT}/target/debug/bookforge"

if command -v bookforge >/dev/null 2>&1; then
  try_exec_bookforge "$(command -v bookforge)"
fi

if command -v cargo >/dev/null 2>&1; then
  exec cargo run -p bookforge-cli -- serve --bind "${BIND}" --open
fi

cat >&2 <<'EOF'
Could not start BookForge dashboard.

No BookForge binary was found on PATH and Cargo is not installed.
Install BookForge, build it from this checkout, or set BOOKFORGE_BIN to the bookforge executable.
EOF
exit 1
