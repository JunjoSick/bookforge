# Coding-agent handoff — project remediation (2026-07-10)

## Objective

Finish the full remediation plan agreed with the owner:

1. restore release/version/documentation truth;
2. add Windows and security validation;
3. add durable human corrections and an editable dashboard review loop;
4. make runtime reconfiguration genuinely live and dashboard-accessible;
5. split the largest modules behind behavior-preserving seams;
6. run the full cross-platform, migration, corpus, and release gate.

The owner asked the current agent to stop at the first useful checkpoint and
leave this handoff. Do not discard or rewrite the existing worktree: it is a
useful, compiling checkpoint, but it is intentionally not committed yet.

## Git/worktree state (historical starting checkpoint)

- Repository: `C:\Users\gangi\Desktop\bookforge`
- Branch: `codex/project-remediation`
- Base: `a94c8a8665c9e96f0af16c3b1bb5c8cdcf926cc6` (`origin/main` when work began)
- Worktree: dirty, about 900 inserted lines across 30 tracked/new files.
- Nothing has been committed, staged, pushed, or opened as a PR.
- The pre-sync local v1.8.2 tip is separately preserved on
  `codex/pre-sync-main-20260710`.

Start with:

```powershell
git status --short --branch
git diff --stat
git diff --check
```

### Current authoritative checkpoint (2026-07-13)

- Branch: `codex/project-remediation`; draft PR:
  <https://github.com/JunjoSick/bookforge/pull/27>.
- Artifact-smoke HEAD: `16b5b762` (`release: prepare v2.4.0 artifact smoke`).
- The live-reconfiguration milestone, all requested modularization, the
  pre-0007 migration regression, and the `2.4.0` release-candidate version
  bump are committed and pushed.
- Cargo-dist's expensive `pr-run-mode = "upload"` was enabled only for run
  `29281323279`; all five target archives and both global installers built.
  The normal plan-only configuration is restored in this evidence checkpoint.
- Treat the older dirty-tree/restart notes below as provenance only. Inspect
  `git status --short --branch` and this current checkpoint before acting.
- The Codex permission regression recurred after the 2026-07-13 continuation,
  but the subsequent app restart restored GitHub authentication, workspace
  writes, executable access, and process control. The leftover isolated
  dashboard test server PID `16280` was verified and stopped.

## Completed and verified

### Release and documentation truth

- All workspace crates and path dependencies now use `2.4.0-dev`; `Cargo.lock`
  matches.
- `CHANGELOG.md` now contains v2.3.0 and Unreleased/2.4.0-dev entries.
- `CONTRIBUTING.md` uses the real CI commands, explains the Windows MSVC
  baseline, and no longer claims lifecycle tests need `test/test.epub`.
- The stale CI fixture comment, roadmap status notes, and historical pause bug
  wording were corrected.

### Windows and security foundation

- `.github/workflows/ci.yml` has a `windows-2022` workspace-test job.
- Unix-shell-dependent PDF tests are now `#[cfg(unix)]`, fixing a real Windows
  compilation failure exposed by the new gate.
- `.github/dependabot.yml` covers Cargo and GitHub Actions.
- `.github/workflows/security.yml` adds RustSec (`rustsec/audit-check@v2.0.0`)
  and Rust CodeQL (`github/codeql-action@v4`, `build-mode: none`).
- The security workflow is proven on draft PR #27: RustSec and Rust CodeQL are
  green after the dependency remediation recorded below.

### Durable manual correction engine

- New additive migration:
  `crates/bookforge-store/migrations/0007_v2_4_human_corrections.sql`.
- `translations` now records `origin`, `human_corrected`, and `corrected_at`.
- `JobStore::save_manual_correction` validates state, records `manual`
  provenance, clears QA errors/flags, marks the segment succeeded, and
  recomputes the job status.
- Model, QA, and cache writes refuse to overwrite a human-corrected segment.
- Human corrections are excluded from cross-job translation-cache lookups.
- Store tests prove provenance, freeze behavior, job-local cache behavior, and
  rejection while a job is running/paused.

### CLI correction path

- New command: `bookforge correct <job-id> --segment <id>
  (--text ... | --from-file ...)`.
- Plain text is accepted for a single-block segment. Multi-block corrections
  use JSON:

  ```json
  {"blocks":[{"block_id":"b_000000","text":"..."}]}
  ```

- The command reconstructs the source snapshot, validates marker constraints,
  persists the manual correction, rebuilds the EPUB with the original bilingual
  settings, and runs structural validation.
