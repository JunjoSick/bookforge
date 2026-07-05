# Fix pass — pause/stop finalize review (a5e3002), pre-merge PR #25

You (Codex) just reviewed commit `a5e3002` on `feat/pause-stop` and returned
**BLOCK MERGE** with 7 findings (Codex thread 019f3335). Implement the fixes now.
This ships to `main` after Claude re-verifies live, so correctness is paramount.

## Invariants you must not violate

- **PAUSE = park.** Stop starting NEW provider work, but let any single in-flight
  request finish so the job resumes cleanly. The job must **never** reach
  `mark_job_finished` / emit a terminal `TranslationFinished` while state is
  `Paused`. On resume it continues from where it parked.
- **STOP = abort + leave resumable.** Cancel in-flight work and skip remaining
  stages, leaving the job so `bookforge resume` re-runs **exactly** the skipped
  stages. `stopped` is sticky and must never be downgraded to running/paused.

## Fixes (apply your own proposed minimal fixes; clarifications below)

1. **[CRIT] Pause during the final double-check still finishes the job.** Add a
   pause/stop boundary AFTER `run_double_check_pass` (and after persisting
   corrections) and BEFORE `mark_job_finished`, in BOTH `finish_translation_pipeline`
   and `run_mock_translation`. If paused → park (`wait_until_running_or_stopped`);
   if stopped → print the resume hint and return WITHOUT finishing.

2. **[CRIT] Stop overwritten by concurrent pause/resume/status writes.** Make
   `PauseSignal` transitions atomic (compare-and-swap) so a transition reports
   whether it won; re-check `stopped` immediately before every store write; make
   the DB status transition conditional so `stopped` can never be overwritten by a
   later pause/resume/`save_translation`/`touch_job`. Ensure the run-long
   `ControlFileWatcher` poller and the per-stage `ControlFilePoller` cannot race to
   emit duplicate/contradictory `JobPaused`/`JobResumed` or downgrade `stopped`.

3. **[HIGH] Stop doesn't cancel in-flight requests / retries.** On **STOP only**,
   cancel the provider `CancellationToken` so in-flight QA/double-check/fallback
   requests and the OpenAI-compatible retry loop (sends / body-reads / retry-delays,
   provider.rs ~779/826/897/950) abort promptly. Do **NOT** cancel on mere pause —
   in-flight must finish for a clean resume. Verify the token is threaded to all
   finalize provider calls.

4. **[HIGH] Resume can't rerun a skipped fallback pass.** Snapshot fallback
   provider/model/scope in the run snapshot; on resume run the fallback stage when
   it was skipped/incomplete, matching the fresh-run order **QA → fallback →
   double-check**. Add resume args/fields as needed.

5. **[HIGH] Resume double-applies corrections and can leave the job `running`.**
   Make finalize-stage corrections idempotent (a completion checkpoint so a re-run
   doesn't re-apply corrections to already-corrected stored blocks). Recompute +
   persist the final job status after persisting corrections; resume must call
   `mark_job_finished` (or equivalent) before emitting the terminal
   `TranslationFinished` so a completed job is never left `running`.

6. **[MED] Resume progress attribution.** Wrap resume QA with
   `ProgressRequestProvider` so finalize request-ids are emitted; populate
   double-check request metadata with the ACTUAL finalize provider/model when a
   separate double-check provider was built.

7. **[MED] Tests would pass with all the above bugs.** Add regression tests (the
   current ones pause BEFORE finalize via `BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS`
   and never during an in-flight request):
   a. Pause DURING an in-flight QA request AND during an in-flight double-check
      request (not at the stage boundary); assert the job does not finish and no
      new finalize `RequestStarted` appears until resume, then resume completes.
   b. Stop during finalize, then resume, and assert the **fallback** pass runs on
      resume.
   c. After stop→resume completion, assert the final DB job status is the terminal
      finished state (not running/paused).
   d. A stopped run that persisted corrections, then resume — assert corrections
      are NOT double-applied.
   Use the mock provider + `BOOKFORGE_MOCK_*` knobs; add new mock hooks if you need
   to force an in-flight window (e.g. a per-stage delay the test can pause into).

## Working agreement

- Full workspace gates — `cargo check`, `cargo test`, `cargo clippy --all-targets`,
  `cargo fmt --check` — must pass before EACH commit.
- Commit in logical units with conventional-commit messages.
- **Do NOT push.** Leave `feat/pause-stop` ready for Claude's live re-verification
  (pause-during-last-double-check + stop→resume-with-fallback on real DeepSeek).
