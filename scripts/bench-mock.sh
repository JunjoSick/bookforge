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

if [[ ! -f "${INPUT_PATH}" ]]; then
  echo "Missing benchmark input: ${INPUT_PATH}" >&2
  echo "Set BOOKFORGE_BENCH_INPUT to a tiny EPUB fixture." >&2
  exit 1
fi

rm -f "${EVENTS_PATH}" "${OUTPUT_PATH}"

start_ms="$(date +%s%3N)"
cargo run --release -p bookforge-cli -- translate "${INPUT_PATH}" \
  --target Italian \
  --provider mock \
  --model mock-prefix-target \
  --profile v1-fast \
  --ui quiet \
  --progress-jsonl "${EVENTS_PATH}" \
  --out "${OUTPUT_PATH}"
end_ms="$(date +%s%3N)"

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