- Lifecycle coverage proves persistence, rebuild, status recomputation, and
  audit fields in regenerated review JSON.

### Dashboard correction checkpoint

The following is implemented and type-checks, but needs endpoint/UI tests:

- Review JSON includes per-block source/target text, manual provenance,
  correction timestamp, and durable flag state.
- CSRF-protected routes exist for saving a translation, flagging a segment, and
  requesting a single-segment retry with optional guidance.
- Review UI renders editable per-block textareas, Save & rebuild, durable Flag,
  and Re-translate with hint controls.
- Dashboard flags use `segment_flags` instead of browser `localStorage`.
- Retry guidance is persisted in `segment_flags`, loaded during resume, rendered
  into single-segment prompts, and serialized per item as `retry_guidance` in
  batch prompts.

## Verification already run

The original Windows GNU host was incomplete. LLVM-MinGW UCRT was installed via
Winget and the existing Rust gnullvm toolchain was used. Its bin directory is:

```text
C:\Users\gangi\AppData\Local\Microsoft\WinGet\Packages\MartinStorsjo.LLVM-MinGW.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\llvm-mingw-20260616-ucrt-x86_64\bin
```

Passing checks:

```powershell
$env:PATH='<LLVM-MinGW bin>;' + $env:PATH
$env:RUSTFLAGS='-D warnings'
cargo +stable-x86_64-pc-windows-gnullvm check --workspace --all-targets --locked

cargo +stable-x86_64-pc-windows-gnullvm test -p bookforge-store manual_correction --locked
cargo +stable-x86_64-pc-windows-gnullvm test -p bookforge-llm prompt_renders_glossary_json_prose_and_prompt_extra --locked
cargo +stable-x86_64-pc-windows-gnullvm test -p bookforge-llm batch_items_include_segment_glossary --locked
cargo +stable-x86_64-pc-windows-gnullvm test -p bookforge-cli --test lifecycle cli_correct_persists_manual_blocks_and_rebuilds_output --locked
cargo fmt --all
git diff --check
```

The corpus, macOS, release-build, installer, and real-provider scenarios have
not yet been run for this branch. The continuation log below records the full
workspace suite, exact CI clippy command, MSRV check, and GitHub publication.

## Claude continuation log (2026-07-10/11)

Claude Code took over from this handoff and is executing the "Immediate next
work" items below as narrow-scope subagent work packages (WP). Status is kept
current here; if this session also ends, resume from the first non-done WP.
The completed work packages were committed as `50542786`; later validation
notes and clippy cleanups are recorded in the continuation below.

| WP | Scope | Status |
| --- | --- | --- |
| WP1 | Prompt version bump v2→v3 for the six `retry_guidance` batch templates (critical item below); cache non-reuse test | DONE — see notes below the table |
| WP2 | Store tests: `set_dashboard_segment_flag`, `request_segment_retry`, guidance load/consume, frozen-segment rejection (item 1) | DONE — 6 tests added in `db.rs` tests module, 33/33 store tests pass. See finding below. |
| WP5 | Correction atomicity: staged rebuild → DB persist → atomic replace (item 5) | DONE — `correct_job_segment` now rebuilds from the pre-persist in-memory merged blocks to `<stem>.staged-<pid>-<nonce><ext>`, validates the staged file, persists the DB, then `fs::rename`s atomically. Rename failure after persist keeps the staged file and the error names both paths + recovery options. Covers CLI and dashboard (both call `correct_job_segment`). New test `cli_correct_with_marker_violation_leaves_db_and_output_unchanged`; success test now asserts no stray staged files. 37/37 lifecycle tests pass. |
| WP6 | Report artifacts record manual provenance and refresh after correction (item 6) | DONE — QA report (`{stem}.report.json`/`.md`, the only auto-written per-segment artifact) gains additive `corrected_segments` count and is regenerated from store data after each correction via new `regenerate_report_after_correction` (best-effort, logged on failure since DB+EPUB are already durable). Review JSON is on-demand and already covered; `.validation.json` has no translation content. Note: review.rs emits `human_corrected`/`corrected_at` but no `origin` field, contrary to earlier wording above. Lifecycle test extended to prove report refresh (0→1). |
| WP3 | Router CSRF tests + isolated-store e2e dashboard test; store path into `AppState` (item 2) | DONE — `AppState` now carries `store_path` resolved once at server construction (was per-request cwd-based `open_default()`, 13 call sites); 3 CSRF tests (no-token + wrong-token → 403 + no store mutation, per endpoint) and `dashboard_review_and_mutation_endpoints_end_to_end` (isolated temp store: review fetch, save correction, flag set/clear, guided retry, frozen-segment 400) in `serve.rs`'s test module (crate has no lib target, so router tests live there). No new deps. |
| WP4 | Lifecycle test: retry guidance survives restart/resume, reaches single+batch prompts, consumed only on terminal result (item 3) | DONE — 3 tests added to lifecycle.rs (single-segment mode, batch mode, frozen-segment rejection through the real `correct` CLI), 40/40 pass. Two documented gaps (see notes below the table). |
| — | Manual dashboard exercise on a mock job (item 4) | DONE — owner completed the click-path and the store, events, report, logs, and retry-guidance consumption were independently checked; see below |

