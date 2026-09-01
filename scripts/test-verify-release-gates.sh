#!/usr/bin/env bash
set -euo pipefail

# Synthetic regression tests for scripts/verify-release-gates.sh. A fake `gh`
# in PATH serves canned check-run pages and compare responses, so the latest
# selection, pagination flattening, and fail-closed verdicts are exercised
# without touching the GitHub API.
#
# The fake models the caller-visible contract of the real `gh api` CLI: flags
# may appear in any order, `--paginate` concatenates every page after applying
# the `--jq` filter (one result per line), and an explicit `page=` query
# parameter selects a single page. The fixture stores one JSON object per API
# response: `.check_pages` is an array of pages, each `{ total_count,
# check_runs }`, and `.compare` mirrors the compare endpoint's
# `{ merge_base_commit: { sha } }` shape.
#
# Version/CHANGELOG cases run against a throwaway workspace fixture (a copy of
# the gate script plus a minimal Cargo crate and CHANGELOG), so tracked
# Cargo.toml and CHANGELOG.md files are never modified.

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
GATES="${ROOT_DIR}/scripts/verify-release-gates.sh"

# A release tag must correspond to a CHANGELOG release heading and a workspace
# crate version, so pass-cases use the currently released version.
TAG_SHA="1111111111111111111111111111111111111111"
OTHER_SHA="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
TAG_VERSION="$(grep -m1 -o -E '^## v[0-9][^[:space:]]*' "${ROOT_DIR}/CHANGELOG.md" | sed 's/^## v//')"
GITHUB_REF_NAME="v${TAG_VERSION}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cat >"$tmp/gh" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
: "${FAKE_GH_DATA:?}"

cmd="${1:-}"
shift

url=""
jq_filter="."
paginate=0
while (($#)); do
  case "$1" in
    --paginate) paginate=1 ;;
    --jq) jq_filter="${2:?}"; shift ;;
    -H | --hostname) shift ;;
    *) url="$1" ;;
  esac
  shift
done

case "$cmd" in
  api)
    if [[ -n "${FAKE_GH_FAIL_URL:-}" && "$url" == *"$FAKE_GH_FAIL_URL"* ]]; then
      echo "gh: simulated API failure for ${url}" >&2
      exit 1
    fi
    if [[ "$url" == *"/compare/"* ]]; then
      jq -r ".compare | ${jq_filter}" "$FAKE_GH_DATA"
    elif [[ "$url" == *"/check-runs"* ]]; then
      # Only an explicit `page=N` query parameter selects a page; `per_page=`
      # must not match. Real `gh api --paginate` fetches every Link-header
      # page, so the fixture's pages are flattened exactly like the CLI's
      # concatenated output.
      page=1
      if [[ "$url" =~ (^|[?&])page=([0-9]+) ]]; then
        page="${BASH_REMATCH[2]}"
      fi
      if ((paginate)); then
        jq -r ".check_pages[] | ${jq_filter}" "$FAKE_GH_DATA"
      else
        jq -r --argjson p "$page" ".check_pages[\$p - 1] | ${jq_filter}" "$FAKE_GH_DATA"
      fi
    else
      echo "unexpected gh api URL: $url" >&2
      exit 1
    fi
    ;;
  *)
    echo "unexpected gh invocation: $*" >&2
    exit 1
    ;;
esac
MOCK
chmod +x "$tmp/gh"

mkrun() {
  local id="$1" name="$2" status="$3" conclusion="$4" started="$5" completed="$6" sha="$7"
  jq -n \
    --arg id "$id" --arg name "$name" --arg status "$status" \
    --arg conclusion "$conclusion" --arg started "$started" --arg completed "$completed" --arg sha "$sha" \
    '{ id: ($id | tonumber),
       name: $name,
       status: $status,
       # An empty sentinel maps to JSON null so pending runs can carry
       # conclusion/completed_at of null like the real API.
       conclusion: (if $conclusion == "" then null else $conclusion end),
       started_at: (if $started == "" then null else $started end),
       completed_at: (if $completed == "" then null else $completed end),
       head_sha: $sha,
       html_url: ("https://example.com/runs/" + $id) }'
}

SUCCESS='success'
FAILURE='failure'

