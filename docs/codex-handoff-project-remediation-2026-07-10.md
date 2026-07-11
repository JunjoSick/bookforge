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

## Git/worktree state

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
- The security YAML has not run on GitHub yet; validate with actionlint/CI before
  treating it as complete.

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

### Next steps for the next agent (written 2026-07-11)

1. Watch draft PR #27's first Windows CI and security runs and fix any real
   workflow or code failures they expose.
2. Live reconfiguration milestone: follow the recommended design under
   "True live reconfiguration" below. Write a short design doc first
   (snapshot/watch-channel shape, which settings are boundary-applied,
   event additions, dashboard exposure, race-test matrix) before coding.
3. Fold the dashboard UX findings above into that design: inline retry-hint
   controls and an actionable stopped-process resume path.
4. Modularization only after live reconfig is behavior-locked; then the
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

Not started in this branch. Current behavior remains sidecar + stop/resume.

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

Not started. Do it only after dashboard correction and live reconfiguration are
behavior-locked by tests.

Suggested seams:

- split `bookforge-llm/src/batch.rs` into planning, rendering, execution,
  truncation/escalation, and tests;
- split store migrations/schema, jobs, translations/cache, flags, and glossary
  out of `bookforge-store/src/db.rs` while preserving `JobStore`'s public API;
- split PDF detection/rendering/reporting from `bookforge-pdf/src/convert.rs`;
- split translate orchestration/finalization from
  `bookforge-cli/src/commands/translate/mod.rs`;
- move dashboard CSS/JS/HTML to separate source files embedded with
  `include_str!`, retaining the single-binary invariant.

Each extraction should be its own behavior-neutral commit with byte-stability or
equivalent regression evidence.

### Final release gate

Still required:

- `cargo fmt --all --check`;
- exact CI clippy command with warnings denied;
- full workspace tests on Linux and Windows MSVC;
- MSRV 1.88 check;
- small and full Standard Ebooks corpus;
- migration tests from pre-0007 databases;
- macOS/Windows/Linux release builds and installer smoke tests;
- mock dashboard correction/reconfiguration runs;
- selected real-provider correction, guided retry, pause, and live-reconfigure
  scenarios;
- RustSec and CodeQL green runs;
- complete v2.4.0 changelog/release notes and final version bump from
  `2.4.0-dev` to `2.4.0`.

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
