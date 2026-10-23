#!/usr/bin/env bash
set -euo pipefail

# A release tag is allowed to publish only after the exact tagged commit has
# passed every required CI/security job and the tag itself is consistent with
# the workspace, the CHANGELOG, and the configured release branch.
#
# GitHub check runs can contain older attempts for the same SHA, so after
# flattening every page of results and keeping only the exact head SHA, the
# latest attempt per required name is selected deterministically: newest
# `completed_at` (falling back to `started_at` when the run is not finished,
# then an empty string), and a re-run always carries a higher numeric check-run
# id, so the id breaks an exact timestamp tie. Only two runs that share the
# same timestamp *and* the same id are truly indistinguishable, and those fail
# closed rather than guessing. The "CodeQL (Rust)" workflow job can complete
# successfully while its results raise alerts, so code scanning's own record
# is checked as well (see below).

: "${GH_TOKEN:?GH_TOKEN is required to query commit checks}"
: "${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}"
: "${GITHUB_SHA:?GITHUB_SHA is required}"
: "${GITHUB_REF_NAME:?GITHUB_REF_NAME is required}"

RELEASE_BRANCH="${RELEASE_BRANCH:-main}"

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"

required_checks=(
  "fmt"
  "clippy"
  "test"
  "test (windows-msvc)"
  "clippy (macos)"
  "msrv (1.88.0)"
  "corpus (small)"
  "RustSec advisory audit"
  "CodeQL (Rust)"
)

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

echo "Verifying release ${GITHUB_REF_NAME} for ${GITHUB_REPOSITORY}@${GITHUB_SHA} (release branch: ${RELEASE_BRANCH})"

# `gh api --paginate` fetches every Link-header page and applies the `--jq`
# filter to each response, emitting one JSON object per check run per page (the
# whole stream is a single gh invocation). `jq -s` collects the stream into one
# array so later selection can see runs spread across pages. Failing the API
# call (auth, rate limit, or missing permissions) must stop the release.
check_runs="$(gh api \
  --paginate \
  -H 'Accept: application/vnd.github+json' \
  --jq '.check_runs[]' \
  "repos/${GITHUB_REPOSITORY}/commits/${GITHUB_SHA}/check-runs?per_page=100" \
  | jq -s -c '.')" \
  || fail "could not fetch check runs for ${GITHUB_SHA}; check the token and workflow permissions"

for check_name in "${required_checks[@]}"; do
  selection="$(jq -c --arg name "$check_name" --arg sha "$GITHUB_SHA" '
    def candidates: [ .[] | select(.name == $name and .head_sha == $sha) ];
    def ts: .completed_at // .started_at // "";
    def sortkey: [ ts, ((.id // 0) | tonumber) ];
    candidates as $c
    | if ($c | length) == 0 then
        { verdict: "missing", run: null }
      else
        ([ $c[] | sortkey ] | max) as $best
        | ([ $c[] | select(sortkey == $best) ]) as $top
        | if ($top | length) > 1 then
            { verdict: "ambiguous",
              run: { ambiguous: ($top | map({ id, name, status, conclusion, started_at, completed_at, html_url })) } }
          else
            { verdict: "found",
              run: ($top[0] | { id, name, status, conclusion, started_at, completed_at, html_url }) }
          end
      end
  ' <<<"$check_runs")"

  verdict="$(jq -r '.verdict' <<<"$selection")"
  case "$verdict" in
    missing)
      fail "no check run named '$check_name' exists for ${GITHUB_SHA}"
      ;;
    ambiguous)
      echo "FAIL: '$check_name' has multiple latest check runs with the same timestamp for ${GITHUB_SHA}; refusing to guess" >&2
      jq -r '.run.ambiguous[] |
        "  - run \(.id): status=\(.status) conclusion=\(.conclusion // "(none)") started=\(.started_at // "-") completed=\(.completed_at // "-") \(.html_url // "")"' \
        <<<"$selection" >&2
      exit 1
      ;;
    found)
      status="$(jq -r '.run.status // "(unknown)"' <<<"$selection")"
      conclusion="$(jq -r '.run.conclusion // "(not completed)"' <<<"$selection")"
      started_at="$(jq -r '.run.started_at // "-"' <<<"$selection")"
      completed_at="$(jq -r '.run.completed_at // "(pending)"' <<<"$selection")"
      echo "- ${check_name}: ${status} ${conclusion} (started ${started_at}, completed ${completed_at})"
      [[ "$status" == "completed" ]] \
        || fail "required check '$check_name' is still ${status} for ${GITHUB_SHA}; retry once the checks finish"
      [[ "$conclusion" == "success" ]] \
        || fail "required check '$check_name' did not succeed (conclusion=${conclusion})"
      ;;
    *)
      fail "internal error: unexpected verdict '${verdict}' for '${check_name}'"
      ;;
  esac
done