# All required checks passing at the given timestamp; the result is a JSON array.
all_pass() {
  local started="$1" completed="$2" sha="$3" id=100
  local names=( \
    'fmt' 'clippy' 'test' 'test (windows-msvc)' 'msrv (1.88.0)' \
    'corpus (small)' 'RustSec advisory audit' 'CodeQL (Rust)' 'CodeQL' )
  local out="[]" name
  for name in "${names[@]}"; do
    id=$((id + 1))
    out="$(jq -c --argjson run "$(mkrun "$id" "$name" completed "$SUCCESS" "$started" "$completed" "$sha")" \
      '. + [ $run ]' <<<"$out")"
  done
  printf '%s' "$out"
}

write_data() {
  # $1: fixture JSON for FAKE_GH_DATA
  printf '%s\n' "$1" >"$tmp/data.json"
}

run_gate() {
  # Optional args: 1) gate script path, 2) GITHUB_REF_NAME, 3) simulated gh
  # API failure URL substring. Empty args fall back to the defaults.
  local gate="${1:-$GATES}"
  local ref_name="${2:-$GITHUB_REF_NAME}"
  local fail_url="${3:-}"
  PATH="$tmp:$PATH" \
    GH_TOKEN=fake \
    GITHUB_REPOSITORY="acme/bookforge" \
    GITHUB_SHA="$TAG_SHA" \
    GITHUB_REF_NAME="$ref_name" \
    FAKE_GH_DATA="$tmp/data.json" \
    FAKE_GH_FAIL_URL="$fail_url" \
    bash "$gate" >"$tmp/out" 2>&1
  echo "$?"
}

run_case() {
  local name="$1" expected="$2"
  shift 2
  local actual
  actual="$(run_gate "$@")"
  if [[ "$actual" == "$expected" ]]; then
    echo "PASS: ${name}"
  else
    echo "FAIL: ${name}: expected exit ${expected}, got ${actual}" >&2
    cat "$tmp/out" >&2
    exit 1
  fi
}

# A throwaway workspace fixture for version/CHANGELOG mutations. It is rebuilt
# before each case so no case depends on another's mutation, and nothing under
# the tracked tree is ever edited.
FIXTURE="${tmp}/fixture"
FIXTURE_GATE="${FIXTURE}/scripts/verify-release-gates.sh"
FIXTURE_TAG="v1.2.3"
make_fixture() {
  rm -rf "$FIXTURE"
  mkdir -p "${FIXTURE}/crates/demo/src" "${FIXTURE}/crates/demo-extra/src" "${FIXTURE}/scripts"
  cp "$GATES" "$FIXTURE_GATE"
  cat >"${FIXTURE}/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = ["crates/demo", "crates/demo-extra"]
EOF
  cat >"${FIXTURE}/crates/demo/Cargo.toml" <<'EOF'
[package]
name = "demo"
version = "1.2.3"
edition = "2021"
EOF
  cat >"${FIXTURE}/crates/demo-extra/Cargo.toml" <<'EOF'
[package]
name = "demo-extra"
version = "1.2.3"
edition = "2021"
EOF
  : >"${FIXTURE}/crates/demo/src/lib.rs"
  : >"${FIXTURE}/crates/demo-extra/src/lib.rs"
  printf '## v1.2.3\n' >"${FIXTURE}/CHANGELOG.md"
}

make_empty_fixture() {
  rm -rf "$FIXTURE"
  mkdir -p "${FIXTURE}/scripts"
  cp "$GATES" "$FIXTURE_GATE"
  # A virtual workspace with no members is valid: cargo metadata emits
  # `"packages": []` and exits 0, so the gate itself must fail closed.
  cat >"${FIXTURE}/Cargo.toml" <<'EOF'
[workspace]
resolver = "2"
members = []
EOF
  printf '## v1.2.3\n' >"${FIXTURE}/CHANGELOG.md"
}

old="2026-08-02T00:00:00Z"
new="2026-08-31T00:00:00Z"
now="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

# Base fixture used by most cases: every required check passes on the tag SHA
# and the tag SHA is the merge base of the release branch compare.
base_json="$(jq -n \
  --argjson runs "$(all_pass "$new" "$new" "$TAG_SHA")" \
  --arg merge "$TAG_SHA" \
  '{ check_pages: [ { total_count: ($runs | length), check_runs: $runs } ],
     compare: { status: "ahead", ahead_by: 0, behind_by: 0,
                merge_base_commit: { sha: $merge } } }')"

