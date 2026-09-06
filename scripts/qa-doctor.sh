#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
missing=0
for tool in git cargo rustc node npm python3 java; do
  if command -v "$tool" >/dev/null; then echo "$tool: $(command -v "$tool")"; else echo "Missing: $tool"; missing=1; fi
done
if command -v node >/dev/null; then node -e 'if (+process.versions.node.split(".")[0] < 22) process.exit(1)' || missing=1; fi
cargo fmt --version || missing=1
cargo clippy --version || missing=1
if [[ ! -d qa/browser/node_modules/@playwright/test ]]; then
  echo 'Browser setup: npm ci --prefix qa/browser'; missing=1
else
  node -e 'const {chromium}=require("./qa/browser/node_modules/@playwright/test"); const p=chromium.executablePath(); require("node:fs").accessSync(p); console.log("Chromium: " + p)'  || missing=1
fi
if [[ -z ${BOOKFORGE_EPUBCHECK:-} ]] && ! command -v epubcheck >/dev/null; then
  echo 'Corpus QA also needs EPUBCheck: see scripts/epubcheck.sh and CI installation.'
fi
echo 'Browser installation: cd qa/browser && npx --no-install playwright install chromium'
echo 'Full verification requires loopback sockets and browser/child-process execution.'
exit "$missing"
