#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
EVENTS_PATH="${BOOKFORGE_BENCH_EVENTS:-/tmp/bookforge-bench-events.jsonl}"
OUTPUT_PATH="${BOOKFORGE_BENCH_OUTPUT:-/tmp/bookforge-bench.epub}"
DEFAULT_INPUT="${ROOT_DIR}/tests/fixtures/tiny.epub"
if [[ ! -f "${DEFAULT_INPUT}" && -f "${ROOT_DIR}/test/test.epub" ]]; then
  DEFAULT_INPUT="${ROOT_DIR}/test/test.epub"
fi
INPUT_PATH="${BOOKFORGE_BENCH_INPUT:-${DEFAULT_INPUT}}"

now_ms() {
  if command -v python3 >/dev/null 2>&1; then
    python3 -c 'import time; print(time.time_ns() // 1_000_000)'
  elif command -v perl >/dev/null 2>&1; then
    perl -MTime::HiRes=time -e 'printf "%.0f\n", time() * 1000'
  else
    # POSIX fallback: second precision, still numeric on BSD/macOS.
    printf '%s000\n' "$(date +%s)"
  fi
}

if [[ ! -f "${INPUT_PATH}" ]]; then
  echo "Missing benchmark input: ${INPUT_PATH}" >&2
  echo "Set BOOKFORGE_BENCH_INPUT to a tiny EPUB fixture." >&2
  exit 1
fi

rm -f "${EVENTS_PATH}" "${OUTPUT_PATH}"

start_ms="$(now_ms)"
cargo run --release -p bookforge-cli -- translate "${INPUT_PATH}" \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --profile v1-fast \
  --ui quiet \
  --progress-jsonl "${EVENTS_PATH}" \
  --out "${OUTPUT_PATH}"
end_ms="$(now_ms)"

elapsed_ms="$((end_ms - start_ms))"
if [[ -f "${EVENTS_PATH}" ]]; then
  requests="$(grep -c 'RequestFinished' "${EVENTS_PATH}" || true)"
  segments="$(grep -c 'SegmentFinished' "${EVENTS_PATH}" || true)"
  events_note="${EVENTS_PATH}"
else
  requests="0"
  segments="0"
  events_note="${EVENTS_PATH} (not created)"
fi

echo "Elapsed ms: ${elapsed_ms}"
echo "Requests: ${requests}"
echo "Segments: ${segments}"
echo "Events: ${events_note}"
echo "Output: ${OUTPUT_PATH}"