Planned order: WP1+WP2 (parallel, disjoint crates), then WP5, WP6, WP3, WP4
sequentially (all touch `bookforge-cli`). Verification per WP uses the
gnullvm toolchain commands from "Verification already run".

### Milestone status after the continuation (dashboard correction milestone)

All six items in "Immediate next work" are DONE and verified; the critical
prompt-versioning follow-up is RESOLVED. The manual dashboard exercise closed
the final open item on 2026-07-11.

Final composed verification on this Windows gnullvm host (2026-07-11, after
all work packages merged in the shared worktree; the retry guard landed after
this pass, followed by green re-runs of the full store and cli suites):

- `cargo check --workspace --all-targets --locked` with `-D warnings` — clean
- `cargo test --workspace --locked` — 548 passed, 0 failed
  (bookforge-cli: 142 unit + 40 lifecycle + 16 roundtrip; core 57; epub 76;
  pdf 8; llm 144; llm integration 31; store 34)
- `cargo fmt --all --check` — clean
- `git diff --check` — clean (only benign autocrlf warnings)

On 2026-07-11 the owner approved committing this checkpoint as a single
commit on `codex/project-remediation` (supersedes the "nothing has been
committed" note in "Git/worktree state" above), and approved adding the
`request_segment_retry` running/paused guard (see the resolved WP2 finding
below). The branch was pushed and draft PR #27 was opened:
<https://github.com/JunjoSick/bookforge/pull/27>.

### Manual dashboard exercise — DONE (2026-07-11)

The exercise used isolated workspace
`tmp/manual-dashboard-20260711`, mock job
`job_1783791354858557100_52d94333cfc2`, and dashboard
`http://127.0.0.1:8876/`. The owner completed the full click-path:

1. Library → completed mock job → Review.
2. Flag a segment → refresh → confirm the flag persists.
3. Edit translation → Save & rebuild → confirm "human correction saved" is
   shown next to Re-translate.
4. Stop before requesting a guided retry (running/paused jobs are rejected).
5. Re-translate with hint → enter guidance → Resume.

The owner corrected two different segments, so the final report correctly
shows `Manually corrected: 2`. Independent inspection found no server errors,
two durable `manual`/`human_corrected` translations, one pending retry with
the supplied `dashboard_retry` hint, and the expected control signal. A CLI
`resume` then completed the pending segment: its attempts increased from 1 to
2, the hint became consumed, the job returned to `succeeded`, the report
showed `Retried: 1` and `Retry pending: 0`, and the expected resume/request/
checkpoint/artifact/finished events were appended.

UX finding: dashboard Resume writes a control signal for an existing process;
it cannot restart a translation process that has already exited. The hint UI
also currently uses a prompt dialog. Both should be addressed alongside the
live-reconfiguration/dashboard work: make the stopped-process action explicit
(spawn/offer `bookforge resume`, or clearly present the command) and replace
the prompt with inline hint controls.

### Cheap local release-gate checks — DONE (2026-07-11)

- Exact CI lint command passed:
  `cargo +stable-x86_64-pc-windows-gnullvm clippy --all-targets --all-features -- -A clippy::too_many_arguments -D warnings`.
- MSRV passed with Rust 1.88 GNU and WinLibs on PATH:
  `cargo +1.88.0-x86_64-pc-windows-gnu check --workspace --all-targets --locked`
  (`CARGO_TARGET_DIR=target/msrv-1.88`). Rust 1.88 did not publish a Windows
  gnullvm host toolchain, which is why this check uses the GNU host.

### First GitHub workflow exercise — DONE (2026-07-11)

Draft PR #27 successfully exercised the previously unrun workflows. At commit
`ef08182a`, the following checks are green:

- Linux workspace tests, Windows MSVC workspace tests, and MSRV 1.88;
- exact CI clippy and formatting;
- small Standard Ebooks corpus smoke test;
- release planning (artifact jobs correctly skip on a pull request);
- CodeQL Rust analysis.

The initial failing check was RustSec. Its log reported three vulnerable locked
dependencies and one warning:

- `quick-xml 0.38.4`: RUSTSEC-2026-0194 and RUSTSEC-2026-0195, patched in
  `>=0.41.0`. This is a direct workspace dependency (`Cargo.toml` currently
  requests `0.38`), so it needs a manifest bump plus any API adaptation.
- `quinn-proto 0.11.14`: RUSTSEC-2026-0185, patched in `>=0.11.15`. It is a
  locked transitive dependency from the HTTP stack; first try a precise
  lockfile update to `0.11.15` or newer allowed by its parent.
- `anyhow 1.0.102`: RUSTSEC-2026-0190 unsoundness warning. The advisory output
  names the fixing upstream commit but no patched-version range. Update to the
  first published release containing that fix after verifying its advisory
  metadata; do not add an ignore merely to make the check green.

Resolved in `5ad38b35`: `quick-xml` was upgraded to 0.41.0 and its deprecated
attribute APIs were migrated to equivalent explicit XML 1.0 normalization;
`quinn-proto` was locked to 0.11.15 and `anyhow` to 1.0.103. XML-heavy tests,
CLI round trips, the full workspace suite, exact clippy, and MSRV 1.88 passed
locally. PR #27 then passed RustSec, CodeQL, Linux and Windows tests, clippy,
format, MSRV, small corpus, and release planning. The GitHub logs retain
non-blocking Node 20 deprecation warnings for
`actions/checkout@v4`, `actions/setup-python@v5`, `actions/setup-java@v4`, and
`rustsec/audit-check@v2.0.0`; update actions where supported, but keep that
separate from the security dependency fix.

### Next steps for the next agent (written 2026-07-11)

1. Implement the accepted live-reconfiguration design in
   `docs/design-live-reconfiguration.md`, including inline retry-hint controls
   and an actionable stopped-process resume path.
2. Modularization only after live reconfig is behavior-locked; then the
   final release gate (both unchanged below).

Known flake: `cli_stop_then_resume_mock_run` occasionally fails with "stop
test should checkpoint at least one segment" (a ~50 ms sleep race in the
fixture); it passes on re-run and is unrelated to this branch's changes.
Worth deflaking when lifecycle tests are next touched.

