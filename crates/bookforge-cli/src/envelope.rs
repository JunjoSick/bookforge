//! Single versioned stdout JSON envelope (UI-23).
//!
//! ## Machine contract (v2)
//!
//! Every stdout JSON line emitted under `--ui json` has exactly three
//! top-level members:
//!
//! ```json
//! {"v":2,"kind":"<kind>","payload":{...}}
//! ```
//!
//! - `v` — u64 wire-dialect version, currently `2`. Bumped whenever the shape
//!   of `kind` values or of any `payload` layout changes. Consumers should
//!   fail fast on an unknown `v`.
//! - `kind` — string discriminator. Known values:
//!   - `"event"` — payload is one `bookforge_core::ProgressEvent`, serialized
//!     exactly as persisted in `events.jsonl` (externally tagged: the variant
//!     name is the sole key). Emitted by `translate`/`resume --ui json`.
//!   - `"audiobook"` — payload is the bespoke audiobook progress object,
//!     including its inner `"event":"audiobook_*"` discriminator (e.g.
//!     `audiobook_plan`, `audiobook_chunk_finished`, `audiobook_finished`,
//!     `audiobook_pruned`, `audiobook_planning_started`,
//!     `audiobook_plan_detected_sizes`). Inner field meanings are unchanged.
//!   - `"serialization_error"` — `payload` is `null`; emitted instead of a
//!     torn/malformed line if a payload cannot be serialized. Consumers may
//!     count these; they never appear mid-line.
//! - Unknown `kind` values must be ignored by consumers (forward
//!   compatibility); the line always remains one self-contained JSON object.
//!
//! Scope: this envelope applies **only** to stdout in `--ui json` mode. The
//! `events.jsonl` file log keeps the un-enveloped `ProgressEvent` schema
//! documented in `docs/events.md`, `tail <job> --json` passes those persisted
//! objects through unchanged, and dashboard SSE frames keep their wave-1
//! `state`/`done` event framing. `--ui json-v1` reproduces the historical
//! raw-line stdout dialects byte-for-byte for pre-envelope automation.
//!
//! Version rationale: there was no explicit version signal before, so the two
//! shipped-incompatible dialects are retroactively designated `v1`; the first
//! explicitly versioned dialect therefore starts at `v2`.

use serde::Serialize;

/// Wire-dialect version carried by every enveloped stdout line.
pub(crate) const STDOUT_ENVELOPE_VERSION: u64 = 2;

/// Envelope kind for `bookforge_core::ProgressEvent` payloads.
pub(crate) const KIND_EVENT: &str = "event";

/// Envelope kind for the audiobook command's bespoke progress payloads.
pub(crate) const KIND_AUDIOBOOK: &str = "audiobook";

const KIND_SERIALIZATION_ERROR: &str = "serialization_error";

/// Wrap one payload record in the versioned envelope and serialize it as a
/// single compact JSON line.
///
/// Serialization failures degrade to an explicit [`KIND_SERIALIZATION_ERROR`]
/// record rather than a panic or a malformed stream, so a bad payload can
/// never corrupt lines around it silently.
pub(crate) fn stdout_line<T: Serialize + ?Sized>(kind: &str, payload: &T) -> String {
    let wrapped = match serde_json::value::to_raw_value(payload) {
        Ok(raw) => serde_json::json!({
            "v": STDOUT_ENVELOPE_VERSION,
            "kind": kind,
            "payload": raw,
        }),
        Err(_) => serde_json::json!({
            "v": STDOUT_ENVELOPE_VERSION,
            "kind": KIND_SERIALIZATION_ERROR,
            "payload": null,
        }),
    };
    serde_json::to_string(&wrapped).unwrap_or_else(|_| {
        // Unreachable in practice (the value above is a plain JSON object),
        // but a guaranteed-parseable line beats a panic mid-stream.
        format!(
            "{{\"v\":{STDOUT_ENVELOPE_VERSION},\"kind\":\"{KIND_SERIALIZATION_ERROR}\",\"payload\":null}}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Serializer;

    #[test]
    fn event_payload_round_trips_inside_the_envelope() {
        let event = bookforge_core::ProgressEvent::StageStarted {
            stage: "translating".to_string(),
            timestamp_ms: 1_234,
        };
        let line = stdout_line(KIND_EVENT, &event);

        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["v"], STDOUT_ENVELOPE_VERSION);
        assert_eq!(parsed["kind"], "event");
        assert_eq!(
            parsed["payload"]["StageStarted"]["stage"], "translating",
            "payload must stay byte-compatible with the persisted event schema"
        );
    }

    #[test]
    fn audiobook_payload_preserves_its_inner_discriminator() {
        let payload = serde_json::json!({"event": "audiobook_plan", "chunks": 3});
        let line = stdout_line(KIND_AUDIOBOOK, &payload);

        let parsed: serde_json::Value = serde_json::from_str(&line).expect("valid JSON line");
        assert_eq!(parsed["v"], STDOUT_ENVELOPE_VERSION);
        assert_eq!(parsed["kind"], "audiobook");
        assert_eq!(parsed["payload"]["event"], "audiobook_plan");
        assert_eq!(parsed["payload"]["chunks"], 3);
    }

    /// A payload whose serializer fails must produce the explicit
    /// `serialization_error` record, never a torn line or a panic.
    #[derive(Debug)]
    struct Unserializable;

    impl Serialize for Unserializable {
        fn serialize<S: Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("nope"))
        }
    }

    #[test]
    fn unserializable_payload_degrades_to_an_explicit_error_record() {
        let line = stdout_line(KIND_EVENT, &Unserializable);

        let parsed: serde_json::Value = serde_json::from_str(&line).expect("still valid JSON");
        assert_eq!(parsed["v"], STDOUT_ENVELOPE_VERSION);
        assert_eq!(parsed["kind"], "serialization_error");
        assert!(parsed["payload"].is_null());
    }

    #[test]
    fn one_record_per_line_without_interior_newlines() {
        let payload = serde_json::json!({"event": "audiobook_chunk_finished"});
        let line = stdout_line(KIND_AUDIOBOOK, &payload);

        assert_eq!(line.lines().count(), 1);
        assert!(!line.contains('\n'));
    }
}
