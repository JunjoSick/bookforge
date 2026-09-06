# Development and QA workflow

## Setup and verification

Use the declared Node version (`.nvmrc`, currently 22), Rust stable with
rustfmt and Clippy, and the committed Cargo/npm lockfiles. CI separately checks
Rust 1.88 MSRV and Windows MSVC. `bash scripts/qa-doctor.sh` reports prerequisites;
it does not install software or change system permissions. Install browser QA with:

```bash
npm ci --prefix qa/browser
(cd qa/browser && npx --no-install playwright install chromium)
bash scripts/verify.sh quick
bash scripts/verify.sh full
```

On Linux, Playwright may need OS libraries: its `install --with-deps chromium`
command installs these and can require administrator access. CI uses that command.
Full verification runs formatting, release-overlay checks, strict workspace
Clippy, Rust tests, examples, dashboard Node tests, a strict no-default-feature
CLI check, and real-server Chromium QA. Separate `fmt`, `clippy`, `test`,
`features`, and `browser` lanes are available and used by CI.

The browser test builds this checkout's binary, starts it on an OS-assigned
loopback port, and uses a fresh writable runtime directory. It checks an
unauthenticated request, bootstrap login, cookie authentication, rendered library,
translation wizard navigation, and sign-out revocation. It neither uses provider
credentials nor mocks browser fetch/DOM behavior. Existing Rust lifecycle tests
cover synthetic-book translation, interruption/resume, and provider failures.
The existing corpus CI validates representative EPUBs with pinned EPUBCheck.

## Worktrees and runtime isolation

```bash
bash scripts/worktree.sh task-name          # fresh origin/main
bash scripts/worktree.sh task-name REF      # explicitly agreed revision
```

The helper fetches origin, records the resolved base SHA, creates a sibling
checkout on `work/task-name`, and prints an environment file to source.
It refuses existing branches/directories through Git; it never resets or copies
uncommitted changes. When continuing another agent's unfinished work, use that
agent's existing worktree by agreement, or first create a reviewable commit/patch.
A new worktree contains committed source only; uncommitted QA scripts will not
appear in it until included in the selected revision.

Use `bash scripts/qa-run.sh serve --bind 127.0.0.1:0` to build this checkout
and start an isolated interactive dashboard. The printed URL contains its login
token. Other CLI commands can use the same wrapper and persistent runtime.

Each checkout has its own `target/` and `.qa/` directories. After sourcing the
printed environment file, run interactive jobs from `$BOOKFORGE_QA_RUNTIME` with
this checkout's absolute binary path; BookForge stores its database, uploads,
and run cache under that working directory's `.bookforge/`. Explicitly place
outputs under `$BOOKFORGE_QA_OUTPUT`. `$BOOKFORGE_QA_CACHE` is available for tools
with configurable caches; it is not a BookForge product environment variable.
Never start a different checkout's binary from PATH to validate a patch.

For concurrent changes, create a disposable integration branch containing both
reviewed commits and run full verification there before merging. Record conflict
resolutions as part of the tested patch. Remove worktrees only after their work
and useful artifacts are preserved; the helper does not delete anything.

## Debug access and evidence

The test runner needs subprocess execution, loopback TCP sockets, and Chromium
sandbox support. Dependency/bootstrap installation needs outbound package-host
access. A restricted agent environment may require a scoped approval for these
operations; repository scripts cannot grant permissions. Retry a denied loopback
operation with the permitted scope before classifying it as a product failure.
No public listener or live provider access is required for routine QA.

Each verification run writes `.qa/runs/<timestamp>-<unique>/` with command output,
versions, HEAD, tracked-patch hash, untracked file hashes, working-tree status,
and exit status. Browser failures retain traces and screenshots; server logs
redact the bootstrap URL token. Browser traces can contain ephemeral test session
cookies, so artifacts are for trusted review and expire from CI after seven days.
The test uses synthetic, isolated state. Do not run it against a personal server.
Local evidence is gitignored. CI uploads Linux QA evidence even on failure.

For local diagnosis, use `RUST_LOG=bookforge=debug`, inspect the run's server log,
and open a failed Playwright trace with `npx --no-install playwright show-trace`
from `qa/browser`. Logs and output artifacts belong to the same recorded patch;
rerun affected checks after changing code. A local Linux pass does not certify
Windows or remote CI; inspect those results separately before merging.

## Real books supplied by the user

Use books the user needs as occasional live-provider quality runs, rather than
scheduling extra paid translations. For each supplied book, establish the target
language, requested provider/model, output location, and acceptable spend from
the request or existing agreement. Ask only for missing necessary choices.
Use configured credentials without copying secrets into logs or reports.

Translate the requested book and preserve its checkpoint/output for useful resume.
Record the source hash, code revision/patch, provider/model, effective settings,
validation results, elapsed time, and reported usage/cost. Inspect representative
source/output passages across the book, including difficult structure such as
footnotes, tables, dialogue, math, and multilingual passages when present. Assess
meaning, omissions, terminology, and readability as well as EPUB structure and
representative rendered pages. Deliver the usable book plus a compact assessment
of strengths, defects, and suggested improvements. Report any unmeasured costs or
uninspected regions explicitly. Keep book text, credentials, and live run state
out of CI artifacts. These runs complement deterministic tests; they do not
replace them or imply authorization for recurring paid jobs.