### WP1 notes (prompt version bump — the critical item is resolved)

- The six changed batch templates were renamed `.v2.md` → `.v3.md` (`git mv`,
  so these renames are the only staged entries) and `prompt.rs` now parses
  them as `"v3"`. `translate_batch_repair(_compact)`, single-segment, and QA
  templates were untouched.
- Key finding: `segments.prompt_version` (the actual cache-key column) is NOT
  fed by `PromptTemplate.version`; it comes from `PromptVersion` in
  `bookforge-core/src/config.rs` via `translate/mod.rs` (~line 890). A
  file rename alone would have left the cache key at `"batch_v2"` and stale
  pre-retry_guidance cache entries reusable. Fixed by adding
  `PromptVersion::BatchV3` and switching real batch runs to it (mirrors the
  historical `BatchV1`→`BatchV2` bump). `BatchV2` variant kept for existing
  DB rows.
- Old v2 template files are safely deleted: templates are compile-time
  `include_str!`, and resume renders with the current binary's templates while
  keeping the job's original prompt_version tag purely as a cache/provenance
  string (`resume.rs` ~280–410). Known nuance: a pre-bump job resumed under
  the new binary renders v3 prompt text but keeps its `"batch_v2"` cache tag —
  job-internal cache continuity is preserved by design.
- New tests: `batch_prompt_templates_are_versioned_v3_for_retry_guidance`
  (bookforge-llm) and `cached_translation_rejects_mismatched_prompt_version`
  (bookforge-store, covers single + batch lookup). CHANGELOG Unreleased bullet
  added.
- Verified: workspace check clean (`-D warnings`), bookforge-llm 144/144,
  bookforge-store 34/34, fmt, `git diff --check`.

### WP4 coverage gaps (documented, deliberate)

- Lifecycle tests cannot assert the literal prompt text contains the guidance:
  `MockProvider` transforms source text only and exposes no prompt capture to
  the test subprocess. Prompt content IS asserted at unit level
  (`bookforge-llm` scheduler test and batch.rs ~4205). Lifecycle tests assert
  the strongest observable proxy (guidance present pre-resume, attempts++,
  correct single/batch code path via progress events, guidance consumed
  post-resume). If prompt-level lifecycle coverage is ever wanted, add an
  env-var-gated prompt log to `MockProvider::complete`.
