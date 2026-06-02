# Pre-merge review checklist

A reusable template for reviewing a milestone's PR stack before merging
to `main`. Each new milestone (v1.4, v1.5, …) gets its own consolidated
review pass against this checklist.

The goal is a single artifact — a written review that gates the merge,
not a chat. Style/consistency review is intentionally out of scope; this
checklist covers correctness, security, and performance only. Verify the
PRs already mirror the patterns of the milestone they're patterned on
before relying on this gate.

## Review path

One combined `general-purpose` subagent invocation. The agent inspects
all branches in the stack, applies the checklist below, and produces a
single consolidated report with a per-PR ship/block verdict and an
overall verdict.

Before launching the agent, gather:

- The list of branches in the stack, in merge order, with each branch's
  parent (so the agent can compute the per-PR diff range).
- The acceptance criteria for the milestone (typically in
  `docs/ROADMAP.md`).
- Any new architectural patterns the milestone introduces that the
  agent should validate against the existing invariants in `CLAUDE.md`.

The agent should `git diff <parent>..<branch>` per PR and read modified
files in their final form (`git show <branch>:<path>`) — not just the
diff, because load-bearing logic (the completion fence, the cache
namespace, dispatchers, etc.) only makes sense as a whole.

## The agent prompt template

Use the `general-purpose` subagent type. Capture the agent's output as
a markdown report (suggested path: `/tmp/<milestone>-review.md`).
Suggested prompt:

> You are reviewing the **\<milestone\>** milestone of BookForge, a
> Rust EPUB translator. **\<N\>** stacked PRs (#X–#Y) implement
> **\<list deliverables\>**. All PRs are stacked on `main`; their
> parents are listed in the branch table the user will paste.
>
> Branches to review (in order):
>
> 1. \<branch\> (PR #X): \<one-line scope summary\>
> 2. \<branch\> (PR #X+1): \<…\>
> 3. \<…\>
>
> For each PR, run `git diff <parent>..<branch>` to see the change,
> then read the full final-state files for context. Apply the checklist
> below. Produce ONE consolidated markdown report with three sections
> (Correctness/Invariants, Security/Privacy, Performance) per PR and a
> Cross-PR section. End with a verdict line per PR (`ship` /
> `block: <reason>` / `needs-changes: <list>`) and an overall verdict.
>
> Constraint: read-only. Do not edit, commit, push, or run cargo
> commands. Cite file paths and line numbers for every claim.

## The checklist

### Correctness / invariants

`CLAUDE.md` §1 architectural invariants — verify none are violated:

1. **The program owns EPUB structure.** No PR should add raw-XHTML
   handling to model-facing code paths.
2. **Reassembly is deterministic, pure code.** No fill-LLM, no
   secondary structure model.
3. **The cache is content-addressable.** New prompt blocks that opt in
   to the cache namespace must use distinct domain separators and must
   be byte-identical to a pre-milestone hash when the feature is not in
   use. Verify in `crates/bookforge-core/src/segment.rs`.
4. **CLI flags follow semver for v1.** All new flags additive; existing
   flags unchanged. Verify `crates/bookforge-cli/src/commands/translate/args.rs`.
5. **Single-binary distribution.** No new dependency that needs a JVM,
   RocksDB, embedded JS, etc. `Cargo.toml` deltas should be minimal.

Milestone-specific acceptance — confirm the testable acceptance items in
the milestone's ROADMAP section are mechanically met by the test suite.
Note any human-in-the-loop acceptance items the maintainer owns.

Completion-fence / scheduler specifics (if the milestone changes
scheduler behaviour):

- Any new `await` site in `crates/bookforge-llm/src/scheduler.rs` or
  `batch.rs` must be paired with a publisher that fires on every
  terminal status (success, failure, needs-review, schedule-canceled,
  batch-failure). Walk every Err branch.
- Slice / index math handles edge cases: window=0, idx=0, idx>=len.
- Pre-population paths cover: cache hits (`commands/translate/mod.rs`),
  resumed jobs (`commands/resume.rs`), and in-flight publish (the spawn
  closure in `scheduler.rs` and any per-segment buffer in `batch.rs`).
- Notify-style waits use the `notified()`-before-`lock()` pattern
  (register the future before checking the predicate; otherwise a
  publisher firing between check and await is lost).

Snapshot compatibility:

- Every new `RunConfigSnapshot` field must carry `#[serde(default)]` or
  a `default_*` fn. Pre-milestone snapshots must deserialize cleanly.
  Verify `crates/bookforge-core/src/run_snapshot.rs`.
- Every new `TranslationRunConfig` field is populated at every
  construction site. Verify with `grep "TranslationRunConfig {" -rn
  crates/`.

Batch / scheduler invariants (if the milestone touches `batch.rs`):

- `build_translation_batches` and `group_batches` — verify any
  partition invariant (e.g. no batch crosses a section boundary) is
  enforced under degenerate input.
- `split_batch_with_config` and `repack_batch_with_config` propagate
  any new batch metadata.
- Repair batches use a sentinel value for any field they don't
  participate in.

### Security / privacy

- API keys: `--api-key-env` is still the only key intake path. No new
  code reads env vars without justification. No key value is logged or
  persisted to the snapshot.
- Snapshot contents: any new user-authored text fields (prompt blocks,
  rendered glossary/style/entity output) don't leak to remote services
  beyond the LLM prompt itself. Verify
  `crates/bookforge-llm/src/telemetry.rs` doesn't reference them.
- TOML / file loaders: schema_version mismatches rejected. Scope IDs
  validated for series/book scopes.
- Migrations: no `DROP TABLE`, no destructive `ALTER`. New tables use
  `CREATE TABLE IF NOT EXISTS` with `UNIQUE` constraints.
- Review HTML and glossary candidates contain private user data; verify
  no new leakage paths.

### Performance

- Concurrency: any new `Semaphore` / `Notify` interaction must guarantee
  no scenario where waiters fill the pool while no publisher can ever
  fire. Cache pre-population and fence-unblock-on-failure paths together
  prevent the worst case — confirm both are wired into every entry
  point.
- Permit / lock holding across awaits: flag any case where a
  rate-limited resource (a permit, a connection slot) is held across an
  unbounded wait (e.g. an in-section context await).
- Cache fingerprint: a non-opt-in run must produce a `cache_namespace`
  byte-identical to a pre-milestone build. Verify by reading the
  cache-namespace tests in `crates/bookforge-core/src/segment.rs`.
- Allocation hot spots: flag new per-segment or per-batch allocation
  loops that scale super-linearly with input size.
- Smoke bench: `scripts/bench-mock.sh` should still complete within the
  normal range. Note any meaningful delta.

### Cross-PR consistency

- Architectural model: identify the shared mental model the stack
  embodies (e.g. "prompt blocks are precomputed off the critical path,
  fingerprinted, and opt-in to the cache namespace"). Verify the model
  holds across all PRs in the stack with no asymmetric divergences.
- Public types that grow new fields: every construction site updated;
  no literal still lives with a missing field.
- Modules that mirror an existing pattern (e.g. v1.2's
  `commands/glossary.rs` was the precedent for v1.3's
  `commands/{style,entity}.rs`): spot-check divergences against the
  precedent. Note any divergence that's a bug rather than a deliberate
  simplification.

## Critical files to inspect

These are the load-bearing surfaces that almost every milestone touches.
For each milestone, augment with the files specific to its scope.

- `crates/bookforge-llm/src/scheduler.rs` — the translation entry
  point, the per-segment spawn closure, prompt rendering.
- `crates/bookforge-llm/src/batch.rs` — `build_translation_batches`,
  `group_batches`, `translate_one_batch`, `translate_batches_with_callback`,
  failure-unblock helpers, split/repack.
- `crates/bookforge-core/src/segment.rs` — `compute_cache_namespace` /
  `compute_cache_namespace_inner` and the domain separators.
- `crates/bookforge-core/src/run_snapshot.rs` — `RunConfigSnapshot`
  fields, `#[serde(default)]` coverage.
- `crates/bookforge-store/src/db.rs` — `migrate()`, every new
  `upsert_*` / `load_active_*` / `list_*` / `clear_*_scope` method.
- `crates/bookforge-store/migrations/*.sql` — schema artifacts;
  confirm they match the inline DDL in `migrate()`.
- `crates/bookforge-cli/src/commands/translate/mod.rs` — every
  `TranslationRunConfig` literal, every `prepare_*_run_config` helper,
  the cache-namespace call site.
- `crates/bookforge-cli/src/commands/translate/snapshot.rs` —
  `persist_snapshot` field coverage.
- `crates/bookforge-cli/src/commands/resume.rs` — rehydration of every
  snapshot field into runtime config, the cache-namespace recompute.
- `crates/bookforge-cli/src/commands/<new-subcommand>.rs` — any new
  subcommand TOML parsers and command surface.
- `crates/bookforge-cli/src/main.rs` — command variant registration,
  `tracing` writer.

## Verification (post-review)

After the agent's report comes back:

1. Read the report. Per-PR verdict: `ship`, `needs-changes` (list each
   change), or `block` (with reason).
2. For each `needs-changes` item: decide if it's a follow-up issue
   (file a tracking issue, merge as-is) or a pre-merge fix (patch the
   PR branch).
3. For any `block`: address before merging.
4. Run any human-in-the-loop acceptance checks that the milestone's
   ROADMAP section calls out. These are typically subjective and the
   only checks that can't be automated.
5. Merge order: parent → child, depth-first along the stack. Each
   parent's landing flips its child's base to `main`. Note that
   GitHub will auto-close a PR if its base branch is deleted by the
   parent merge's `--delete-branch` — retarget remaining PRs to `main`
   *before* the next merge if you want to avoid the cascade.

## Optional: smoke-translate against a real provider

For milestones that change generation behaviour (context, style,
entities, etc.), run one real-API translation against the maintainer's
test fixture (`test/test.epub`) before declaring the milestone done.
This catches behavioural regressions the mock provider can't.

The smoke-translate run should:

- Exercise every new prompt block at the same time.
- Use a representative target language for the milestone's acceptance
  criteria (e.g. Italian for pronoun/gender concord work).
- Persist the run's `events.jsonl` and the SQLite snapshot for
  inspection.
- Sample 6–12 concord-relevant sentences from the output and
  verify against the milestone's acceptance criteria.

## Out of scope for this review

- Style / consistency — assumed already aligned with the precedent
  patterns in the codebase.
- Cross-crate API ergonomics — accept the existing patterns.
- ROADMAP changes beyond §2 status-table updates.
- Real-book translation *quality* assessment — that's a maintainer
  task done after the structural review passes.
