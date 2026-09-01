#!/usr/bin/env bash
set -euo pipefail

# Lightweight, dependency-free guard against regenerating the release workflow
# with the old broad token scope or an unverified cargo-dist bootstrap.

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="${ROOT_DIR}/.github/workflows/release.yml"
CONFIG="${ROOT_DIR}/dist-workspace.toml"
GATES="${ROOT_DIR}/scripts/verify-release-gates.sh"
POLICY="${ROOT_DIR}/SECURITY.md"

fail() {
  echo "release security validation: $*" >&2
  exit 1
}

[[ -f "$WORKFLOW" ]] || fail "missing ${WORKFLOW}"
[[ -f "$CONFIG" ]] || fail "missing ${CONFIG}"
[[ -x "$GATES" ]] || fail "${GATES} must be executable"
[[ -f "$POLICY" ]] || fail "missing ${POLICY}"
for heading in 'Supported Versions' 'Reporting a Vulnerability' 'Response Expectations' 'Scope'; do
  grep -q -E "^## ${heading}$" "$POLICY" \
    || fail "SECURITY.md is missing the '${heading}' section"
done

for script in "$GATES" "$ROOT_DIR/scripts/validate-release-security.sh" "$ROOT_DIR/scripts/test-verify-release-gates.sh"; do
  if grep -n -E '(^|[[:space:]])r[g]([[:space:]]|$)' "$script"; then
    fail "release gate scripts must not depend on ripgrep: ${script}"
  fi
done

sed -n '1,46p' "$WORKFLOW" | grep -q -E '^  "contents": "read"$' \
  || fail "workflow-wide contents permission must be read"
if sed -n '1,46p' "$WORKFLOW" | grep -n -E 'contents.*write'; then
  fail "workflow-wide contents permission must not be write"
fi

grep -q -E '^  verify-release-gates:$' "$WORKFLOW" \
  || fail "missing exact-commit release gate job"
plan_block="$(awk '
  /^  plan:$/ { in_plan=1; next }
  in_plan && /^  [^ ]/ { exit }
  in_plan { print }
' "$WORKFLOW")"
printf '%s\n' "$plan_block" | grep -q -E '^      - verify-release-gates$' \
  || fail "release plan must depend on the release gate"
grep -q -E 'cargo install cargo-dist --version 0\.32\.0 --locked' "$WORKFLOW" \
  || fail "cargo-dist must be installed from the locked crate release"
if grep -n -E 'cargo-dist-installer\.sh|matrix\.install_dist\.run' "$WORKFLOW"; then
  fail "unverified/generated cargo-dist installer path remains"
fi

grep -q -E '^allow-dirty = \["ci"\]$' "$CONFIG" \
  || fail "manual workflow hardening must be recorded in dist-workspace.toml"

for pin in \
  '3d3c42e5aac5ba805825da76410c181273ba90b1' \
  '043fb46d1a93c77aae656e7c1c64a875d1fc6a0a' \
  '3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c' \
  'f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6'; do
  grep -F -q "$pin" "$CONFIG" \
    || fail "action pin missing from dist-workspace.toml: ${pin}"
  grep -F -q "$pin" "$WORKFLOW" \
    || fail "action pin missing from release.yml: ${pin}"
done

for check_name in \
  'fmt' \
  'clippy' \
  'test' \
  'test (windows-msvc)' \
  'msrv (1.88.0)' \
  'corpus (small)' \
  'RustSec advisory audit' \
  'CodeQL (Rust)' \
  'CodeQL'; do
  grep -F -q "$check_name" "$GATES" \
    || fail "required check missing from gate script: ${check_name}"
done

echo "release security validation: OK"