- "Guidance survives a non-terminal failure" cannot be scripted: the mock
  provider never returns a retryable error (its only failure mode routes to
  the terminal `needs_review` path). Documented as a gap, not force-tested.

### WP2 finding — RESOLVED (owner decided 2026-07-11)

`request_segment_retry` originally had no job-state guard. The owner chose
rejection: it now rejects `running`/`paused` jobs with
`StoreError::InvalidCorrection`, mirroring `save_manual_correction`. Test
renamed to `request_segment_retry_rejects_running_and_paused_jobs`; three
sibling store tests updated to mark jobs `needs_review` before retrying. The
dashboard endpoint already maps the error to a 400. If "queue a retry against
a live job" is ever wanted as UX, design it as part of the live-reconfig
milestone (apply at batch boundaries), not by removing this guard.

## Immediate next work (finish dashboard milestone)

1. Add store tests for `set_dashboard_segment_flag`,
   `request_segment_retry`, guidance loading/consumption, and rejection of retry
   for frozen manual segments.
2. Add router tests proving all three new mutation endpoints reject missing or
   cross-site CSRF requests. Add an end-to-end dashboard/API test using an
   isolated store; avoid process-global current-directory races in parallel
   tests (consider making the store path part of `AppState`).
3. Add a lifecycle test showing retry guidance survives process restart/resume,
   reaches the prompt for both single and batch modes, and is consumed only
   after a terminal provider result.
4. Manually exercise the dashboard on a mock job: flag, refresh, edit/save,
   confirm output rebuild, queue a guided retry, stop/resume, and inspect the
   prompt/event/report state.
5. Improve correction atomicity. Current order is DB save then EPUB rebuild; a
   rebuild failure can leave the DB corrected while the output file is stale.
   Preferred fix: validate/rebuild to a staged sibling path, persist the DB
   transaction, then atomically replace the output, with a clear recovery path
   if the final rename fails.
6. Ensure generated report artifacts (not only on-demand review JSON) record
   manual provenance and are refreshed after a correction.

### Critical prompt-versioning follow-up — RESOLVED (see WP1 notes above)

Batch prompt files were changed to teach models about `retry_guidance`, but the
prompt/cache version has not yet been bumped. This violates the repository's
prompt-versioning invariant if left as-is. Before committing:

- inspect `PromptVersion` and the cache namespace rules;
- bump the appropriate prompt minor/major version;
- preserve old templates if resume/backward compatibility requires them;
- add tests proving old cache entries are not reused under changed prompt text.

Do not skip this item.

## Remaining remediation milestones

### True live reconfiguration

Design and implementation are complete in
`docs/design-live-reconfiguration.md`. Commit `29ee6077` is pushed and the
mock/automated acceptance matrix is green. The selected real-provider
acceptance run remains part of the final release gate; the historical restart
and WIP-review notes below are retained only as provenance.

#### Live implementation checkpoint (Codex, 2026-07-13)

This checkpoint supersedes both historical subsections below.

- Atomic revisioned overrides, shared validation, last-valid recovery,
  runtime channels/events/replay state, provider-attempt snapshots, dynamic
  single and batch dispatch, adjustable concurrency, stable batch work keys,
  pending repartition, and finalize-stage snapshots are implemented.
- Runtime leases and launch claims make dashboard controls worker-aware.
  Resume signals a fresh worker, launches one replacement for stopped/crashed
  work, adds `--force` for a dead paused worker, deduplicates concurrent clicks,
  recognizes finalize-only work, and rejects completed jobs.
- The Progress screen contains the typed ten-field Runtime settings editor and
  immutable identity/revision/lease state. Runtime events refresh it without a
  page reload. Review retry guidance now uses an inline textarea with
  Cancel/Queue/Stop and the explicit stop-before-retry instruction.
- Lifecycle coverage proves live single/batch changes, request revision and
  attempt metadata, finalize-stage freezing, stop/crash preservation, resume
  consumption without retranslating checkpoints, successful sidecar/lease
  cleanup, and stale crash leases. The old fixed-sleep stop/resume test was
  made event-driven and passes repeatedly.
