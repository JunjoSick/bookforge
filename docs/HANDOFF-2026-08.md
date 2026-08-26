# BookForge Audit Remediation — Agent Handoff (2026-08-25)

You are resuming a multi-wave remediation campaign. This file is your complete context.
The tracking base is `docs/report.md` (the full audit report; finding IDs are stable references).

**Branch:** `remediation/audit-2026-08` (integration branch, based on `main` @ fe953c4c).
Push it if absent: `git fetch origin && git checkout remediation/audit-2026-08`.

---

## 1. Locked decisions (from the repo owner — do not re-litigate)

1. **Scope:** EVERYTHING in the report — all severities (🔴🟠🟡⚪) plus all 12 investment items (§6 "Worth starting now").
2. **Git workflow:** per-phase PRs. Short-lived workstream branches merged sequentially into `remediation/audit-2026-08`; final reviewable PR set to `main`.
3. **Serve auth (H-5):** token auth DEFAULT-ON (auto-generated session token, printed URL bootstraps browser, all API routes require it) with a `--no-auth` opt-out escape hatch.
4. **Semver:** minor-breaking changes acceptable → final version bump likely **v3.0.0** (status enums replace magic strings, unified estimator changes cache keys, JSON envelope versioning).

## 2. Current state — Wave 0 COMPLETE ✅ · Wave 1 COMPLETE ✅ (2026-08-26)

Wave 1 landed on this branch as individual commits (all gates green post-integration: fmt/clippy 0 warnings/test 1043 passed):

- `test:` lifecycle fixture made EPUBCheck-valid (nav doc + dcterms:modified) — was failing output validation on any machine with EPUBCheck installed.
- **P1-store** (`fix(store)`): H-1/STORE-1 atomic correction freeze (IMMEDIATE txn + guarded upsert, SQL-enforced), STORE-3 single-txn checkpoints, STORE-4 transactional glossary rebuild/rename cascade, STORE-13 partial unique indexes for global rows (+dedupe migration 0009), STORE-16 created_at index, STORE-11 resume re-insert refreshes provider/model/source_hash, STORE-14/15/18 (txn upsert_entities, RETURNING add_glossary_term, StoreError::NotFound), H-7 store half (record_migration gated → zero migration writes on reopen).
- **P1-llm-hotfix** (`fix(llm)`): H-3 llm half (unknown-segment failures re-attributed/unattributable dropped at aggregation), LLM-7 mojibake fixed in 7 templates + guard test extended, DRIFT-1 repair templates renamed .v2→.v3 + stale headings fixed, LLM-6 conservative fence/prose-stripping JSON decode.
- **P1-epub** (`fix(epub)`): H-2 ArchiveReadBudget wired through reflow + validate (+case-insensitive ext), EPUB-3 script/style/svg/math suppression as verbatim paired markers, EPUB-10 depth cap + de-quadratic close scan, EPUB-4 folio-range numeric-heading cleanup, EPUB-11 canonical util.rs replacing 16 divergent helpers (platform-neutral path normalization), EPUB-5/6/7/9/12/13/14/15/16/17, EPUB-8 assessed-only (cell granularity = core redesign). Follow-up `fix(epub)`: writer scanner marker ids restored to single shared ordinal stream (adjacency regression caught by roundtrip suite, now pinned by new tests).
- **P1-cli-lifecycle** (`fix(cli)`): H-3 cli half (checkpoint writer log-and-continue + dropped tally), H-4 resume truthfulness (DB-status-driven completion; blockless terminal rows emitted), H-7 cli half (watcher owns one long-lived connection), CLI-3 Ctrl+C in resume, CLI-4 late-control cannot rewrite terminal outcomes, CLI-5 hard errors mark job failed, CLI-7 rename-based stale claim reclaim, CLI-8 lease acquisition for plain resume, CLI-10 O(n) fallback scan, CLI-16 resume flag parity, CLI-12–18 (honest warning, benchmark provider_config, job-id validation for pause/stop, bounded tail read, doctor non-zero exit w/ --no-fail, release builds drop test-only hook).
- **P1-serve-security** (`fix(serve)`): H-5 token auth default-on (--no-auth opt-out; bootstrap URL flow; header on every route incl. SSE/audio re-wiring), H-6 private dirs/files at serve entry points, SERVE-3 fail-closed PID liveness gate, SERVE-4 strict job-id allowlist, SERVE-5 PrivateTempDir estimate uploads, SERVE-6 launch slot cap, SERVE-7 panic boundary (child isolation deferred: needs audiobook plan-mode plumbing — documented), SERVE-8/9/10 + quality items (spawn_blocking large writes, monotonic launch tags, orphan cleanup, lock eviction).
- Merged external **PR #108** into the campaign branch (script-aware source-copy detection + shared core ScriptClass; groundwork for P3-estimator).