# Case 1: duplicate old-success / new-failure -> the newer failure must win.
fmt_old_success_new_failure="$(mkrun 1 'fmt' completed "$SUCCESS" "$old" "$old" "$TAG_SHA")"
fmt_new_failure="$(mkrun 2 'fmt' completed "$FAILURE" "$new" "$new" "$TAG_SHA")"
data="$(jq -c --argjson a "$fmt_old_success_new_failure" --argjson b "$fmt_new_failure" \
  '.check_pages[0] |= (
     .check_runs = ([ $a, $b ] + .check_runs | map(select(.name != "fmt")) + ([ $a, $b ])) |
     .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "duplicate old-success/new-failure fails (newer failure wins)" 1

# Case 2: duplicate old-failure / new-success -> the newer success must win.
fmt_old_failure="$(mkrun 3 'fmt' completed "$FAILURE" "$old" "$old" "$TAG_SHA")"
fmt_new_success="$(mkrun 4 'fmt' completed "$SUCCESS" "$new" "$new" "$TAG_SHA")"
data="$(jq -c --argjson a "$fmt_old_failure" --argjson b "$fmt_new_success" \
  '.check_pages[0] |= (
     .check_runs = ([ $a, $b ] + .check_runs | map(select(.name != "fmt")) + ([ $a, $b ])) |
     .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "duplicate old-failure/new-success passes (newer success wins)" 0

# Case 3: two runs share the exact latest timestamp but disagree on the
# conclusion. A re-run always gets a higher numeric check-run id, so the id
# tiebreak picks run 6 deterministically and the gate must not guess.
fmt_ambiguous_a="$(mkrun 5 'fmt' completed "$FAILURE" "$new" "$new" "$TAG_SHA")"
fmt_ambiguous_b="$(mkrun 6 'fmt' completed "$SUCCESS" "$new" "$new" "$TAG_SHA")"
data="$(jq -c --argjson a "$fmt_ambiguous_a" --argjson b "$fmt_ambiguous_b" \
  '.check_pages[0] |= (
     .check_runs = ([ $a, $b ] + .check_runs | map(select(.name != "fmt")) + ([ $a, $b ])) |
     .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "same-timestamp runs resolve deterministically by numeric id (higher id wins)" 0

# Case 4: truly indistinguishable runs - the same timestamp *and* the same
# numeric id, so the ordering cannot tell them apart and the gate fails closed.
fmt_ambiguous_same_id_a="$(mkrun 5 'fmt' completed "$SUCCESS" "$new" "$new" "$TAG_SHA")"
fmt_ambiguous_same_id_b="$(mkrun 5 'fmt' completed "$FAILURE" "$new" "$new" "$TAG_SHA")"
data="$(jq -c --argjson a "$fmt_ambiguous_same_id_a" --argjson b "$fmt_ambiguous_same_id_b" \
  '.check_pages[0] |= (
     .check_runs = ([ $a, $b ] + .check_runs | map(select(.name != "fmt")) + ([ $a, $b ])) |
     .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "truly indistinguishable runs (same timestamp and id) fail closed" 1

# Case 5: pagination - an old success lives on page 1 and the newer failure on
# page 2. The gate must flatten both pages and let the newer (page 2) run win.
page1="$(all_pass "$old" "$old" "$TAG_SHA")"
page1="$(jq -c 'map(select(.name != "test"))' <<<"$page1")"
test_new_failure="$(mkrun 7 'test' completed "$FAILURE" "$new" "$new" "$TAG_SHA")"
data="$(jq -n --argjson p1 "$page1" --argjson p2 "[ $test_new_failure ]" --arg merge "$TAG_SHA" \
  '{ check_pages: [
       { total_count: ($p1 | length), check_runs: $p1 },
       { total_count: ($p2 | length), check_runs: $p2 }
     ],
     compare: { status: "ahead", ahead_by: 0, behind_by: 0,
                merge_base_commit: { sha: $merge } } }')"
write_data "$data"
run_case "pagination flattens pages and newer failure on page 2 wins" 1

# Case 6: wrong SHA - every run belongs to a different commit, so the exact
# tagged SHA has no results at all.
data="$(jq -c \
  --arg OTHER_SHA "$OTHER_SHA" \
  '.check_pages[0] |= (.check_runs |= map(.head_sha = $OTHER_SHA))' \
  <<<"$base_json")"
write_data "$data"
run_case "runs for a different SHA are ignored (missing)" 1

# Case 7: missing - no run exists for the first required check.
data="$(jq -c \
  '.check_pages[0] |= (.check_runs |= map(select(.name != "fmt")) | .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "missing required check fails" 1