- Focused and full engine/CLI suites pass (154 engine; 159 CLI unit; 45 CLI
  lifecycle; 16 round-trip). Formatting, exact CI clippy with warnings denied,
  and the Rust 1.88 all-target workspace check pass on Windows. After replacing
  the old fixed-sleep lifecycle flake with an event-driven boundary, the clean
  linked workspace suite passes in full: 159 CLI unit, 45 lifecycle, 16
  round-trip, 59 core, 154 LLM, 31 PDF, and 34 store tests, plus EPUB and
  documentation tests.

The live implementation was committed, pushed, and confirmed on Linux and
Windows before behavior-neutral modularization began.

#### Historical restart checkpoint (superseded)

This subsection supersedes the older WIP review immediately below it. The app
had to be restarted because its approval state was malfunctioning. Stop here:
the worktree is intentionally dirty and must be continued in place.

- Branch/HEAD: `codex/project-remediation` at `b809e598`.
- Do not discard, reset, or broadly rewrite the dirty tree. There are 18
  modified Rust files plus this handoff.
- `crates/bookforge-cli/.bookforge/` is an untracked generated test artifact;
  inspect it before removing it and never commit it.
- The last fully verified checkpoint predates the newest lease/dashboard
  edits. Run formatting and a targeted CLI check before making further edits.

Use the Windows gnullvm toolchain because this host's default GNU linker is
missing `libgcc_eh`:

```powershell
$llvm='C:\Users\gangi\AppData\Local\Microsoft\WinGet\Packages\MartinStorsjo.LLVM-MinGW.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\llvm-mingw-20260616-ucrt-x86_64\bin'
$env:PATH="$llvm;$env:PATH"
cargo fmt --all
cargo +stable-x86_64-pc-windows-gnullvm check -p bookforge-cli --all-targets --locked
```

Implemented in the current uncommitted tree:

- removed the obsolete paused-resume override rejection; overrides are loaded
  and published before Resume is applied, with focused tests;
- corrupt sidecars can be recovered by the next atomic writer while readers
  keep last-valid behavior;
- provider attempt limits and runtime revision are frozen per request and
  recorded additively in request metadata/events;
- batch dispatch now leaves genuinely pending work available for revision-time
  repartition, with per-item retry/escalation bookkeeping and live adaptive
  concurrency behavior;
- CLI-owned `JobRuntimeSettings` snapshots QA, double-check, validation, and
  engine settings at stage/request boundaries;
- persisted run snapshots now include additive QA mode and output-validation
  fields, and successful finalization clears overrides;
- `runtime.json` worker leases, heartbeats, owner-safe cleanup, stale detection,
  and deduplicating `resume.launch` claims are implemented in `control.rs`;
- dashboard GET/POST `/api/jobs/{id}/reconfigure` handlers and lease-aware
  Pause/Stop/Resume behavior are largely implemented in `serve.rs`. Resume now
  signals a fresh worker or spawns `bookforge resume <id> --ui quiet` when no
  worker is live.

Focused tests that passed before the newest lease/dashboard edits cover sidecar
recovery/order, provider-attempt freezing, additive event compatibility,
single and batch runtime boundaries, pending-batch merging, adaptive-concurrency
enablement, and limiter shrink. A targeted CLI all-target check also passed at
that earlier point. The lease tests and newest dashboard handlers have not yet
been compiled or run.

First continuation task:

1. Run the formatting/check commands above and fix only errors from the current
   WIP (likely around the new lease-aware dashboard handlers or new
   `RunConfigSnapshot` fields).
2. Finish the Progress-screen runtime settings panel and event rendering in
   `serve.rs`.
3. Replace `window.prompt` in `bfReviewRetry` with an inline textarea plus
   Cancel/Queue controls and the explicit instruction that a running/paused job
   must be stopped before a retry is queued. This is the owner's requested UI
   follow-up; the underlying retry behavior already works.
4. Add dashboard API/CSRF tests, fresh/stale lease and launch-dedup tests, then
   stage-boundary lifecycle/race coverage. Run the full llm/cli suites.
5. Update this subsection with exact commands/results, commit the coherent live
   reconfiguration milestone, and push draft PR #27. Only then begin the
   modularization milestone and final release gate.

Potential review points, not established bugs: verify Windows
`CommandExt::creation_flags` compiles through Tokio's command wrapper; ensure
isolated dashboard tests do not leak global `.bookforge` state; verify a newly
written sidecar is observed before a stage boundary; and confirm launch claims
remain until the new worker clears them or they become stale.

#### Historical review of the work-in-progress (superseded)

Implemented and looking correct (roughly steps 1–4 of the design's
implementation order):

