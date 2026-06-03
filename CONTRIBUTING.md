# Contributing to BookForge

BookForge is maintained by one person. Contributions are welcome but the
maintainer's response time is "weeks, not days." If something is urgent
to you and not to me, the fastest path is usually a fork.

## Before you start

Read the architectural invariants in [`docs/ROADMAP.md`](docs/ROADMAP.md) §1.
They are non-negotiable. The most important one:

> The program owns EPUB structure. The model only ever sees validated
> JSON prose payloads.

Pull requests that ask the LLM to emit, repair, or reassemble XHTML
will be declined regardless of how well they work in isolation. If you
think your change needs to violate an invariant, open an issue first so
we can talk about whether the design needs to change instead.

## Issues

### Bug reports

Please include:

- BookForge version (`bookforge --version`).
- Operating system and architecture.
- A minimal repro EPUB (or, if the EPUB is private, a description of
  the structural features that triggered the bug — footnotes, RTL
  passages, drop caps, etc.).
- The exact command you ran.
- Relevant excerpts from the job's `events.jsonl` if the bug happened
  mid-run, redacted as needed.

API keys, real book contents, and `.bookforge/` snapshots are private
data — don't paste them into issues.

### Feature requests

Check [`docs/ROADMAP.md`](docs/ROADMAP.md) first. If your request maps to
a milestone, that's the answer to "when." If it doesn't, open an issue
and we'll talk about whether it fits.

The roadmap is sequenced deliberately, so requests to skip ahead
(e.g. "can we get bilingual output before EPUBCheck integration?")
usually won't move.

## Pull requests

Before submitting:

```bash
cargo fmt
cargo clippy --all-targets --all-features
cargo test
```

All three should pass cleanly. CI runs the same set.

Add tests for any behaviour change, especially around:

- EPUB segmentation and rebuild (`bookforge-epub`).
- The translation contracts (plain, marker-safe, run-preserving).
- The cache key composition.
- Anything that touches the `ProgressSink` event schema or the JSONL
  field set (these are semver-stable for v1.x; see ROADMAP §1.5).

Some lifecycle integration tests under `crates/bookforge-cli/tests/`
read `test/test.epub`, which is gitignored. They will be skipped or
fail on a fresh clone unless you drop your own fixture at that path.
A real-but-small public-domain EPUB works fine.

## Commit and PR style

- Recent commits show the convention: scoped, lowercase subject
  (`feat(v1.4):`, `fix:`, `docs:`), short body explaining *why*.
- One PR per logical change. If you find unrelated cleanups along the
  way, a follow-up PR is usually better than a bundle.
- Don't `--no-verify` past hooks. If a hook fails, fix the underlying
  issue.

## What not to commit

- API keys, secrets, real book EPUBs, `.bookforge/` artefacts.
- Anything under `target/`, `.bookforge/`, `test/`, `.claude/`, `*.env`,
  `*.key` — these are already gitignored, but be careful with
  `git add -A`.
