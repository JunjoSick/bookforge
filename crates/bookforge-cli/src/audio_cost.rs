//! Thin re-export of the canonical audio pricing loader in
//! [`bookforge_core::providers`]. The typed schema, embedded default JSON,
//! `BOOKFORGE_AUDIO_PRICING_PATH` override, hard schema-version checks, and
//! per-entry rate validation live there.

pub(crate) use bookforge_core::providers::{AudioCost, estimate_audio_cost, load_audio_pricing};
