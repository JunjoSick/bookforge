# Release pipeline security

Security review date: 2026-08-31. The release workflow is based on cargo-dist
0.32.0, as pinned by `cargo-dist-version` in `dist-workspace.toml`. It carries a
small checked-in hardening overlay, recorded with `allow-dirty = ["ci"]` so a
future `dist generate` cannot silently discard the security review.

## cargo-dist 0.32 configuration findings

The v0.32 configuration supports `github-action-commits`, which overrides an
action's generated version with a full commit SHA. The following immutable
release commits are configured and reflected in `release.yml`:

- `actions/checkout` v7.0.1: `3d3c42e5aac5ba805825da76410c181273ba90b1`
- `actions/upload-artifact` v7.0.1: `043fb46d1a93c77aae656e7c1c64a875d1fc6a0a`
- `actions/download-artifact` v8.0.1: `3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c`
- `actions/attest` v4.2.0: `f7c74d28b9d84cb8768d0b8ca14a4bac6ef463e6`

> **Pin reconciliation (2026-08-31):** the four SHAs above were re-verified
> against `dist-workspace.toml [dist.github-action-commits]` and the generated
> `release.yml`; the checkout pin is v7.0.1 (`3d3c42e5…`), not the v6.0.2 SHA
> previously listed here.

This closes the mutable-action-reference item. Updating an action now requires
an intentional config change and workflow regeneration.

The workflow now also includes the following hand-maintained controls:

- `verify-release-gates` queries check runs for the exact tagged SHA and fails
  unless the latest completed attempt for every required CI/security job is
  successful. The required names are maintained in
  `scripts/verify-release-gates.sh`.
- The workflow-wide permission is `contents: read`; only `host` receives
  `contents: write`, `attestations: write`, and `id-token: write`.
- Plan/build jobs receive only the artifact permissions they need. The plan
  job has an artifact-scoped token for cargo-dist metadata planning; build
  commands do not receive `GH_TOKEN`.
- cargo-dist is installed with `cargo install cargo-dist --version 0.32.0
  --locked`, so its installer is not executed by piping a remote script to a
  shell. The cargo package checksum and its lockfile are verified by Cargo.
- `scripts/validate-release-security.sh` runs in the CI `fmt` job to catch
  accidental regeneration or permission/pin drift before a release tag.

The conclusions above are based on the cargo-dist 0.32 config reference and the
v0.32.0-generated workflow. No local `dist` or `cargo-dist` executable was
available for an additional schema/help check.

## Open findings and residual risk

### Host publishing permissions

The broad workflow-wide write grant is removed. The `host` job still requires
`contents: write` to create the release, `attestations: write` and
`id-token: write` for Sigstore provenance. A compromised command or action in
that job can therefore publish or modify release content.

The exact-commit gate runs before `plan`; however, GitHub cannot revoke the
host token if the host job itself is compromised. Keep the host job small,
review action pins, and treat the release workflow as a protected-code-owner
surface.

### Container Rust bootstrap

The cargo-dist bootstrap no longer uses `cargo-dist-installer.sh | sh`. Hosted
runners already provide Cargo, and container jobs retain a fallback that pipes
the pinned rustup installer from `https://sh.rustup.rs` to `sh` when their image
does not contain Cargo. This is a remaining bootstrap-origin risk.

The durable fix is to use container images with a preinstalled, pinned Rust
toolchain (or a checksum-verified rustup installer) and remove that fallback.

### Exact-check freshness and branch policy

The preflight fails closed when a required check is missing, pending, cancelled,
or unsuccessful for the exact tag SHA. It does not wait for checks; a tag pushed
too early must be retried after the checks complete. Repository settings must
also require the CI/security checks before merging to the release branch, since
the workflow can verify success but cannot enforce branch protection itself.

## Existing protections and durable options

Sigstore-backed GitHub artifact attestations remain enabled with
`github-attestations = true` and run in the separately permissioned `host` job.
They provide verifiable provenance for released assets, but they do not make a
compromised producer safe: a compromised producer can attest compromised
output.

The release workflow is intentionally hand-maintained in this branch. Keep the
following ownership rules:

1. Run `bash scripts/validate-release-security.sh` after every workflow or
   cargo-dist configuration change.
2. Review generated diffs after every cargo-dist upgrade; do not remove
   `allow-dirty = ["ci"]` without reapplying the permission and gate review.
3. Replace the container rustup fallback when the build images can carry a
   pinned toolchain.

References: [cargo-dist configuration](https://axodotdev.github.io/cargo-dist/book/reference/config.html),
[cargo-dist v0.32.0](https://github.com/axodotdev/cargo-dist/releases/tag/v0.32.0),
and the official action release pages for [checkout](https://github.com/actions/checkout/releases),
[upload-artifact](https://github.com/actions/upload-artifact/releases),
[download-artifact](https://github.com/actions/download-artifact/releases), and
[attest](https://github.com/actions/attest/releases).

## Dashboard authentication residuals

**Status:** deliberately deferred, owner decision 2026-07-21. **Update
2026-08-26 (release-candidate only):** the primary intended mitigation is
implemented on the **unreleased** remediation branch (audit wave 1, H-5) — the
dashboard mints a random session token at startup, includes it only in the
printed bootstrap URL, and requires it on every route outside that page;
`--no-auth` restores unauthenticated serving explicitly. It ships only when the
branch merges and is released (v3.0.0 candidate; PR #112 open/blocked), so it
is **not present in the published v2.6.1**. The analysis below is preserved as
written when the risk was accepted; concurrent-launch caps also landed on the
candidate branch, while cookie exchange and spend limits remain open ideas.

The remaining accepted risks are deliberately narrower: the token is not
exchanged for an `HttpOnly; SameSite=Strict` cookie, remembered-provider-key
spend limits are not a separate quota, and `--no-auth` provides no local API
authentication by design. Revisit these controls before exposing the server
outside loopback, using it on a multi-user machine, or allowing unattended
provider spend with meaningful cost.
