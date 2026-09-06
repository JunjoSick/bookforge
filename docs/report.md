# BookForge Deep Audit Report

> **Status (2026-08-31).** This report is the **historical audit record** for
> the 2026-08 deep audit (state as of v2.6.1, audit date 2026-08-25). The
> remediation described below is **NOT complete and NOT released**: the
> campaign branch `remediation/audit-2026-08` at commit `aa90d94` is not merged
> to `main` (PR #112 open/blocked; PR #108 folded in), there is **no v3.0.0
> tag/release**, and the latest published release is **v2.6.1**. Wave history
> (0–4), the six-reviewer pre-merge pass, and dogfooding are release-candidate
> work on that branch. For authoritative per-item status, see
> **`docs/AUDIT-2026-08-31.md`**; entries there close only as Fixed, Removed, or
> Refuted with evidence and tests. The wave summaries below are historical
> record, not a claim that the campaign shipped.

**Date:** 2026-08-25 · **Version audited:** v2.6.1 (workspace @ `JunjoSick/bookforge`, 427 commits)
**Method:** 14 specialized audit agents dispatched in sequential waves over the full repo (~80k LOC Rust, 7 crates + CI/docs/scripts/artifacts). Later waves received earlier findings to verify or refute. Builds and tests were actually executed. ~250 raw findings were deduplicated into the consolidated items below.

> Finding IDs (`CORE-n`, `STORE-n`, …) are stable references used throughout. Where several agents found the same underlying issue, the item lists all IDs.

---

## 1. Executive summary

BookForge is in **unusually good shape for a project of this size**: zero clippy warnings, zero `cargo check` warnings, rustfmt-clean, no TODO/FIXME debt, no secrets in git history, a genuinely enforced security boundary around EPUB parsing, and documentation whose claims mostly check out against code line-by-line. The architecture (models never touch markup; deterministic Rust owns structure) is sound and consistently implemented.

The audit still found **real problems concentrated in five clusters**:

1. **Two headline guarantees have cracks.** "Human corrections are protected from overwrites" is a check-then-write race across processes ([H-1](#h-1)); "bounded EPUB decompression" is bypassed entirely by two first-class CLI commands (`reflow`, `validate`) ([H-2](#h-2)).
2. **One malformed model response can kill an entire run.** A phantom `"unknown"` segment fails an FK check inside the single checkpoint writer, which fail-fast aborts the run and marks every pending segment failed ([H-3](#h-3)). Related: resume can report success while segments are still failed ([H-4](#h-4)).
3. **The serve dashboard's local-only trust model is thinner than documented.** No auth on the loopback API means any local process can spend remembered provider keys and read book content; the `.bookforge` privacy claim is defeated by serve's own directory probes and upload paths ([H-5](#h-5), [H-6](#h-6)).
4. **A permanent background tax runs on every translation:** the pause/stop watcher reopens and fully migrates the SQLite store ~10×/second for the life of every job ([H-7](#h-7)).
5. **Supply-chain hardening opportunities:** three plaintext API keys live in `tests/` (untracked by intent, history verified clean — but worth an explicit ignore rule so intent is encoded, not positional), and CI pins actions inconsistently (release workflow SHA-pinned; ci/security floating tags).

Beyond those: pervasive duplication (8 divergent token-estimation formulas, 2 EPUB emitters, 3 pricing loaders), a systematic CJK blind spot shared by token estimation, PDF reconstruction, and audiobook chunking, feature asymmetry between CLI/dashboard/crates, and a long tail of dead code (~30 verified-unused items).

**Suggested order of attack:** H-8 (rotate keys, 10 min) → H-7/H-3/H-4 (small fixes, biggest reliability win) → H-1/H-2/H-5 (guarantee repairs) → easy-wins list → investments.

---

## 2. Top findings — priority matrix

| # | ID(s) | Title | Sev | Effort |
|---|---|---|---|---|
| H-1 | STORE-1, DOC-11 | Human-correction protection is check-then-write (TOCTOU), not SQL-enforced | high | small |
| H-2 | EPUB-1/2, CLI-6, DOC-1 | `reflow` + `validate` do unbounded zip reads — bounded-decompression claim broken | high | small |
| H-3 | LLM-2, CLI-2, DEAD-6 | Phantom `"unknown"` segment kills checkpoint writer → whole-run abort | high | small |
| H-4 | CLI-1 | Resume marks jobs `succeeded` even when segments remain failed | high | small |
| H-5 | SERVE-1 | Unauthenticated loopback API: any local process can spend remembered keys / read books | med-high | medium |
| H-6 | SERVE-2 | serve pre-creates `.bookforge` world-readable; uploads 0644 — defeats "private data" claim | med-high | small |
| H-7 | STORE-2, CLI-9 | Control watcher reopens + fully migrates SQLite ~10×/sec for entire run | high | trivial–small |
| H-8 | INFRA-1 | Plaintext API keys in `tests/*.txt` (untracked by intent; history verified clean) — harden with explicit ignore rule + optional relocation | medium | trivial |

---

## 3. Critical & high findings (detail)

### H-1 · Correction freeze is not atomic
`bookforge-store/src/db/translations.rs:49` checks `translation_is_human_corrected()` with one SELECT, then unconditionally writes with `INSERT OR REPLACE` at :55–69 (same pattern :112/:119–132, :170/:177–190). `INSERT OR REPLACE` deletes-and-reinserts, so a losing race also wipes `origin='manual'`, `human_corrected=1`, `corrected_at`. The dashboard (`serve`) and CLI worker are separate processes sharing one SQLite file, and `--force` resume explicitly permits double-running. Fix: make the model-write path a single conditional statement/transaction (`... WHERE NOT EXISTS (... human_corrected=1)` or take the write txn `IMMEDIATE` before checking). Docs (README:48) state an absolute guarantee — soften or fix code. **Effort: small.**

### H-2 · Bounded-decompression claim has two holes
`ArchiveReadBudget` is genuinely excellent where it's used (reader/writer; lying-central-directory defense tested). But:
- `epub/src/reflow.rs:148–164` uses bare `by_index` + `read_to_end`; `validate.rs:55–96` same. Both are exposed directly on untrusted input via `bookforge reflow` / `bookforge validate` — a lying-small zip entry OOMs the process.
- Fix is mechanical: route both through the existing budget API (writer.rs:121,185 pattern). Also case-insensitive extension matching in validate.
README:74 and CHANGELOG claim this broadly; docs agent confirms overclaim. **Effort: small.**

### H-3 · One duplicate batch-item ID aborts the whole run
Chain (verified end-to-end): model echoes one item ID twice → `BatchItemFailure{segment_id:"unknown"}` (`llm/batch/rendering.rs:366–377`) → aggregated as NeedsReview entry (only the *repair* list filters `"unknown"`, execution.rs:1244 vs :1693) → CLI forwards to checkpoint writer → `save_needs_review` INSERT violates FK → writer task dies via `?` (`cli/checkpoint.rs:133`) → every later checkpoint send fails → run aborts, `mark_unfinished_segments_failed` fails everything pending. One glitchy response = total loss of a long paid run. Fix: filter unknown-segment entries at aggregation (LLM crate) *and* make the writer log-and-continue per command instead of fail-fast. Adjacent: `CheckpointCommand::MarkFailed` exists only for tests while production failure-marking bypasses the channel (DEAD-6) — unify. **Effort: small.**

### H-4 · Resume can green-light a failed book
`resume.rs:461–507` discards the engine's returned translations and rebuilds purely from stored block rows (:514–517); segments that failed again during resume have status rows but no blocks (:1167 silently omits blockless), so `mark_job_finished` sees nothing Failed → job = `succeeded`, report green, output EPUB contains raw source text as placeholders for failed segments. Fix: include DB status summary in completion decision, or emit status-only entries for blockless terminal segments. Add a targeted integration test (the trigger is narrow but reachable). **Effort: small.**

### H-5 · Dashboard has zero authentication by design — but the design assumes only browsers
Loopback bind + Host allowlist + CSRF tokens defend against web pages, not against other local processes. Any local user/process can `GET /`, harvest the CSRF token embedded in HTML (`serve/security.rs:56–68`), then launch billable translations using the session's remembered provider key (key omission falls back to remembered key, `translation.rs:277–279`) and read full source+translation text via `/api/jobs/{id}/review`. On multi-user machines this is real; on single-user machines it's still a privilege boundary the README doesn't state. Fix options: console-displayed session token required on all routes; or refuse remembered-key reuse unless the key was supplied in-process. **Effort: medium.**

### H-6 · "Private .bookforge on Unix" defeated by serve's own flows
`serve.rs:269–282` creates the `.bookforge` root with plain `create_dir_all` (umask, typically 0755) *before* anything applies 0700 — and the store only tightens newly-created components, never pre-existing ones. `serve-uploads` dirs and uploaded EPUBs are written 0644 (`translation.rs:79`, `audio.rs:413`). Result: another local user can read `jobs.sqlite` (full book text) and uploads. Translate-path snapshots do this correctly (0700/0600, `translate/snapshot.rs`) — reuse `create_private_dir_all` semantics + chmod-if-existing at the serve entry points. **Effort: small.**

### H-7 · Watcher churn tax
`control.rs:24` polls every 100 ms; each tick calls `JobStore::open()` (:490) which runs the full migration pass (9 CREATE TABLE IF NOT EXISTS, 15 column probes, 7 unconditional write transactions) — ~10 connection opens + write-lock acquisitions per second contending with the checkpoint writer, for the entire life of every run. Root cause: `RefCell<Connection>` makes `JobStore !Send`, so the async watcher can't hold one. Fix: dedicated watch connection owned by the watcher (reopen-on-error), plus gate `record_migration` behind `migration_applied()` like migration 8 already does. **Effort: small.**

### H-8 · Keys in the tree (untracked, but fragile)
`tests/DEEPSEEK_KEY.txt`, `tests/ELEVENLABS_KEY.txt`, `tests/OPENROUTER_KEY.txt` contain live-shaped keys (35–73 chars, correct prefixes). They're ignored today solely because `.gitignore:20` is `/tests/*` — positional, not intentional. Git history is clean (`git log --all -S` × 8 patterns, zero hits). Actions: move out of the tree, rotate all three as precaution, add explicit `/tests/*KEY*` ignore so intent matches accident. **Effort: trivial.**

---

## 4. Findings by area

Severity: 🔴 critical/high · 🟠 medium · 🟡 low · ⚪ info/trivial. Effort: T=trivial (<30min), S=small (<½d), M=medium (1–2d), L=large.

### 4.1 bookforge-core (14 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| CORE-4 → merged into [LLM-1](#llm--batching--providers-20-findings) | 🟠 | Output-token floors override context/user caps | S |
| CORE-2 | 🟡 | Oversized single blocks produce over-limit segments; `max_tokens` constraint field never read anywhere | S |
| CORE-3 | 🟠 | Glossary selection O(segments×terms×window) with full-text lowercase allocs — and computed twice per translate (mod.rs:294 + orchestration.rs:353) | S |
| CORE-5 | 🟡 | Static context crosses chapter boundaries (contradicts Chapter-scoped sliding context default); `context_tokens` counts words; wholesale clone | S |
| CORE-8 | 🟡 | Equal-priority glossary/entity duplicates resolve by unspecified row order → nondeterministic prompts/cache keys | S |
| CORE-9 | 🟡 | Entity.notes fingerprinted (cache-busting) but never rendered into prompts | T |
| CORE-12 | 🟡 | Candidate extraction misses ALL-CAPS names; naive italic close-tag matching mispairs nested markers | S |
| CORE-10/11 | 🟡 | ~200-line snapshot mirror of config structs; resolve() = 330 lines × 6 near-identical literals — every new knob touched in 4+ places | S |
| CORE-13 | ⚪ | Cache-namespace hash missing `\|glossary\|` domain separator (style/entities have them) | T |
| CORE-1/6/7/14 | ⚪ | Dead: marker helpers (one subtly wrong), Xml/Zip error variants (+2 deps), ProviderErrorKind, ModelRouteConfig, PromptVersion V1/BatchV1/BatchV2, SpineItem.linear, render_and_fingerprint; re-export gaps | T |

**Strengths:** marker parsing traced panic-free against adversarial inputs; validation contract strict at both enforcement points; cache-invalidation discipline unusually careful; RunState fold design clean and tested.

### 4.2 bookforge-store (18 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| STORE-1 → [H-1](#h-1--correction-freeze-is-not-atomic) | 🔴 | TOCTOU correction freeze | S |
| STORE-2 → [H-7](#h-7--watcher-churn-tax) | 🔴 | 10Hz store reopen/migrate from watcher | S |
| STORE-3 | 🟠 | Per-segment checkpoint = 3–5 separate autocommit txns (crash window; WAL churn) | S |
| STORE-4 | 🟠 | Glossary table rebuild + legacy rename cascade non-transactional → crash mid-rename orphans data, next open silently recreates empty | T |
| STORE-5 | 🟠 | migrations/*.sql never executed at runtime — dual source of truth already drifted (v3 name, cache_namespace column placement) | M |
| STORE-6 → [CLI-8](#commands-layer-18-findings) | 🟠 | No cross-process exclusion for plain resume | M |
| STORE-11 | 🟡 | Resume re-insert leaves stale provider/model/source_hash columns after config changes → future cache filtering misattributes | S |
| STORE-12 | 🟡 | Job/segment statuses are unchecked magic strings (14 states, stringly compared) | M |
| STORE-13 | 🟡 | NULL scope_id defeats UNIQUE constraints on global rows → concurrent first-inserts duplicate globals | S |
| STORE-16 | 🟡 | Missing index on jobs.created_at (dashboard/watch sorts whole table per refresh) | T |
| STORE-17 | 🟡 | file_hash reads whole EPUB to RAM; no retention/prune path for store growth | S |
| STORE-14/15/18 | ⚪ | upsert_entities non-transactional; add_glossary_term id re-select race; lossy paths; InvalidCorrection doubles as generic rejection | T/S |
| STORE-7/8/9/10 | ⚪ | synchronous=NORMAL durability note; N+1 prepares; find_cached_translation + mark_segment_failed_if_unfinished dead; unused dep `toml` (+ `serde` found later — see deps section) | T |

**Strengths:** pragmas exactly right for multi-process use and test-enforced; zero SQL-injection surface (verified every format! site); cache-key filter set thorough with regression tests; best-effort findings isolation deliberate; exceptional storage test suite incl. two-store concurrency smoke.

### 4.3 bookforge-epub (18 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| EPUB-1/2 → [H-2](#h-2--bounded-decompression-claim-has-two-holes) | 🔴 | reflow + validate unbounded reads | S |
| EPUB-3 | 🔴 | `<script>/<style>/<svg>/<math>` inside an active paragraph absorbed as translatable inline markers — suppression intent defeated in the common nesting; MathML flattened, JS "translated" | M |
| EPUB-11 | 🟠 | Duplicated helpers with *divergent* implementations (11 helpers; reader-vs-writer path normalization disagree on Windows separators/`..` — load-bearing for patch matching) | S |
| EPUB-4 | 🟠 | pdf_cleanup deletes legitimate numeric headings ("1984") when pdftohtml meta detected | S |
| EPUB-18 | 🟠 | Test gaps: no hostile-input corpus, no EPUB2 fixture, no math/svg regression, no property test for the reader↔writer index protocol | L |
| EPUB-7 | 🟡 | AppendBlock can put `<p>` inside `<hgroup>` → EPUBCheck RSC-005 regression risk | S |
| EPUB-5/6/9/13 | 🟡 | Replace-mode clobbers all dc:language tags; duplicate manifest ids last-wins silent; spine_index collisions across synthetic sections; double-escaping of entity-like LLM output + silent drop of unknown entities | T/S |
| EPUB-10 | 🟡 | Unbounded recursion on deeply-nested markers from stored/resumed translations (stack-abort); quadratic needle rebuild | T |
| EPUB-8 | 🟡 | TableCell blocks unreachable (cells inline into row blocks) — granularity + dead writer arms | M |
| EPUB-12/14/15/16/17 | ⚪ | Mixed zip timestamps undercut determinism; inspect decompresses everything just to count; O(entries×resources) scans; reflow recompresses all; dead params | T/S |

**Strengths:** archive_limits enforced-not-decorative with a lie-detection test; zip-slip structurally impossible (in-memory, no extraction); billion-laughs non-issue via quick-xml GeneralRef; patch-don't-regenerate with byte-identical raw copies and validated patches; atomic staged commits with rollback; text_coverage honesty.

### 4.4 bookforge-llm — batching & providers (20 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| LLM-1 (+CORE-4, LLM-16) | 🔴 | `cap_output_tokens` floors override both the context remainder (→ guaranteed 400s on largest segments) and user caps <256; single-segment path skips the clamp entirely (mode-dependent behavior for same flag) | S |
| LLM-2 → [H-3](#h-3--one-duplicate-batch-item-id-aborts-the-whole-run) | 🔴 | phantom "unknown" segment | T |
| LLM-3 | 🟠 | Repair phase ignores pause/stop signals, limiter, rate controller (contrast workers) | S |
| LLM-4 | 🟠 | Batch-level transient retries have zero inter-round delay; worst case ≈18 immediate requests vs rate-limited endpoint; 408/425 treated permanent | S |
| LLM-5 (+DUP-1) | 🟠 | chars/4 estimate mis-sizes CJK ~4× (batches pack oversized → truncation churn); cost estimates exclude QA/double-check/repair passes | M |
| LLM-6 | 🟡 | JSON parsers accept neither markdown fences nor trailing prose → cheap strip would kill split/retry churn | T |
| LLM-7 (+DOC-10) | 🟡 | Mojibake `â€"` double-encoded em-dashes baked into 7 of 15 prompt templates — ships in every prompt | T |
| LLM-8 (+DRIFT-1) | 🟡 | Prompt-version identity maintained in 3 unlinked places; repair files named .v2.md registered as "v3"; stale internal headings | S |
| LLM-9 | 🟡 | DoubleCheckConfig.concurrency ignored (serialized latency); correction_rounds never read (>1 silently does nothing) | S |
| LLM-13 | 🟡 | `"v4-flash"` substring classifies DeepSeek default model as reasoning → ×3 output multiplier + ≥300s timeouts by default | T |
| LLM-15 | 🟡 | Prompt-injection surface: book text in unfenced `=== Context ===` blocks; malicious EPUB can steer tone (validation bounds structural damage) | S |
| LLM-10/11/12/14/19/20 | ⚪ | Deterministic-per-attempt jitter (herd sync); Retry-After HTTP-date form dropped, caps silently at 60s; telemetry records status_code None/retry_count 0 always (printed `429s=` dead); duplicated-marker contract contradicts validator; QA verdict strings unvalidated; ≤8KB provider bodies persist into segment errors | T/S |
| LLM-17/18 | ⚪ | Vestigial max_rounds outer loop; 3–4× cloning of book payloads per run | T/M |

**Strengths:** secrets hygiene solid (env-name indirection, bearer_auth, never logged); 4 MiB streamed-response cap with checked arithmetic; AdaptiveLimiter subtle and correctly done; validation genuinely adversarial (~100 batch tests); fence-deadlock avoidance in section partitioning.

### 4.5 bookforge-pdf (22 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| ~~PDF-1~~ | ❌ **REFUTED** | "Windows test compile break" — build/test agent compiled `cargo test -p bookforge-pdf` cleanly on Windows; ungated tests reference none of the cfg(unix) helpers | — |
| PDF-3 | 🟠 | Temp dirs leak when figure/media passes error (cleanup skipped on early `?`) | S |
| PDF-5 | 🟠 | Successful OCR wipes figure blocks anchored on that page | S |
| PDF-6 | 🟠 | Running-header removal pushes pages below 95% coverage threshold → spurious OCR spend / page rasterization | S |
| PDF-7 | 🟠 | No bidi/RTL handling — Arabic/Hebrew lines scrambled left-to-right, coverage metric blind to it | M |
| PDF-10 | 🟠 | Caption detection English-only ("figure/table" prefixes) in a *translation* product; vector-figure recovery lost for foreign PDFs, no warning | S |
| PDF-2/TEST-2 | 🟠 | PDF integration coverage effectively Unix-only (37 cfg(unix) gates; shell-script stubs) while Windows is a primary target — env scrubbing/timeout/temp-dir claims never execute on Windows CI/dev | L |
| PDF-4 | 🟡 | pdftohtml runs without `-i` — extracts every image just to delete it (wastes the shared 120s timeout on scans) | T |
| PDF-8/9/12/13 | 🟡 | Hyphen repair fuses legitimate compounds ("well-known", German); CJK paragraphs never merge across pages; heading levels from unused fontspecs; vertical/rotated text silently dropped | S |
| PDF-14 | 🟠 | Synthetic EPUB = one monolithic XHTML + one-entry nav + constant UID/frozen timestamp for all conversions; detected headings unused for TOC; second parallel EPUB emitter (see DUP-5) | S/L |
| PDF-22 | 🟡 | OCR request body uncapped pre-encode; extreme MediaBox → huge render/base64 | S |
| PDF-11/15/16/18/19/20/21 | ⚪ | stderr over-limit flag computed-then-discarded; double file reads + fixed-key hash dedup; `[::1]` bracket mismatch breaks doc'd no-key exemption; stale per-page stats; line merging ×2; dead jpg arms; O(n²) cluster pairing (fine at scale) | T/S |

**Verified claims:** time-limited poppler ✔ (kill+reap, tested); scrubbed environments ✔ (allowlist, negative test); bounded OCR reads ✔ (8 MiB, tested); auth-header hygiene ✔ (redirects off, key never logged); shell-free subprocess ✔. Partial: private temp dirs (Windows ACL inheritance; leak paths PDF-3).

### 4.6 bookforge-audio (18 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| AUDIO-3 (+DOC-15) | 🟠 | ElevenLabs auto-model preflight fails open to *most expensive* tier (multilingual_v2 ≈ 2× credits) on transient network error; also poisons resume hashes | T |
| AUDIO-1 | 🟠 | Windows "atomic" write backup-rename dance built on a false premise (`fs::rename` does replace on Windows) — widens the durability window it claims to close; pid-less backup name collides across concurrent writers | T |
| AUDIO-2 | 🟠 | No cross-process lock on out_dir — concurrent builds corrupt manifest, prune deletes the other run's fresh paid chunks | S |
| AUDIO-4 | 🟠 | Sentence splitter: no CJK terminators (。！？), splits "Mr."/"e.g."; CJK prose cut mid-word arbitrarily (no spaces to fall back on) | S |
| AUDIO-5 | 🟠 | ffmpeg inherits stdin, no `-nostdin`, no timeout → can hang forever awaiting tty input (incl. dashboard child) | T |
| AUDIO-7 | 🟡 | Dashboard estimator skips reflow/PDF-grouping preprocessing → estimates diverge from actual run for PDF-derived books | S |
| AUDIO-6/8 (+ASYM-1) | 🟡 | Dashboard accepts seed for any provider (late child failure); cannot launch chapter subsets; prune/retry-failed/text-normalization/break-tags unreachable from UI; --text-normalization silently dropped for OpenAI/Gemini in CLI itself | S |
| AUDIO-11 | 🟡 | Crash debris (*.part.tmp, *.replace.bak, staged m4b parts) invisible to --prune, no sweep | S |
| AUDIO-12/13/14 | 🟡 | --loudnorm silently ignored for stitch-only runs; single-pass loudnorm + pre-encode durations drift chapter markers; gap support silently degrades on aac/flac | T/S/M |
| AUDIO-15/16/17/18 | ⚪ | Whole-book text duplicated across queued futures; full-read SHA256 per cached chunk; metadata calls ignore cancellation; minor message/padding nits | T/S |
| nav-audio residual | 🟡 | The old "unwanted navigation audio" bug class is defended (sec_nav_ prefix, nav property, furniture skip) — but a malformed EPUB3 whose nav lacks the property gets narrated wholesale; heuristic backstop recommended | S |

**Strengths:** cache-hash design thorough (length-prefixed fields, per-field tests); resume matrix well-tested incl. corrupt-cache regeneration and retry-failed economics; provider hardening above average (magic-byte validation, ApiKey redaction, HTTPS policy); ffmpeg degrades gracefully and refuses incomplete publishes.

### 4.7 Commands layer (18 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| CLI-2 → [H-3](#h-3--one-duplicate-batch-item-id-aborts-the-whole-run) | 🔴 | checkpoint writer fail-fast cascade | S |
| CLI-1 → [H-4](#h-4--resume-can-green-light-a-failed-book) | 🔴 | resume false-success | S |
| CLI-3 | 🟠 | Ctrl+C during `resume` does nothing (handler installed globally, token never passed) — users must kill the console | T |
| CLI-4 | 🟠 | Pause/Stop landing in the post-completion report window flips terminal status succeeded→stopped/paused | S |
| CLI-5 | 🟠 | Hard errors (rebuild failure, fallback misconfig) leave jobs stuck `running` forever — only doctor/dashboard hint at truth | S |
| CLI-6 → [H-2](#h-2--bounded-decompression-claim-has-two-holes) | 🟠 | reflow/validate unbounded reads exposure | S |
| CLI-7 | 🟡 | Launch-claim/override-lock stale reclaim TOCTOU (check-then-delete; rename-based claim would fix) | S |
| CLI-8 (+STORE-6) | 🟠 | Plain `resume`: zero liveness check — concurrent resumes both proceed, doubled LLM spend; lease machinery exists, dashboard-only | S |
| CLI-9 → [H-7](#h-7--watcher-churn-tax) | 🟠 | watcher store churn confirmed+extended | S/M |
| CLI-10 | 🟡 | Fallback candidate scan O(segments×translations) (~4×10⁸ comparisons on 20k-segment book) at finalize time | T |
| CLI-11 | 🟡 | QA reviews computed before fallback/double-check mutate text → reports show verdicts for superseded text | M |
| CLI-16 | 🟡 | translate/resume flag asymmetry (no --validate-output/--qa-*/double-check/fallback on resume); estimate ignores prompt overhead/QA/retries — systematically low | S |
| CLI-12/13/14/15/17/18 | ⚪ | Misleading --double-check-model warning enforcing nothing; benchmark hardcodes OpenRouter defaults while claiming deepseek; pause/stop accept typo'd job IDs happily; tail loads whole event log; doctor reports FAILED then exits 0; test-hook env var ungated in production | T |

**Lifecycle trace verdict:** start/pause/crash-resume/stop mechanics are solid (namespace-equality bail, durable snapshots, drained writers between stages). The breaks concentrate at resume completion (H-4, CLI-3) and the completion window (CLI-4, CLI-5). Correct-flow (`correct.rs`) and review HTML (XSS-clean, `textContent` everywhere, `\u003c` escaping) are exemplary.

### 4.8 TUI & CLI surface (31 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| UI-2 | 🔴(UX) | `resume --ui tui`: footer says "q/Ctrl-C abort & quit" — quitting neither aborts the run (token never passed) nor is labeled correctly; worker keeps spending headless | S |
| UI-5 | 🟠 | ANSI/control-char injection in non-TUI outputs: doctor prints 200 raw chars of LLM responses; EPUB titles/chapter names into bars/status verbatim. Crafted EPUB can rewrite terminal titles / play escape games on exactly the non-developer flows | S |
| UI-22 (+DOC-5) | 🔴(JSON) | Three ungated println!s pollute `--ui json` stdout whenever double-check/fallback enabled → parse failure mid-stream for automation audience | T |
| UI-21 (+DOC-3, CLI-17) | 🟠 | Exit codes undefined & surprising: Ctrl+C→0, stop→0, error→1, clap→2, doctor-failed→0; nothing documented | S |
| UI-23 (+DOC-6) | 🟠 | Two incompatible `--ui json` dialects (ProgressEvent objects vs bespoke audiobook envelopes), no version signal | M |
| UI-24 (+DOC-4) | 🟠 | Store is CWD-relative everywhere except serve which silently relocates to %LOCALAPPDATA% → CLI `status` can never see dashboard jobs launched from unwritable dirs; no BOOKFORGE_HOME override exists | M |
| UI-1 | 🟡 | ↑ from follow mode jumps to oldest log line instead of up-one (top-offset vs follow pinning bug) | S |
| UI-9/10 | 🟠 | RunState: ETA/rate poisoned across resume epochs (first_timestamp never reset); gauge vs stats mix time domains. DroppedEvents declared-but-never-emitted — burst losses vanish silently from replay logs (SQLite truth diverges from dashboards permanently) | M |
| UI-13 | 🟠 | Tri-state bools require explicit values on translate (`--adaptive-concurrency true`) but not on reconfigure — same knob, two syntaxes, confusing clap error | S |
| UI-28/30/31 | 🟡 | tail loads whole events.jsonl + hand-scanner drifts from RunState semantics (miscounts across epochs); four rendering presentations of one state already drifting | S/L |
| UI-6/8/11/12/14/15/16/17/18/19/20/25/26/27/29 | 🟡⚪ | Blank-line ring pollution from unhandled events; divergent ETA formatters; corrupt-JSONL lines silently skipped; JSONL-log open failure degrades to one stderr line; `--out` vs `--output`; no ArgGroups (combos fail late after reading inputs); clap env feature unused + hidden env vars; numeric validation one-off; reconfigure help-text bare where users most need cache-safety guidance; benchmark --concurrency parsed-printed-ignored; silent refresh clamps differing (20 vs 50 floor); destructive clear commands lack --yes; long silent phases before first feedback (audiobook planning, convert); legacy-console glyph fallback absent; per-draw allocation churn | T/S |
| Inventory | info | 24 subcommands, ≈250 options. Top inconsistencies: tri-state syntax split, naming split, hand-rolled provider args in qa/double-check/fallback families instead of flattening ProviderArgs, negative flags as SetTrue pairs | — |

**Strengths:** shared deterministic RunState layer with self-healing counters; boundedness everywhere (2048 queue, 500 ring, 256 KiB lines); lazy JSONL open preserving complete logs; thorough terminal hygiene incl. Drop impls and broken-pipe tolerance; ratatui path immune to control chars; rendering tests assert actual buffers.

### 4.9 Serve dashboard — security (10 security + quality findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| SERVE-1 → [H-5](#h-5--dashboard-has-zero-authentication-by-design--but-the-design-assumes-only-browsers) | 🟠 | unauth local API + remembered-key reuse | M |
| SERVE-2 → [H-6](#h-6--private-bookforge-on-unix-defeated-by-serves-own-flows) | 🟠 | privacy defeat via probe/uploads | S |
| SERVE-3 | 🟡 | Cancel reads PID from disk and OS-kills it without liveness/ownership check (PID reuse kills unrelated trees; contrast jobs' fresh-lease gating done right) | S |
| SERVE-4 | 🟡 | Job-id path params reach filesystem unsanitized on read paths (percent-decoded `../` folds arbitrary JSONL into API responses; audio ids validated, job ids not) | S |
| SERVE-5 | 🟡 | Estimate endpoints write uploaded EPUBs to shared temp dir with predictable names (symlink/pre-create games on multi-user hosts; leak on parser panic) | S |
| SERVE-6 | 🟡 | No cap on simultaneous dashboard launches sharing remembered keys | S |
| SERVE-7 | 🟡 | Audio path parses untrusted EPUBs *inside* the key-holding server process (translation path isolates in child — inconsistent trust split) | M |
| SERVE-8/9/10 | ⚪ | Keyed proxy GETs skip CSRF; hardened headers only on `/` + CSP unsafe-inline; full anyhow chains + absolute paths disclosed in errors (local-only, informational) | S |
| Quality | ⚪ | Every storage hiccup maps to 404/400; sync 64MB write on async thread; launch filename collisions (timestamp-only tags); orphaned uploads on spawn failure; provider-tier table duplicated JS/server; correction_locks registry grows unbounded | T/S |

**Claim verification:** keys-memory-only ✔ verified clean (name-only argv, scrubbed env, no readback endpoint, tests enforce); least-privilege ◐ (minimal exposure yes, quota/job key separation no); hardened headers ◐; private data ✗ under serve-first flow (H-6); loopback+rebinding defenses ✔ exemplary (Host allowlist w/ port pinning).

### 4.10 Infrastructure & supply chain (13 findings)
| ID | Sev | Finding | Effort |
|---|---|---|---|
| INFRA-1 → [H-8](#h-8--keys-in-the-tree-untracked-but-fragile) | 🔴 | plaintext keys in tests/ | T |
| INFRA-3 | 🟠 | ci.yml has no `permissions:` block (security.yml does it right) | T |
| INFRA-4 | 🟠 | Action-pinning inconsistency: release.yml SHA-pinned (cargo-dist), ci/security floating tags incl. `dtolnay/rust-toolchain@master` | S |
| INFRA-6 | 🟠 | Known cargo-dist residuals (contents:write inheritance, GH_TOKEN in non-publish jobs, curl\|sh bootstrap) — documented + accepted in release-pipeline-security.md; keep current | n/a |
| INFRA-9 | 🟠 | ~89GB artifacts: target/ 53GB, .claude/worktrees 31GB (nine abandoned clones), tmp/ 1GB; tests/ holds ~15 copyrighted/z-library-named EPUBs — legal foot-gun if folder ever shared/synced | S(manual) |
| INFRA-5 | 🟡 | EPUBCheck jar downloaded in CI twice, no checksum | S |
| INFRA-8 | 🟡 | .tools/ = 2.76GB vendored toolchain, only one digest recorded; java17 JRE appears orphaned (wrapper resolves jdk-21) | S |
| INFRA-2/10/11/12/13 | ⚪ | tracked __pycache__/.pyc; 787 litter dirs under crates/bookforge-cli/.bookforge/runs incl. 51 empty retry_pending_overrides_<pid> (never reaped — code fix flagged); gitignore gaps (__pycache__/, .agents/); release-pipeline-security.md checkout SHA stale vs v7; python3/shebang assumptions on Windows box | T/S |

**Secret scan verdict:** tracked files + full history clean (env-var names and placeholders only). Pricing JSON honest (units explicit, null+note where uncertain, dual copies hash-guarded by test). Corpus supply chain exemplary (SHA-pinned immutable assets, atomic downloads).

### 4.11 Documentation (18 findings)
| ID | Sev | Finding |
|---|---|---|
| DOC-1 → [H-2] | 🔴 | bounded-decompression overclaim |
| DOC-3 → [UI-21] | 🟠 | exit codes/Ctrl+C documented nowhere |
| DOC-4 → [UI-24] | 🟠 | store-location self-contradiction; relocated dashboard jobs invisible to CLI, wrong troubleshooting answer |
| DOC-2 | 🟠 | events.md missing 4 shipped variants (JobPaused/JobResumed/RuntimeConfigChanged/RuntimeConfigRejected) — machine contract rejects live logs |
| DOC-9 → [UI-13] | 🟠 | tri-state flags undocumented |
| DOC-14 | 🟠 | PROVIDERS.md documents nonexistent `Strict` JSON mode (actual: Auto/ResponseFormat/PromptOnly) |
| DOC-5/6/7/8/10/11/12/13/15/16/17/18 | 🟡⚪ | ui-modes/quiet undocumented; json dialects unreconciled; watch --refresh-ms missing; BOOKFORGE_AUDIO_PRICING_PATH only in internal handoff; mojibake acknowledged nowhere; corrections-protection absolutism; writeup-v1.4 claims currency while describing v2 batch contract; codex-handoff-v2.6.0 "in progress" though shipped; ElevenLabs fail-open only in CHANGELOG; clippy invocation differs across README/CONTRIBUTING/CI |

**Verdict tables worth keeping:** README claim-by-claim — 40+ claims checked, all ACCURATE except: bounded-decompression (overclaim), corrections-protection (absolute wording), private-data caveat (serve flow). Installers/layout/version-hygiene internally consistent; CHANGELOG matches 2.6.1 reality; zero broken relative links; audiobooks.md is a model reference (every default verified); ROADMAP solves staleness deliberately; benchmark docs commendably honest about their own gaps.

### 4.12 Dependencies & build health
**Toolchain:** rustc/cargo 1.96.0 vs declared MSRV 1.88 (CI enforces 1.88 via msrv job ✔).
**Build:** `cargo check` 0 errors/0 warnings · `clippy --workspace --all-targets` **0 warnings** (no lint allows) · fmt clean · 1 rustdoc warning (`[`stitch`]` ambiguous, audio/lib.rs:15).
**Tests:** 861 tests — 854 passed, 6 failed, 3 ignored(deliberate). All 6 failures (5 unique) **pass in isolation → load-flaky**, not deterministic: shared loopback-capture harness SendError races (audio provider, llm provider, pdf ocr ×2) + cli control-watcher timeout + cli translate mock e2e port race. These will intermittently break CI.
| Item | Detail |
|---|---|
| Duplicate versions | base64 0.22/0.23, getrandom 0.2/0.4, hashbrown, syn 2/3, windows-sys — all transitive/unavoidable |
| **zip 8.6 feature bloat** | defaults pull ALL codecs + legacy crypto + zstd-sys (**C toolchain at build**) + `time 0.3.53` — one of seven crates pinning MSRV at exactly 1.88. Trim to `default-features=false, features=["deflate-flate2"]` |
| Unused deps (new) | store declares **serde** with zero references (plus known toml) |
| Licenses | 317 packages, no copyleft; Unicode-3.0 fine; CDLA-Permissive (webpki-roots data) worth a license-report note; bzip2 clause non-canonical but permissive |
| Key risks | rusqlite bundled SQLite 3.51.3 (current era, historical CVE classes patched); zip advisories N/A by usage (no disk extraction) except bomb-class mitigated by archive_limits; reqwest/rustls/tokio/clap all current-line, known advisory classes fixed at locked versions |
| MSRV posture | Seven crates sit at floor 1.88 with zero headroom (time, zip, ratatui×4, instability, darling). Dependabot rule covers only rusqlite — resolver-3 MSRV awareness + the required msrv CI job are the real gates. Keep msrv job a required check; comment dependabot.yml accordingly |

---

## 5. Cross-cutting themes

### Duplication inventory (top consolidation targets)
| What | Sites | Divergence risk |
|---|---|---|
| Token estimation | **8 sites, 4 formulas** (chars/4, bytes/4, words×4/3, word-count): llm planning/qa×2/double_check, scheduler(bytes!), core glossary(bytes!), epub reader(words), judge example | Same segment measured differently by different subsystems → batch packing, glossary budgets, printed costs disagree; CJK worst case |
| EPUB emitters | pdf/epub.rs (string templates, manual escaping, frozen timestamps) vs epub/writer.rs (quick-xml) + third mimetype copy in reflow | New EPUB features must land twice; escaping rules differ |
| Pricing loaders | cost.rs, audio_cost.rs, judge example + dual providers.json copies | judge copy already lost cache-pricing support |
| Retry/backoff | llm provider (RetryAfterPolicy, 60s cap, LCG jitter) vs audio provider (300s cap, wall-clock jitter, ignores policy) | Same outage, different cadence per pipeline |
| validate_scope | glossary vs entity vs style | whitespace-trim behavior differs — `" "` creates junk partitions in two stores |
| Provider/model defaults | translate mod.rs, estimate.rs ("unknown"), serve options.rs consts, llm provider constructors | Adding a model touches 4 places; dashboard list already drifts |
| Filename sanitizers ×3, percentile ×3, open_in_browser ×2, run-summary printers ×2, Toki Pona vocab ×2 | — | Slow drift already visible in ETA formatters |

### Feature asymmetry highlights
- Style-sheet and entity stores: full CRUD in store + CLI, **zero dashboard endpoints** — invisible to the non-developer audience they exist for; entities lacks CLI export while glossary/style have it.
- Audiobook: dashboard reaches ~15 of 25 flags (no chapters/prune/retry-failed/text-normalization/timeout); estimator ≠ launcher preprocessing.
- `TranslationProfile::Fastest` user-selectable but no code branches on it — inert option.
- PDF conversion integration tests Unix-only while Windows is a shipped platform.
- Dashboard parses EPUBs in-process (audio) vs child-isolated (translate) — inconsistent threat model.

### Dead-code rollup (all caller-verified)
core: marker helpers (1 subtly wrong), 3 error variants + 2 deps, ProviderErrorKind, ModelRouteConfig, 3 PromptVersion variants, SpineItem.linear, render_and_fingerprint · store: find_cached_translation, mark_segment_failed_if_unfinished, toml, serde · epub: rebuild_epub_with_language, TableCell arms · pdf: convert_pdf wrapper + crop-render path (entire pdftoppm crop implementation), jpg arms · llm: split_batch re-export, with_limiter, glossary_rule_counts, max_rounds loop · audio: bookforge-epub dep, tracing dep, wide pub surface · cli: MarkFailed variant (test-only), sender_with_progress, shutdown(), dropped_count(), issue_style, benchmark --concurrency, Fastest profile · Ungated test hooks in production binaries: BOOKFORGE_TEST_FINALIZE_BOUNDARY_DELAY_MS + 6 BOOKFORGE_MOCK_* vars (undocumented, env-controllable timing in releases).

### TODO/FIXME census
Zero matches workspace-wide for TODO/FIXME/HACK/todo!/unimplemented!. 19 `#[allow(dead_code)]` sites, most actionable ones listed above.

---

## 6. Recommended roadmap

### This week (easy wins, high value)
1. **Rotate + relocate the three API keys** (H-8) + add `/tests/*KEY*` ignore rule.
2. **Fix the watcher churn** (H-7): own one connection; gate record_migration behind applied-check.
3. **Kill the phantom-segment cascade** (H-3): filter at aggregation + writer log-and-continue.
4. **Resume truthfulness** (H-4): completion decision from DB statuses.
5. **Wire ArchiveReadBudget into reflow/validate** (H-2) + fix validate extension case.
6. **Gate the three stdout prints** behind human_stdout_enabled (UI-22).
7. **Pass cancel token into resume** (CLI-3) and into `--ui tui` attach (UI-2).
8. **Prompt template encoding** (LLM-7) + rename repair templates to .v3.md (DRIFT-1).
9. **ElevenLabs fallback → cheapest, not priciest** (AUDIO-3).
10. **ffmpeg `-nostdin` + stdin(null)** (AUDIO-5).
11. **ci.yml permissions block + SHA-pin remaining actions** (INFRA-3/4) + epubcheck checksum (INFRA-5).
12. **Trim zip features** (deps §): drops zstd-sys C-build + one MSRV pinner.
13. Delete the verified-dead list (§5) — mechanical PR.
14. gitignore: `__pycache__/`, `.agents/`; untrack the .pyc (INFRA-2/11).
15. Doc fixes: exit-codes section, events.md 4 missing variants, Strict-mode correction, store-location truth (DOC-2/3/4/14).

### Worth starting now (investments)
1. **Transactional checkpoints + SQL-enforced correction freeze** (STORE-1/3) — the reliability story is the product.
2. **Cross-process job leases** (STORE-6/CLI-7/CLI-8 + AUDIO-2): generalize RuntimeLaunchClaim into the store; adopt for resume AND audiobook out_dir.
3. **One token estimator in core** (DUP-1) with script-awareness (CJK×1, else chars/4) — fixes LLM-5 sizing + cost honesty; plan the cache-namespace version bump in the same commit.
4. **Property/fuzz harness for the reader↔writer protocol** (EPUB-18) — roundtrip "untouched blocks byte-stable" would pin the project's hardest invariant; add hostile-input fixtures + EPUB2 sample.
5. **Serve auth token on all routes** (SERVE-1) + private-dir fixes (H-6) + child-parse isolation for audio (SERVE-7).
6. **Shared provider registry** (defaults/models/pricing loaders) — collapses 4 drift sites and the JS/server tier-table duplication.
7. **Windows parity for PDF integration tests** via in-process fake PopplerTools (TEST-2/PDF-2) — currently the platform-specific claims (scrubbing, timeouts) ship untested on Windows.
8. **Deflake the loopback test harness** (build §) — 6 intermittent CI failures undermine trust in the suite.
9. **RTL/CJK handling in PDF reconstruction** (PDF-7/9/10) — for a translation product these are correctness, not polish.
10. **Store retention/prune + status enums + CHECK constraints** (STORE-12/17).
11. **BOOKFORGE_HOME / store-location resolution** (UI-24/DOC-4) — the single most confusing UX trap found.
12. **Consolidated rendering onto RunState** (UI-31) + single JSON envelope schema with version (UI-23).

### Deliberately not recommended now
- Migrating off cargo-dist residuals (documented, accepted, upstream-blocked).
- base64/getrandom dedup (transitive-owned).
- Per-feature MSRV matrix (nice-to-have; required-check msrv job suffices).

---

## 7. Hygiene catalog (artifacts)

| Path | Contents | Size | Verdict |
|---|---|---|---|
| `target/` | Build tree | 53.3 GB | `cargo clean` stale profiles periodically |
| `.claude/worktrees/` | 9 abandoned agent worktree clones | 31.3 GB | Prune |
| `.tools/` | Vendored JRE×2, epubcheck, llvm-mingw, winlibs, poppler, dist | 2.76 GB | Digest sidecars; delete orphaned java17 |
| `tmp/` | Manual QA scratch | 1.0 GB | Clean |
| `tests/` (untracked) | Personal EPUBs incl. z-library-named copyrighted works + derivatives | 451 MB | Move personal media out of repo tree |
| `.bookforge/` (root) | Real job history, 56MB sqlite, runner exes ×5 identical | 371 MB | Consider retention cap + doctor surfacing DB size |
| `crates/bookforge-cli/.bookforge/runs/` | 787 dirs: 736 synthetic test leftovers + **51 empty `retry_pending_overrides_<pid>`** | 24 KB | Reap in code (startup sweep once owner-PID gone) |
| `scripts/__pycache__/` | Tracked bytecode | 30 KB | Untrack + ignore |

Naive `git add -A` today stages nothing new (except future writes into `.agents/`/`__pycache__/`-style paths) — but the margin is positional (see H-8).

---

## 8. What's genuinely good (worth protecting)

- **The core invariant holds.** Models never touch markup; validation is adversarial (marker pinning, locale-aware number equivalence, source-copy detection); patch-don't-regenerate keeps untouched bytes identical.
- **archive_limits.rs** — bounds enforced at runtime with a lying-metadata test; zip-slip structurally impossible; billion-laughs defused by design.
- **Zero-warning clippy/check across 80k LOC with no lint suppressions**; zero TODO debt.
- **correct.rs and review-HTML XSS hygiene** are textbook; serve's browser-axis defense (loopback gate, Host pinning, per-route constant-time CSRF, exhaustively tested) is strong within its stated model.
- **Storage layer**: right pragmas, injection-proof, cache filters thorough, exceptional tests including cross-process smoke.
- **Subprocess handling** (poppler): env allowlists with rationale, pipe-full deadlock prevention, kill-and-reap with a clever self-executing-binary test rig.
- **Docs culture**: claim-by-claim accuracy, honest changelogs, threat-model docs with dates and accepted-risk reasoning, benchmark docs admitting their own gaps.

---

## 9. Caveats & corrections

- **PDF-1 refuted**: the suspected Windows-only test compile break was wrong — `cargo test -p bookforge-pdf` compiles and runs on Windows; the ungated tests don't touch cfg(unix) helpers. (Kept here because the earlier static claim circulated.)
- Flaky-test root cause undiagnosed beyond "passes isolated"; likely shared-loopback-port/harness races.
- SERVE-4 traversal reachability reasoned statically (axum percent-decoding); end-to-end exploit not executed per audit rules.
- Dependency "latest version" statements are knowledge-dated (audit day), not crates.io-queried; run `cargo audit` (already wired in CI) for authoritative advisories.
- LLM-13 (deepseek-v4-flash reasoning classification) may be intentional; verify against provider capability docs before changing.
- Several agents' severities assume a multi-user-machine threat model; single-user laptop users can downgrade SERVE-1/3/5 accordingly.

---

## Appendix: finding-ID index (agent → count)

CORE 1–14 · STORE 1–18 · EPUB 1–18 · LLM 1–20 · PDF 1–22 (PDF-1 refuted) · AUDIO 1–18 · CLI 1–18 · UI 1–31 · SERVE 1–10 (+quality notes) · INFRA 1–13 · DEAD 1–8 · DUP 1–12 · ASYM 1 · DRIFT 1–3 · TEST 1–2 · DOC 1–18 · BUILD (toolchain/test-health results, unnumbered)

Merged items: EPUB-1/EPUB-2/CLI-6/DOC-1 → H-2 · CORE-4/LLM-1/LLM-16 → LLM-1 row · LLM-2/CLI-2 → H-3 · STORE-2/CLI-9 → H-7 · STORE-6/CLI-8 → CLI-8 row · AUDIO-3/DOC-15 → AUDIO-3 row · DUP-1/LLM-5 → both rows cross-referenced.
