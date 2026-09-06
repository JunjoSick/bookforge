# Migrations — documentation of record (STORE-5)

## Truth model: one executable source

The **procedural migrator** in `src/db/schema.rs` (`JobStore::migrate`) is the
only thing that executes at runtime. It builds the schema idempotently on every
open, with a few explicitly gated data migrations recorded in the `_migrations`
ledger.

The `NNNN_*.sql` files in this directory are **documentation**: they narrate the
schema history as it shipped. They are never executed by the library. Keeping a
second *executable* copy of the same schema was rejected after the two truths
already drifted (ledger names vs filenames, and `segments.cache_namespace`
placement), so the .sql files were demoted to docs instead of being promoted to
runtime.

A parity unit test (`src/db/migrations_docs.rs`) is the guard that keeps the
docs an exact mirror of what the procedural path actually builds:

- every documented table must exist live with exactly the documented column set
  (order-independent; additive `ADD COLUMN`s legitimately change placement),
- every documented index must exist live,
- every live user table must be documented somewhere in these files,
- the applied `_migrations` ledger must match these files 1:1 under the alias
  table below.

## Ledger naming: canonical mapping going forward

Applied rows are **never renamed in place** — existing databases already carry
the historical strings, and rewriting them would break backward compatibility
for no functional gain. Where a doc-file stem differs from the name the runtime
records, the canonical pair is:

| Version | Runtime-recorded name        | Doc file                                    |
|--------:|------------------------------|---------------------------------------------|
| 1       | `initial`                    | `0001_initial.sql`                          |
| 2       | `v1_0_1_input_snapshot`      | `0002_v1_0_1_input_snapshot.sql`            |
| 3       | `v1_1_segment_flags` *(legacy alias)* | `0003_v1_1_token_usage_and_flags.sql` |
| 4       | `v1_2_glossary_terms`        | `0004_v1_2_glossary_terms.sql`              |
| 5       | `v1_2_1_nullable_glossary_candidate_targets` | `0005_v1_2_1_nullable_glossary_candidate_targets.sql` |
| 6       | `v1_3_context_styles_entities` | `0006_v1_3_context_styles_entities.sql`   |
| 7       | `v2_4_human_corrections`     | `0007_v2_4_human_corrections.sql`           |
| 8       | `v2_7_qa_findings`           | `0008_v2_7_qa_findings.sql`                 |
| 9       | `v2_7_1_global_scope_unique_indexes` | `0009_v2_7_1_global_scope_unique_indexes.sql` |
| 10      | `v2_8_status_check_constraints` *(gated, see below)* | documented in this README |
| 11      | `v3_0_qa_finding_block_attribution` *(gated, see below)* | `0011_v3_0_qa_finding_block_attribution.sql` |

Version 3 is the one historical drift: files document what the ledger has
always called `v1_1_segment_flags` under its more descriptive stem. New
migrations pick one canonical name at birth and register any future drift in
`LEGACY_ALIASES` (see `src/db/migrations_docs.rs`) rather than renaming rows.

## Documented-but-runtime-only deltas not owned by a numbered file

- `idx_segments_cache_lookup` on `segments(source_hash, cache_namespace,
  prompt_version, provider, model, status)` — created procedurally with the
  cache-attribution remediation; no .sql file carries it because it postdates
  the file-based history.
- Migration 10 (`v2_8_status_check_constraints`, gated): rebuilds `jobs` and
  `segments` once to attach CHECK constraints to their TEXT `status` columns,
  enforcing the STORE-12 canonical vocabularies:
  - `jobs.status ∈ {running, paused, stopped, interrupted, succeeded, failed,
    needs_review, retry_pending}`
  - `segments.status ∈ {queued, succeeded, failed, retry_pending, needs_review,
    skipped_cached}`
  Databases containing values outside those sets are deliberately left
  un-hardened (plain TEXT) and warn on open until corrected, so pre-existing
  rows are always tolerated and never silently rewritten.
- Migration 11 (`v3_0_qa_finding_block_attribution`, gated): adds the nullable
  `qa_findings.block_id` column so findings can be pinned to a single
  translated block (audit remediation — findings used to lose block
  attribution when the CLI re-parsed engine error strings). Plain
  `ensure_column`-style `ADD COLUMN`, no table rebuild: legacy rows read back
  NULL and keep the segment-level meaning. `severity` stays plain TEXT; only
  `'error'`/`'warning'` may be persisted, enforced in Rust at the single
  findings insert choke point (`db::findings::insert_qa_finding_row`).