# Case 8: pending latest - the newest attempt for the tag SHA is still running
# even though an older attempt succeeded; the gate must not fall back to the
# older success.
fmt_old_success="$(mkrun 8 'fmt' completed "$SUCCESS" "$old" "$old" "$TAG_SHA")"
fmt_pending="$(mkrun 9 'fmt' in_progress "" "$now" "" "$TAG_SHA")"
data="$(jq -c --argjson a "$fmt_old_success" --argjson b "$fmt_pending" \
  '.check_pages[0] |= (
     .check_runs = ([ $a, $b ] + .check_runs | map(select(.name != "fmt")) + ([ $a, $b ])) |
     .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "pending latest attempt fails instead of falling back to old success" 1

# Case 9: CodeQL security result fails while the CodeQL (Rust) workflow job
# passes - the gate must require the security result, not just the job.
data="$(jq -c \
  --argjson run "$(mkrun 10 'CodeQL' completed "$FAILURE" "$new" "$new" "$TAG_SHA")" \
  '.check_pages[0] |= (.check_runs |= (map(select(.name != "CodeQL")) + [ $run ]))' \
  <<<"$base_json")"
write_data "$data"
run_case "CodeQL security result failing fails the gate despite CodeQL (Rust) success" 1

# Case 10: the CodeQL (Rust) workflow job is missing entirely while the
# security result is present - the gate must still fail closed.
data="$(jq -c \
  '.check_pages[0] |= (.check_runs |= map(select(.name != "CodeQL (Rust)")) | .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "missing CodeQL (Rust) job fails the gate independently" 1

# Case 11: the CodeQL security result is missing entirely while the workflow
# job is present - the gate must still fail closed.
data="$(jq -c \
  '.check_pages[0] |= (.check_runs |= map(select(.name != "CodeQL")) | .total_count = (.check_runs | length))' \
  <<<"$base_json")"
write_data "$data"
run_case "missing CodeQL security result fails the gate independently" 1

# Case 12: release-branch reachability - all checks pass but the tag is not an
# ancestor of the configured release branch, so the compare merge base is an
# older commit rather than the tag SHA.
data="$(jq -c --arg merge "$OTHER_SHA" \
  '.compare.merge_base_commit.sha = $merge' \
  <<<"$base_json")"
write_data "$data"
run_case "tag not reachable from the release branch fails" 1

# Case 13: the check-runs API itself fails (auth, rate limit, permissions) -
# the gate must fail closed instead of releasing with unknown state.
run_case "check-runs API failure fails closed" 1 "" "" "check-runs"

# Case 14: the compare API itself fails - the gate must fail closed rather than
# assume the tag is reachable.
run_case "compare API failure fails closed" 1 "" "" "/compare/"

# Case 15: explicit happy path - every required check, the tag version, the
# workspace crate versions, the CHANGELOG heading, and reachability all line up.
write_data "$base_json"
run_case "happy path: all checks green, version, changelog, and reachability pass" 0

# Version/CHANGELOG cases run against the throwaway workspace fixture.

make_fixture
write_data "$base_json"
run_case "fixture happy path baseline passes" 0 "$FIXTURE_GATE" "$FIXTURE_TAG"

# Mixed versions: only `demo` drifts to 1.2.4 while `demo-extra` stays at
# 1.2.3, so every package is required to match the tag ("all", not "any").
make_fixture
sed -i 's/version = "1.2.3"/version = "1.2.4"/' "${FIXTURE}/crates/demo/Cargo.toml"
write_data "$base_json"
run_case "tag version mismatching one of two workspace crates fails" 1 "$FIXTURE_GATE" "$FIXTURE_TAG"

# Empty workspace: cargo metadata accepts `members = []` and reports zero
# packages, so the gate must fail closed rather than accept a tag for a release
# that announces nothing.
make_empty_fixture
write_data "$base_json"
run_case "empty workspace with no packages fails closed" 1 "$FIXTURE_GATE" "$FIXTURE_TAG"

make_fixture
printf '# Changelog\n' >"${FIXTURE}/CHANGELOG.md"
write_data "$base_json"
run_case "tag version lacks a first released CHANGELOG heading fails" 1 "$FIXTURE_GATE" "$FIXTURE_TAG"

make_fixture
printf '## v2.0.0\n' >"${FIXTURE}/CHANGELOG.md"
write_data "$base_json"
run_case "tag version mismatches the first released CHANGELOG heading fails" 1 "$FIXTURE_GATE" "$FIXTURE_TAG"

echo "All synthetic gate regression tests passed."