Outstanding sub-items carried forward: STORE-5 sql-file drift reconciliation (wave 3), SERVE-7 child isolation needs plan-mode flags (note for future audio/ui waves), tempfile promotion decision (dev-dep only), CSP unsafe-inline removal (needs nonce plumbing).

## 2.a Historical — Wave 0 COMPLETE ✅

Commits on this branch:
- `636b3a89` docs: add 2026-08 deep audit report (remediation tracking base)
- `e04b1efb` test: deflake loopback-capture harnesses and cli flaky tests
- `af93ce0` chore(infra): CI permissions + SHA pins + epubcheck checksum, zip codec trim, gitignore intent rules

Wave 0 resolved:
- **TEST flakes (all 5 clusters):** root cause = Windows loopback RSTs under thread churn + mock-server lifecycle races. Fixes: tolerant mock IO (`let _ =` writes), capture queued BEFORE response, full content-length-aware request reads, `Shutdown::Write` + drain loop before close, and transient-classification retry wrappers around whole scenarios. Suites verified: audio ×3+×5, llm ×10, pdf ×3, cli ×5, full workspace green.
- **INFRA-3/4:** ci.yml permissions block; all floating action refs SHA-pinned (release.yml already was; left alone).
- **INFRA-5:** EPUBCheck download sha256-verified (v5.3.0, both corpus jobs).
- **INFRA-1/2/11 (partial):** explicit `/tests/*KEY*`, `__pycache__/`, `.agents/` ignore rules; tracked `.pyc` untracked (file kept on disk).
- **Deps §:** zip trimmed to `default-features=false, features=["deflate-flate2-zlib-rs"]` — kills zstd-sys C-toolchain build dep + several transitive chains. NOTE: plain `["deflate-flate2"]` does NOT compile (flate2 default-features=false has no backend); zlib-rs is pure Rust, byte-identical backend.
- **rustdoc:** ambiguous `[stitch]` link fixed (audio/lib.rs).
- Verified: `cargo fmt --check`, clippy --workspace --all-targets ZERO warnings, cargo test --workspace exit 0, `cargo +1.88.0 check --workspace` clean (MSRV holds post-trim).

## 3. Remaining plan