- versioned `overrides.json` envelope with schema version, monotonic
  revision, zero-value rejection, legacy flat fallback, atomic staged write,
  and an `overrides.lock` writer lock with stale-lease recovery;
- `ControlFileWatcher` publishes immutable `EngineRuntimeSettings` snapshots
  through a Tokio `watch` channel, emitting additive `RuntimeConfigChanged`/
  `RuntimeConfigRejected` events (deduped per revision / per error);
- `RunState` gains revision/changed-fields/rejection (serde-defaulted);
- single-segment scheduler snapshots concurrency + output budget per
  dispatch; batch scheduler uses an `AdaptiveLimiter` (including a real
  shrink fix: idle permits are consumed immediately so waiters cannot exceed
  a lowered target — with test) and rebuilds sizing per revision;
- escalation bookkeeping is now keyed by ordered item IDs
  (`batch_work_key`), surviving repartition, and carried onto split parts;
- all four run entry points (translate real/mock, resume, fallback pass)
  wire the watcher channel through `TranslationRunConfig.runtime_settings`
  (`None` preserves old behavior);
- race tests exist with gated providers for concurrency shrink (single +
  batch) and budget change on later requests.

Findings to fix before continuing:

1. FAILING TEST: `paused_fast_resume_with_overrides_errors_with_apply_guidance`
   (resume.rs:1301) — `apply_instructions()` was reworded for the live-worker
   model and no longer contains "bookforge stop <id>". Decide the real fix:
   under this design a live fast-resume of a paused worker should be ALLOWED
   with pending overrides (the watcher applies them), so the guard in
   `live_fast_resume_paused_job` likely becomes obsolete rather than the
   message being patched. Workspace test run stops at this failure, so
   lifecycle/roundtrip suites have not run against the WIP.
2. ORDERING BUG (design invariant): in `control.rs` the watcher loop runs
   `poller.poll(&signal)` (applying Pause/Resume/Stop) BEFORE reloading the
   overrides sidecar. The design requires reloading overrides before applying
   Resume so reconfigure-then-resume never dispatches under the previous
   revision. Swap the order (load/publish overrides first, then poll) and add
   the planned pause→reconfigure→resume scheduler test.
3. Recovery nit: `write_merged_overrides_at_path` reloads the existing
   sidecar through the validating loader, so a corrupt/invalid sidecar makes
   every future reconfigure fail until the file is hand-deleted. Writers
   should treat unreadable existing content as revision-0 default (or surface
   an explicit `--reset`), while readers keep last-valid behavior.

Not yet implemented (matches design steps 3b, 5, 6, 7):

- provider-side `provider_max_attempts` snapshot in
  `OpenAiCompatibleProvider` and `adaptive_concurrency` application — both
  fields currently ride in `EngineRuntimeSettings` but nothing consumes them;
- `RequestStarted` extensions (`runtime_config_revision`,
  `provider_max_attempts`);
- runtime lease `runtime.json` + lease-aware dashboard Resume/spawn;
- QA/double-check/validate stage-boundary snapshots and terminal
  sidecar/lease cleanup;
- dashboard GET/POST reconfigure API, Progress-screen panel, inline
  retry-hint UI;
- most of the design's unit/scheduler/lifecycle test matrix.

Minor observations (acceptable, just be aware): the watcher re-emits
`RuntimeConfigChanged` once at every process start when a sidecar already
exists (informative, additive); a legacy revision-0 sidecar intentionally
does not instantiate a runtime batch sizer.

Current behavior for jobs without overrides remains unchanged
(sidecar + stop/resume fallback preserved).

Recommended design:

- create a shared runtime-settings snapshot/channel (Tokio `watch` is already
  available; avoid a new dependency);
- have the long-lived control watcher reload validated `overrides.json` and
  publish immutable snapshots;
- apply allowed scheduling/budget settings only at segment/batch boundaries,
  never mutate in-flight requests;
- keep provider/model/language/prompt/cache identity immutable;
- add additive runtime-config events and expose the editable settings on the
  dashboard Progress screen;
- test pause/reconfigure/resume races, adaptive batch renaming, finalize stages,
  and cleanup after success/stop/crash.

Authoritative starting points:

- `crates/bookforge-cli/src/commands/reconfigure.rs`
- `crates/bookforge-cli/src/commands/resume.rs`
- `crates/bookforge-cli/src/control.rs`
- `crates/bookforge-llm/src/scheduler.rs`
- `crates/bookforge-llm/src/batch.rs`
- ROADMAP sections 10.1.3 and 10.1.4.

