# Handoff to Codex — on-the-fly settings reconfiguration + truncation fail-alert (2026-07-06)

Two related tasks, both surfaced by a real run (Italian → Toki Pona) that hit
repeated output truncation and could not be re-tuned without abandoning
progress. **The authoritative specs are `docs/ROADMAP.md` §10.1.3 and §10.1.4 —
read them in full before writing code.** If a detail is missing or a spec looks
self-contradictory, stop and ask the maintainer rather than inventing it (you
have correctly flagged spec contradictions before — keep doing that).

## Non-negotiables (ROADMAP §1)

- Additive only: new JSONL events are new variants / optional fields (§1.5);
  SQLite changes are forward-only migrations (§11.2). Prefer a sidecar file
  over a schema change where noted.
- No new dependencies; single static binary (§1.6). Reuse quick-xml/zip/rusqlite
  and the existing control-file + snapshot machinery.
- Never invalidate the cache or re-translate already-succeeded segments.
- Don't touch reassembly/prompt/marker logic.
- Conventional commits, logical units, full workspace gates before EACH commit:
  `cargo fmt --all --check`, `cargo clippy --all-targets --all-features -- -A clippy::too_many_arguments -D warnings`, `cargo test --workspace --locked`.
- Do NOT push. Leave the branch ready for the maintainer's review + live verify.

## Task 1 — `bookforge reconfigure <job-id>` (ROADMAP §10.1.3)

Let a **paused** job's cache-safe settings be amended so `resume` uses the new
values, without a fresh run.

- **New CLI command** `crates/bookforge-cli/src/commands/reconfigure.rs`, wired
  into `main.rs` (Subcommand enum + dispatch) and `commands/mod.rs` alongside
  `resume`/`status`. Flags = the cache-safe knobs only: `--batch-max-output-tokens`,
  `--batch-max-items`, `--batch-target-tokens`, `--concurrency`, `--qa`,
  `--double-check`, `--validate-output`, `--provider-max-attempts`, adaptive
  toggles. (Match the exact flag names/types used by `translate`.)
- **Persistence: sidecar, not migration.** Write
  `.bookforge/runs/<job-id>/overrides.json` next to `control` (see
  `bookforge-core` `run_dir_for_job` / `control.rs`). On resume, merge overrides
  over the loaded `RunConfigSnapshot` (`bookforge-core/src/run_snapshot.rs`;
  loaded via `store.load_job_config_snapshot`, consumed in
  `commands/resume.rs`). Merge = override-if-present, else snapshot value.
- **Guardrails.** Reject cache-affecting settings (provider, model, source/target
  language, profile, context window/scope, prompt version, glossary/style/entity
  inputs) with a clear message that a fresh run is required and why. These must
  never be silently accepted.
- **Surface it in `status`** so overrides are auditable. Dashboard is out of
  scope here (later, per §10.1.2/§10.1.3).
- **Tests:** reconfigure a paused mock job's batch budget + max-items → resume →
  remaining batches use new values, succeeded segments untouched (assert no
  re-translation / identical checkpoint); rejecting an immutable flag errors
  clearly; a job with no overrides resumes byte-identically to today.

## Task 2 — truncation handling + fail-fast alert (ROADMAP §10.1.4)

In the batch scheduler (`crates/bookforge-llm/src/batch.rs`). Today the
`Err(LlmError::InvalidResponse(_))` branch (~line 1893) splits on BOTH truncation
and malformed JSON; splitting shrinks the per-batch output budget
(`capped_batch_max_output_tokens` ~2508 / `batch_max_output_tokens` ~2473 are
proportional to item count), so truncation-driven splits spiral.

- **Distinguish** `RequestStatus::Truncated` from invalid JSON (the status is
  already known at the branch).
- **Escalate-then-split:** on truncation, first retry the *same* batch once with
  an escalated `max_output_tokens` (bounded multiple of the last budget, capped
  at the model/context limit) before falling back to the existing split path.
- **Fail-fast alert:** track truncation rate; when systemic (e.g. N consecutive
  batches or >X% still truncating after escalation), emit a prominent **additive**
  alert (new `ProgressEvent` variant or a distinct `Warning` kind) surfaced in
  CLI/`watch`/dashboard with an actionable message (raise
  `--batch-max-output-tokens`, lower `--batch-max-items`, or change model).
  Optionally auto-park instead of burning tokens — propose before implementing.
- **Tests (mock):** a single over-budget item succeeds after escalation instead
  of failing; a forced systemic-truncation scenario emits the alert within a
  bounded number of failures rather than spiraling; normal runs are byte-stable
  and emit no alert.

## Working agreement

- Branch: create `feat/live-reconfig` off latest `main` (v2.3.0 is released).
- Task 1 and Task 2 are independent — separate commits (or separate PRs) are fine.
- The un-mockable acceptance (maintainer's job, not yours): a real provider run
  that (1) reconfigures a paused job's output budget and resumes to completion,
  and (2) triggers the systemic-truncation alert on a hard target.