| Wave | Agents | Scope |
|---|---|---|
| **1** | 4 heavy + 1 light | P1-store: H-1 atomic correction freeze (translations.rs:49 check-then-write → single conditional stmt / IMMEDIATE txn), STORE-3 txn checkpoints, STORE-4, STORE-13 NULL scope_id, STORE-16 created_at index, STORE-11, STORE-14/15/18, H-7 store-side gate `record_migration`. · P1-epub: H-2 ArchiveReadBudget into reflow.rs:148–164 + validate.rs:55–96 (+ case-insensitive ext), EPUB-3 script/style suppression, EPUB-10 recursion cap, EPUB-4, EPUB-11 helper dedup, EPUB-5/6/7/9/13, EPUB-12/14–17, assess EPUB-8. · P1-cli-lifecycle: H-3 writer log-and-continue (cli/checkpoint.rs:133), H-4 resume truthfulness (resume.rs:461–517,1167) + regression test, CLI-3 cancel token into resume, CLI-4 completion-window races, CLI-5 stuck-running errors, CLI-7 rename-based claim, CLI-8 lease for plain resume, CLI-10, CLI-16, CLI-12–18, H-7 watcher-owned connection (control.rs:24, RefCell makes JobStore !Send). · P1-serve-security: H-5 token auth default-on + --no-auth, H-6 private dirs via create_private_dir_all semantics (serve.rs:269–282, translation.rs:79, audio.rs:413), SERVE-3 PID liveness, SERVE-4 path sanitize, SERVE-5 temp uploads, SERVE-6 launch cap, SERVE-7 child-isolation for audio parse, SERVE-8/9/10 + quality items. · P1-llm-hotfix (light): H-3 llm-side filter unknown segments at aggregation (batch/rendering.rs:366–377 vs execution.rs:1244/1693), LLM-7 mojibake fix in 7 templates, DRIFT-1 repair .v2.md→.v3.md renames, LLM-6 strip markdown fences/trailing prose. |
| **2** | 4 heavy | P2-llm: LLM-1 cap floors (+CORE-4, LLM-16, mode-dependence), LLM-3 repair-phase signals, LLM-4 batch retry pacing, LLM-9 concurrency/rounds ignored, LLM-13 verify DeepSeek classification against provider docs BEFORE changing, LLM-15 prompt fencing, LLM-10/11/12/14/17/19/20. · P2-audio: AUDIO-1 Windows rename premise, AUDIO-2 out_dir cross-process lock, AUDIO-3 fail-open→cheapest tier, AUDIO-4 CJK splitters, AUDIO-5 ffmpeg -nostdin/stdin(null)/timeout, AUDIO-6/8 asymmetry, AUDIO-7 estimator preprocessing, AUDIO-11 debris sweep, AUDIO-12/13/14, AUDIO-15–18, nav-audio backstop. · P2-pdf: PDF-3 temp leak, PDF-5 OCR wipes figures, PDF-6 header threshold, PDF-10 caption English-only warning, PDF-4 `-i` flag, PDF-8/9/12/13, PDF-14 small half, PDF-22 uncapped OCR body, PDF-11/15/16/18–21. · P2-ui-clap (after P1-cli merges): UI-22 stdout gating, UI-21 exit codes, UI-2 tui footer lie, UI-5 ANSI escaping, UI-9/10 RunState epochs + DroppedEvents, UI-13 tri-state syntax, UI-1, UI-28/30, 🟡⚪ tail. |
| **3** | mixed | P3-estimator (SOLO, cross-crate): DUP-1/LLM-5 one script-aware token estimator in core (CJK×1 else chars/4), rewire llm/core/epub/judge, cache-namespace version bump same commit. ‖ P3-docs (parallel, docs-only): DOC-2 events.md 4 variants, DOC-3 exit codes, DOC-4 store location, DOC-14 Strict mode, DOC-5–18 remainder. THEN sequential: P3-deadcode (re-verify DEAD list post-estimator first), P3-store-hardening (STORE-5 dual migration truth, STORE-12 status enums + CHECK, STORE-17 retention/prune, retry_pending_overrides reaper INFRA-10). |
| **4** | 3 batches ×~2 | Investments: (a) EPUB-18 property/fuzz reader↔writer harness + hostile fixtures + EPUB2 sample; TEST-2/PDF-2 Windows parity via in-process fake PopplerTools. (b) PDF-7/9/10 RTL/CJK reconstruction; provider registry + pricing-loader dedup (DUP rows §5). (c) UI-23/31 JSON envelope v2 + rendering consolidation; ASYM dashboard CRUD for style/entity stores + audiobook flag parity. |
| **5** | orchestrator only | Full gates (fmt/clippy/test/msrv/cargo audit), report.md status column update, CHANGELOG, version bump v3.0.0, PRs to main. |

**Excluded (do not do):** key rotation/moving (owner confirmed keys are intentionally untracked, never leaked, history clean), cargo-dist residuals INFRA-6, base64/getrandom dedup, per-feature MSRV matrix, PDF-1 (refuted).

### Cross-crate contracts (H-fix spans)
- **H-3:** llm-side aggregation filter (P1-llm-hotfix) ↔ cli-side writer tolerance (P1-cli). Both required.
- **H-7:** store-side migration gating (P1-store) ↔ cli-side dedicated watch connection (P1-cli).
- **H-2:** fixed entirely in epub crate (library layer). No cli changes.

## 4. Dispatch playbook (how to run agents)

- Use the Task tool, `subagent_type: general`. Concurrency cap: **≤3 heavy + 1 light** (one machine; cargo serializes on target/ lock — slow but safe).
- **Crate/file ownership is exclusive per concurrent agent.** Never two agents in one crate. Dep files (Cargo.toml/Cargo.lock) frozen unless designated.
- Every agent prompt MUST contain: scope boundaries (exact paths), finding list w/ report line refs, exit criteria, and this mandatory-output clause (an agent once returned EMPTY twice without it):
  > "Final message requirements (MANDATORY — your final message is the only thing returned to the orchestrator; it must be complete even if long): 1) per-finding status, 2) files modified, 3) flagged production issues, 4) verification evidence (commands + results), 5) confirmation of zero-warning clippy."
- Exit criteria per agent: `cargo fmt --check`, `cargo clippy -p <crate> --all-targets` zero warnings, scoped `cargo test -p <crate>` green, structured report.
- Integration gate between waves (orchestrator runs): `cargo fmt --all --check` · `cargo clippy --workspace --all-targets` (zero warnings) · `cargo test --workspace` (exit 0) · commit workstreams as logical commits · then open next wave.
- Re-dispatch immediately on empty/failed agent results.

