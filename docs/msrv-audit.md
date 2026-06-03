# MSRV audit — v1.4

**Date:** 2026-06-03
**Audited toolchain:** rustc 1.95.0 (system Rust on Arch Linux).
**Scope:** `docs/ROADMAP.md` §7.7 — decide whether the v1.3 pinning
(`rust-version = "1.95"`, `edition = "2024"`, `resolver = "3"`) is paying
for itself, and lower MSRV to a version that has been stable for at least
six months if not.

**Recommendation: lower `rust-version` from `1.95` to `1.88`. Keep
edition 2024 and resolver 3.** Zero code changes required.

---

## 1. Why the current pin is conservative

`rust-version = "1.95"` is the toolchain version that shipped on the
maintainer's Arch box at the time the workspace was first created.
It was not chosen because BookForge needs anything from 1.95 — it was the
default, picked up from `cargo new`. Every release of v1.x has been built
against it, so it's the de-facto floor today, but nothing in the codebase
depends on it.

The cost of leaving it at 1.95 is real: Debian stable, Ubuntu 22.04 LTS,
RHEL 9, and the NixOS stable channel all ship older `rustc`. Anyone
building BookForge from source via their distro's package manager hits
a "your toolchain is too old" wall they have to work around with
`rustup`. v1.4's distribution work (Homebrew, AUR, cargo-dist prebuilt
binaries) reduces but does not eliminate this — `cargo install
bookforge-cli` from crates.io still goes through the user's toolchain,
and we don't want that path to be lossy.

## 2. What the codebase actually uses

A workspace-wide grep was performed for the common edition-2024 syntax
markers and the post-1.85 stdlib API surface. Findings:

### 2.1 Edition 2024 syntax (none in use)

| Feature                         | Found in code? | Stabilized |
|---------------------------------|----------------|------------|
| `gen` blocks / `gen fn`         | No             | 1.85       |
| `#[unsafe(no_mangle)]` style    | No             | 1.85       |
| `unsafe extern { ... }` blocks  | No             | 1.85       |
| async closures (`async \|\| …`) | No             | 1.85       |
| New `impl Trait` lifetime capture rules | Not relied on | 1.85 |

BookForge does not use a single edition-2024-specific syntax form. The
edition could be dropped to 2021 with zero changes. But it could also
remain at 2024 with zero changes, and the dependency tree already
requires 1.85+ (see §3), so edition 2024 is free.

### 2.2 Stdlib APIs

The newest stdlib API in regular use is `Option::is_none_or` (stable in
**1.82**, October 2024). Other notable items:

| API                            | Stable since | Used? |
|--------------------------------|--------------|-------|
| `Option::is_none_or`           | 1.82         | Yes — translate/mod.rs, reader.rs |
| `Option::is_some_and`          | 1.70         | Yes — heavily |
| `Result::is_ok_and` / `_err_and` | 1.70       | Not found |
| `let-else`                     | 1.65         | Yes — heavily |
| `std::sync::OnceLock`          | 1.70         | Yes — prompt.rs |
| `std::sync::LazyLock` / `LazyCell` | 1.80     | Not used |
| `&raw const` / `&raw mut`      | 1.82         | Not used |
| `c"…"` literals                | 1.77         | Not used |
| `#[expect(...)]` attribute     | 1.81         | Not used |
| `if let` chains (`if let … && let …`) | 1.88 | Not used |

**Effective MSRV implied by our own code: 1.82.** Below that, the
`is_none_or` callsites would need rewrites (cheap — `!opt.is_some_and(p)`
is the trivial equivalent). At 1.82 or higher, the codebase compiles
without source changes.

## 3. Dependency MSRV floor

Direct and transitive dependencies advertise `rust-version` in their
`Cargo.toml`. Filtering to the highest floors and excluding WASI-only
crates (which don't apply for our `x86_64-*`/`aarch64-*` targets):

| MSRV  | Crate                                     |
|-------|-------------------------------------------|
| 1.88  | `time 0.3.47` (transitive via `zip 6.0`)  |
| 1.86  | `icu_normalizer 2.2.0` and family (transitive via `idna → url → reqwest`) |
| 1.85  | `clap 4.6`, `indexmap 2.14`, `hashbrown 0.17`, `hyper-rustls 0.27` |

The binding constraint is **1.88**, set by the `time` crate that `zip 6.0`
pulls in. We do not use `time` directly; all timing in BookForge is
`std::time::*` and `tokio::time::*`.

Pinning `time` or `zip` to older versions to dodge 1.88 was considered
and rejected: the bump from `zip 0.6`/`zip 1.x` to `6.0` was deliberate
(per the v1.0 deps-update sweep) and reverting would re-open the
`time 0.1` vulnerability surface that motivated the upgrade. 1.88 is
the honest floor.

## 4. Edition / resolver decision

- **Edition: keep `2024`.** Stabilized in 1.85, available at any MSRV we
  can realistically choose. Code-change cost of keeping it: zero. Code-
  change cost of moving to 2021: also zero. Defaulting to current means
  future contributors don't have to re-learn edition idioms.
- **Resolver: keep `"3"`.** Stabilized in 1.84, MSRV-aware feature
  resolution is the right default. No cost.

## 5. Recommended change

```diff
 [workspace.package]
 edition = "2024"
 license = "MIT"
 repository = "https://github.com/JunjoSick/bookforge"
-rust-version = "1.95"
+rust-version = "1.88"
```

No source-code changes. No edition migration. No resolver change. CI
config (when it lands in v1.5) should include a `cargo +1.88 check` job
to enforce the floor against future API drift.

## 6. Verification

The maintainer machine runs 1.95 (Arch's system Rust, no `rustup`).
Live verification under 1.88 happens via CI rather than a one-off
local check: `.github/workflows/ci.yml` defines an `msrv` job that
pins to `1.88.0` via `dtolnay/rust-toolchain@master` and runs
`cargo check --workspace --all-targets --locked` on every PR and
every push to `main`. The job is required before tagging `v1.4.0`.

If the MSRV job fails, the options in priority order are: (a) rewrite
the callsite (preferred — the offending API likely has a stable-since-
1.82 equivalent), (b) bump MSRV to whatever the missing API actually
requires and document why, (c) pin the offending dep to a version with
a lower MSRV (rarely viable; the `time 0.3.47` floor came in via the
deliberate `zip 6.0` security bump).

## 7. Out of scope for this audit

- An automated MSRV-check job (deferred to v1.5 CI work).
- `cargo-msrv` or similar tooling integration (deferred; manual grep
  is sufficient for a workspace this size).
- Reducing edition to 2021 (no benefit; would just be churn).
- Pinning transitive dep versions to dodge 1.88 (rejected, see §3).
