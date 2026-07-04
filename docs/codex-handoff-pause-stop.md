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