## 5. Gotchas & lessons learned (READ BEFORE WORKING)

1. **NEVER roundtrip source files through PowerShell Get-Content/Set-Content.** It caused BOM insertion + double-encoded em-dashes (mojibake — ironically finding LLM-7's bug class) and one zero-byte file. Use file edit tools only. If you must inspect bytes: `[System.IO.File]::ReadAllBytes`.
2. **Agents dying mid-task can leave stale-buffer overwrites** (happened twice this campaign: helper blocks vanished while sibling imports remained). After ANY interrupted agent: `git status`, review diffs of its claimed scope, then `cargo check --workspace --all-targets` before trusting anything.
3. **PowerShell quirks:** `$LASTEXITCODE` directly after native commands; `MatchInfo` has no `.Trim()` (use `.Line`); native stderr shows scary-but-harmless `NativeCommandError` wrapper text; avoid masking stderr with `2>$null` when verifying exit codes; `git diff` output through `Select-String "^@@"` works fine.
4. **Toolchain MSRV check:** use fully-qualified `cargo +1.88.0 ...`. Shorthand `+1.88` attempts a network channel sync that fails behind TLS interception.
5. **Windows loopback flake class (now largely fixed):** RSTs under thread churn (~10% fresh connections saturated). The proven pattern, applied across audio/llm/pdf/cli harnesses: tolerant IO (`let _ =` on writes/sends), capture queued BEFORE responding, read requests until headers+content-length complete (single reads split under load), `shutdown(Write)` + drain-read before drop, and whole-scenario retries gated by a transient classifier. **Classifier lesson:** reqwest/hyper hide Winsock codes behind Debug-only formatting — Display-only string checks miss them. The llm classifier walks `std::error::Error::source()` chain downcasting to io::Error AND scans `{:#?}`. The AUDIO classifier is still Display-only (passing today; align it when convenient).
6. **Per-attempt isolation matters in retried scenarios:** shared server-side counters/listeners corrupt state across scenario retries (a retried attempt sees wrong 400/200 sequence or a dead listener). Each retry attempt must spawn its own listener+server+counter (see llm json_mode_auto_fallback / oversized tests for the pattern).
7. `request_metadata_freezes_provider_attempts_per_call` (llm) remains unwrapped by design — a retry would double-count its exact assertion counts. Hasn't flaked recently; revisit only if it does.
8. **Keys:** `tests/{DEEPSEEK,ELEVENLABS,OPENROUTER}_KEY.txt` exist locally, untracked BY INTENT (explicit ignore rule now in .gitignore). Never rotate/delete/commit them. Never print their contents.
9. CI notes: epubcheck checksum line assumes GNU coreutils `sha256sum` (ubuntu runners OK). Workflow YAML wasn't executed end-to-end locally — first CI run after push is the real test.
10. Untracked local noise you WILL see: `target/` 53GB, `.claude/worktrees/` 31GB (nine abandoned clones), `tmp/` 1GB, `.tools/` vendored toolchains, personal EPUBs under `tests/`. Leave them alone (see §6).
11. Report caveats still standing: LLM-13 may be intentional (verify first); SERVE severities assume multi-user threat model; dependency "latest" statements were knowledge-dated (run `cargo audit` at wrap).

## 6. Manual cleanup checklist — REQUIRES EXPLICIT OWNER APPROVAL, never agent-initiated

- [ ] Prune `.claude/worktrees/` (31 GB, nine abandoned clones)
- [ ] Delete `tmp/` scratch (1 GB)
- [ ] Periodic `cargo clean` of stale profiles in `target/` (53 GB)
- [ ] Delete orphaned `.tools/java17` JRE (wrapper resolves jdk-21); add digest sidecars for remaining vendored tools
- [ ] Move personal/copyrighted EPUBs out of `tests/` (legal foot-gun if tree ever shared)
- [ ] Consider retention cap for root `.bookforge/` job history (371 MB)

## 7. Resumption procedure

1. `git checkout remediation/audit-2026-08 && git pull`
2. Read §2 (done) and §3 (next wave table). Wave 1 is next.
3. Dispatch per §4 with scopes from §3; include relevant excerpt lines from docs/report.md §3 detail section in each prompt (report.md is committed on this branch).
4. Between waves: run integration gate, commit, update this handoff's §2 with what landed, push.
5. At wrap (§3 wave 5): update report.md statuses, CHANGELOG, version bump, PRs to main.
