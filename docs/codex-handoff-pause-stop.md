# Handoff to Codex — §10.1.1 Pause + Stop (2026-07-04)

Written by Claude Code on behalf of the maintainer. Second of two
quality-of-life milestones after v2.2.0 (the first, §9c reflow, is on
`feat/reflow`).

**The authoritative spec is `docs/ROADMAP.md` §10.1.1 — read it in full
before writing code.** It defines the three pieces (cooperative pause
signal, file-based control channel, surfaces), the acceptance criteria,
and the out-of-scope list. If a needed detail is missing, stop and ask
the maintainer rather than inventing it.

## Non-negotiables (ROADMAP §1 + §10.1.1)

- **Additive only.** A pre-feature job (no control file) must run
  unchanged (acceptance §10.1.1.4). New JSONL events `JobPaused` /
  `JobResumed` are additive per §1.5 — old readers must tolerate them
  (check how unknown events fold in `RunState`).
- **No new dependencies** (§1.6). The control channel is a plain file
  the run loop polls at segment boundaries:
  `.bookforge/runs/<job-id>/control` containing `pause|resume|stop`.
- On *pause*: stop dispatching new segments; in-flight requests finish
  and checkpoint; job status becomes `paused` (status is a plain string
  in `bookforge-store/src/db.rs` — additive value, no migration).
- On *stop*: same drain, then exit the run loop cleanly. A subsequent
  `bookforge resume` must behave exactly as after a kill today.
- Out of scope (binding): aborting in-flight requests mid-segment,
  multi-job queueing, cross-reboot semantics beyond what the file gives.

## Where the work lives

1. **Engine** — `crates/bookforge-llm/src/scheduler.rs`
   (`translate_segments_with_callback` is the dispatch loop) and
   `concurrency.rs`. Add a `PauseSignal` (tri-state run/pause/stop,
   `Arc<AtomicU8>` semantics) checked before each new segment dispatch,
   sibling to the existing `CancellationToken` plumbing in
   `provider.rs`. While paused, park (poll with a short async sleep)
   until resumed or stopped.
2. **Control channel** — the translate run loop (see
   `crates/bookforge-cli/src/commands/translate/`) reads
   `.bookforge/runs/<job-id>/control` at segment boundaries and drives
   the `PauseSignal`. Event log emission: `JobPaused`/`JobResumed`
   ProgressEvents (`crates/bookforge-core/src/progress.rs`), folded
   into `RunState` so watch/serve display the paused state.
3. **Surfaces** —
   - CLI: new `bookforge pause <job-id>` and `bookforge stop <job-id>`;
     `bookforge resume <job-id>` additionally clears/overwrites a
     `pause` control file before its normal behavior.
   - Serve: `POST /api/jobs/{id}/pause|resume|stop` writing the same
     control file, CSRF-protected exactly like the existing POSTs in
     `serve.rs`; wire the Progress screen's Pause/Stop/Resume buttons.
   - TUI: keybindings on `watch` (pick keys consistent with the
     existing keymap; show them in the footer/help).

## Working agreement

- Branch: `feat/pause-stop` (created from main, checked out).
- Conventional commits, logical units, tests with each commit.
- Full workspace check/test/clippy/fmt clean before each commit.
- Unit tests: PauseSignal state machine; control-file parsing
  (missing/garbage file = run); status transitions running→paused→
  running and running→stopped; RunState fold of the new events;
  pre-feature job path (no control dir) untouched.
- End-to-end (mock provider): start a translation, write `pause` to the
  control file mid-run, verify dispatch stops within one segment
  boundary and status shows `paused`; write `resume`, verify completion
  with no re-translation of succeeded segments; separately verify
  `stop` then `bookforge resume` completes the job.
- The un-mockable acceptance (§15.6 — pause a real DeepSeek run from
  the dashboard and watch token spend stop) is the maintainer's/
  Claude's job afterwards, not yours.
- Do not push; leave the branch ready for Claude's review pass.

## Review fix pass (Claude, 2026-07-05 — two findings)

Implementation landed as 852b15d + 99cf88b; workspace green; lifecycle
e2e verified live pause/resume/stop through real child processes. Two
gaps found in review, both to fix:

1. **Dead-paused job is unresumable (medium).** `bookforge resume` on a
   `paused` job only writes `resume` to the control file and returns.
   Correct for a live paused process (relaunching would double-write the
   job), but if the process died/machine rebooted while paused, the DB
   status stays `paused` forever and resume never falls back to a
   relaunch. Ruling: keep the signal path as default, but after writing
   the control file wait up to ~10s for the job to leave `paused`; if it
   doesn't, print a hint explaining that a dead paused run needs
   `bookforge resume <id> --force`. Add `--force`: clears the control
   file, marks the job for relaunch, and takes the normal resume path.
   No liveness heuristics — the user decides; document the double-run
   risk in the flag help ("only use if the paused process is gone").
2. **Fallback pass ignores the control file (low).** `run_fallback_pass`
   inherits the primary's PauseSignal but passes `control: None`, so
   pause/stop written during a fallback pass isn't polled until it ends.
   Wire the same ControlFilePoller through (same pattern as the main
   pass) if lifetimes permit; otherwise leave a code comment and note it
   in ROADMAP §10.1.1 out-of-scope.

Then: regression tests for both (paused-dead resume hint + --force path;
control honored during fallback), full workspace gates, do not push.

## Real-run finding (Claude, 2026-07-05 — batch path pause is inert)

The §15.6 un-mockable check (real DeepSeek run from the dashboard,
pre-armed pause) caught what mock e2e cannot: in batch mode the pause
took no effect — batch_0002's provider request started TWO MINUTES
after the control file said `pause`, and the job never marked `paused`.

Root cause (`batch.rs` ~1479): the dispatch loop spawns ALL pending
batches as tasks immediately (deliberate — the in-code comment explains
context waiters must not consume provider-concurrency slots), with
concurrency enforced by `request_semaphore` inside the tasks. The pause
check only gates spawning from `pending_queue`, which drains in
milliseconds. Mock e2e missed it because mock runs don't take the batch
path.

Fix ruling — keep the spawn-all design, gate the provider call instead:

1. Inside each spawned batch task, BEFORE acquiring
   `request_semaphore` (never while holding a permit), wait on
   `signal.wait_until_running_or_stopped()`. On Stopped, return a
   synthetic outcome that leaves the batch's items unfinished (same
   shape as a transient skip — segments must stay resumable, not be
   marked failed).
2. The join side of the loop must keep translating file→signal: call
   `on_control_boundary(signal)` after every `join_next` completion
   (and in the paused/parked wait), so a pause written mid-round is
   noticed once in-flight batches land, the store flips to `paused`,
   and JobPaused is emitted.
3. Strict-context interaction: the pre-permit pause gate must sit
   AFTER the strict-context await (a paused prerequisite batch must
   not deadlock a waiter — re-check the stranding comment's scenario
   under pause).
4. Regression test: mock-provider e2e WITH batching enabled — pre-arm
   `pause` before launch, assert no RequestStarted after the first
   completion boundary, status `paused`, then resume completes; same
   for stop→resume. This is the coverage gap that let the bug ship.