### Modularization

DONE and pushed as behavior-neutral commits after live reconfiguration was
locked by tests:

- Dashboard HTML/CSS/JS moved to embedded source assets in `bb76b94c`; Windows
  CRLF normalization and synthetic regression coverage followed in
  `cc9805e8`. The assembled LF dashboard remains byte-stable at 82,407 bytes,
  SHA-256 `7a37e7095182825d2f63afec9776214ce7f99ea33464ad1e86ea43342767ce9b`.
- LLM batching split into planning, rendering, escalation, execution, and tests
  across `e7101459`, `19ea4350`, `436a3511`, `7548126f`, and `48500348`.
- Store schema/migrations, jobs, translations/cache, flags, glossary, and tests
  split across `d180fbfa`, `076f3d76`, `3de00331`, `f73cca00`, `62829ecb`,
  and `2e0de4bf` while retaining `JobStore`'s public surface.
- PDF conversion tests, reporting, rendering, and media detection split across
  `c5476b44`, `e3276721`, `1a57664e`, and `44278107`.
- CLI translate tests, orchestration, and finalization split across
  `c38a7843`, `94f73434`, and `1c4e8fb9`.

Every extraction passed package tests and warnings-denied clippy. The final
shape then passed the full local workspace suite and the dispatched Linux,
Windows MSVC, MSRV, and full-corpus run recorded below.

### Final release gate

Automated gate status on 2026-07-13:

- DONE — `cargo fmt --all --check` and the exact CI clippy command with
  warnings denied pass locally and in GitHub Actions.
- DONE at the modularized source head — the full locked workspace suite passes
  locally (160 CLI unit, 45 lifecycle, 16 round-trip, 59 core, 154 LLM, 31
  PDF, and 34 then-existing store tests, plus EPUB and documentation tests).
  The subsequently added 35th store test (the migration fixture below) passes
  in its targeted run. A post-version-bump local rerun was attempted, but the
  Codex permission regression denied execution of the configured LLVM linker;
  final-head CI remains the authoritative rerun.
- DONE — workflow-dispatch run
  <https://github.com/JunjoSick/bookforge/actions/runs/29280629915> passed Linux
  and Windows MSVC workspace tests, Rust 1.88, exact clippy, formatting, and
  the full nine-book Standard Ebooks corpus with EPUBCheck. PR runs also pass
  the small corpus.
- DONE — `03dd379b` adds and passes an explicit migrations-1-through-6 fixture
  proving migration 0007 preserves existing translations/blocks, defaults
  audit fields correctly, records version 7, and is idempotent.
- DONE (build generation) — cargo-dist run
  <https://github.com/JunjoSick/bookforge/actions/runs/29281323279> passed native
  x86_64/aarch64 macOS, x86_64/aarch64 Linux, Windows MSVC, and global
  archive/checksum/shell/PowerShell-installer generation for v2.4.0; host and
  announce correctly skipped on the PR.
- DONE — the owner's mock dashboard correction click-path and subsequent
  store/event/report/log inspection are recorded above. Automated dashboard
  CSRF/API, lease-aware controls, correction/retry, live reconfiguration, and
  single/batch/finalize lifecycle coverage all pass. A separate delayed mock
  release-gate run completed 43/43 requests without server errors.
- DONE — RustSec and CodeQL are green on draft PR #27.
- DONE — `16b5b762` changes all workspace/path-dependency/lockfile versions
  from `2.4.0-dev` to `2.4.0`, completes release notes, and refreshes stale
  roadmap release truth.

Still required before tagging:

- execute the generated shell/PowerShell installers on native macOS, Linux,
  and Windows and verify `bookforge --version` after installation (artifact
  generation is proven; installer execution is not yet proven);
- run the selected real-provider correction, guided-retry, pause, and live
  reconfiguration scenario and inspect its persisted events/store/output;
- after those two manual/credentialed gates, push the restored plan-only dist
  config plus final evidence, then require the final-head CI, RustSec, and
  CodeQL checks to be green.

## Constraints to preserve

- The program owns EPUB structure; the model never emits/repairs XHTML.
- Rebuild remains deterministic and structurally validated.
- Human corrections are frozen, auditable, job-local, and never cached across
  jobs.
- Runtime reconfiguration cannot change cache-affecting identity.
- Event and CLI changes are additive/semver-safe.
- No hosted/multi-user expansion; dashboard remains loopback-only and
  CSRF/Host protected.
- Single-binary distribution remains intact.
