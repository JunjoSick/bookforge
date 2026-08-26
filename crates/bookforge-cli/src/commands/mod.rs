pub mod audiobook;
pub mod control;
pub mod convert;
pub mod correct;
pub mod doctor;
pub mod entity;
pub mod estimate;
pub mod glossary;
pub mod ingest_flags;
pub mod inspect;
pub mod plan;
pub mod reconfigure;
pub mod reflow;
pub mod resume;
pub mod retry;
pub mod review;
#[cfg(feature = "serve")]
pub mod serve;
pub mod status;
pub mod style;
pub mod tail;
pub mod translate;
pub mod validate;
#[cfg(feature = "tui")]
pub mod watch;

/// Lowest accepted dashboard refresh interval in milliseconds. `watch` and
/// `serve` previously enforced different floors (20 vs 50); both now share
/// this one so a flag value behaves identically on either UI.
pub(crate) const MIN_REFRESH_MS: u64 = 20;
