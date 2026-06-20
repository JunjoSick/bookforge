#!/usr/bin/env bash
set -euo pipefail

EPUBCHECK="${BOOKFORGE_EPUBCHECK:-epubcheck}"
if [[ "$EPUBCHECK" == *.jar ]]; then
  exec java -jar "$EPUBCHECK" "$@"
fi
exec "$EPUBCHECK" "$@"
