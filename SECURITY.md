# Security Policy

## Supported Versions

The latest published release line is `2.6.x`. The `3.x` line is the current
development/release-candidate line until a corresponding GitHub release is
published. Security fixes are applied to `3.x` first; critical fixes may be
backported to the latest `2.6.x` patch release when practical.

| Version | Support |
| --- | --- |
| 3.x development/release candidate | Supported for security fixes |
| 2.6.x latest published line | Supported for critical security fixes |
| < 2.6 | Unsupported |

Do not assume that an unreleased checkout has the same support or artifact
guarantees as a published release. Check the GitHub Releases page before
installing a binary.

## Reporting a Vulnerability

Please use GitHub's private vulnerability reporting form:

<https://github.com/JunjoSick/bookforge/security/advisories/new>

Do not include API keys, book contents, `.bookforge` databases, or other
private material in a public issue. If private reporting is unavailable,
contact the maintainer through the repository owner's GitHub profile and ask
for a private security channel; do not disclose exploit details publicly while
waiting for a response.

Include the affected version or commit, operating system, impact, a minimal
reproduction that contains no secrets or copyrighted book content, and any
logs with credentials and source text removed.

## Response Expectations

- Acknowledgement: within 7 calendar days.
- Initial triage and severity assessment: within 14 calendar days.
- Remediation or a mitigation/update: within 30 calendar days when practical.

Timelines can change for reports requiring provider or operating-system
coordination. Reporters will be kept informed, and credit will be offered in
the advisory unless anonymity is requested.

## Scope

In scope are vulnerabilities in BookForge's Rust crates, CLI, local dashboard,
EPUB/PDF/audio input handling, release artifacts, installers, CI workflows,
dependency configuration, and repository handling of credentials or private
book data. Out of scope are vulnerabilities in third-party provider APIs,
compromised user machines or provider accounts, and behavior explicitly
chosen by a user such as `serve --no-auth` or running untrusted input with
insufficient system resources. Those may still be reported when they expose a
BookForge-specific unsafe default or missing warning.