# Code scanning. GitHub reports the CodeQL results as a separate "CodeQL" check
# run only on pull-request commits; a push commit, such as the merge on the
# release branch, never has one, so requiring that run failed every release
# tagged there. When the run exists its latest attempt must still succeed. In
# every case code scanning must hold a CodeQL analysis of the exact commit that
# did not error, and the release branch must have no open alert of high or
# critical severity: a false positive has to be dismissed with a reason first.
codeql_result="$(jq -c --arg sha "$GITHUB_SHA" '
  [ .[] | select(.name == "CodeQL" and .head_sha == $sha) ]
  | if length == 0 then empty
    else max_by([ (.completed_at // .started_at // ""), ((.id // 0) | tonumber) ])
    end
' <<<"$check_runs")"
if [[ -n "$codeql_result" ]]; then
  status="$(jq -r '.status // "(unknown)"' <<<"$codeql_result")"
  conclusion="$(jq -r '.conclusion // "(not completed)"' <<<"$codeql_result")"
  echo "- CodeQL code-scanning result: ${status} ${conclusion}"
  [[ "$status" == "completed" && "$conclusion" == "success" ]] \
    || fail "the CodeQL code-scanning result for ${GITHUB_SHA} did not succeed (status=${status}, conclusion=${conclusion})"
fi

analyses="$(gh api \
  --paginate \
  -H 'Accept: application/vnd.github+json' \
  --jq '.[]' \
  "repos/${GITHUB_REPOSITORY}/code-scanning/analyses?sha=${GITHUB_SHA}&tool_name=CodeQL&per_page=100" \
  | jq -s -c '.')" \
  || fail "could not fetch code-scanning analyses for ${GITHUB_SHA}; the gate job needs security-events: read"
analysis_count="$(jq --arg sha "$GITHUB_SHA" '[ .[] | select(.commit_sha == $sha) ] | length' <<<"$analyses")"
[[ "$analysis_count" -gt 0 ]] \
  || fail "code scanning holds no CodeQL analysis of ${GITHUB_SHA}; wait for the CodeQL workflow to upload its results"
errored="$(jq --arg sha "$GITHUB_SHA" '[ .[] | select(.commit_sha == $sha and ((.error // "") != "")) ] | length' <<<"$analyses")"
[[ "$errored" -eq 0 ]] \
  || fail "${errored} CodeQL analysis(es) of ${GITHUB_SHA} reported an error"
echo "- code scanning: ${analysis_count} CodeQL analysis(es) of ${GITHUB_SHA}, none errored"

alerts="$(gh api \
  --paginate \
  -H 'Accept: application/vnd.github+json' \
  --jq '.[]' \
  "repos/${GITHUB_REPOSITORY}/code-scanning/alerts?ref=refs/heads/${RELEASE_BRANCH}&state=open&per_page=100" \
  | jq -s -c '.')" \
  || fail "could not fetch open code-scanning alerts for ${RELEASE_BRANCH}; the gate job needs security-events: read"
blocking="$(jq -c '[ .[] | select((.rule.security_severity_level // "") as $level | $level == "critical" or $level == "high") ]' <<<"$alerts")"
blocking_count="$(jq 'length' <<<"$blocking")"
if [[ "$blocking_count" -gt 0 ]]; then
  echo "FAIL: ${blocking_count} open high or critical code-scanning alert(s) on ${RELEASE_BRANCH}:" >&2
  jq -r '.[] | "  - #\(.number) \(.rule.security_severity_level) \(.rule.id) \(.most_recent_instance.location.path // "?"):\(.most_recent_instance.location.start_line // "?")"' \
    <<<"$blocking" >&2
  fail "fix them, or dismiss false positives with a reason, before releasing"
fi
echo "- code scanning: no open high or critical alerts on ${RELEASE_BRANCH}"

# The tag version must match every workspace crate (cargo-dist announces "all
# dist-able packages with that version") and the CHANGELOG release heading, and
# the workspace must contain at least one package.
version="${GITHUB_REF_NAME}"
version="${version##*/}" # strip an optional "package/" prefix
version="${version#v}"   # strip an optional leading "v"

echo "- tag version: ${version}"

# Validate against the workspace this script ships with, regardless of the
# caller's working directory.
cargo metadata --no-deps --format-version 1 --manifest-path "${ROOT_DIR}/Cargo.toml" \
  | jq -e --arg v "$version" '(.packages | length) > 0 and all(.packages[]; .version == $v)' >/dev/null \
  || fail "tag version ${version} must equal every workspace crate version and the workspace must contain at least one package"

changelog="${CHANGELOG_FILE:-${ROOT_DIR}/CHANGELOG.md}"
top_release="$(grep -m1 -n -E '^## v[0-9]' "$changelog")" \
  || fail "no '## vX.Y.Z' release heading found in ${changelog}"
top_version="$(sed -E 's/^[0-9]+:## v([^[:space:]]+).*/\1/' <<<"$top_release")"
echo "- CHANGELOG top release heading: ${top_version}"
[[ "$top_version" == "$version" ]] \
  || fail "CHANGELOG top release heading is v${top_version}, expected v${version}"

# The tagged commit must be reachable from the release branch. The compare API
# resolves the merge base of the two commits: it equals the tag SHA exactly
# when the tag is an ancestor of the release branch, with no assumptions about
# branch names beyond the configured default.
merge_base="$(gh api \
  -H 'Accept: application/vnd.github+json' \
  --jq '.merge_base_commit.sha' \
  "repos/${GITHUB_REPOSITORY}/compare/${RELEASE_BRANCH}...${GITHUB_SHA}")" \
  || fail "could not compare ${GITHUB_SHA} against ${RELEASE_BRANCH}; is the release branch named correctly?"
[[ "$merge_base" == "$GITHUB_SHA" ]] \
  || fail "tag ${GITHUB_REF_NAME} (${GITHUB_SHA}) is not reachable from ${RELEASE_BRANCH}"
echo "- tag ${GITHUB_REF_NAME} is reachable from ${RELEASE_BRANCH}"

echo "All required checks passed for the exact tagged commit; tag version and reachability validated."
