//! Structured QA findings shared between the translation engine, the
//! checkpoint store, and the reporting surfaces.
//!
//! Historically the engine only returned human-readable error strings and the
//! CLI re-parsed them into finding kinds. That parsing lost block attribution,
//! misclassified severities, and concatenated unrelated retry attempts into
//! one unreadable line. The canonical type lives here because the engine
//! (`bookforge-llm`), the store (`bookforge-store`), and the CLI all depend on
//! this crate; the store re-exports the kind enum for backward compatibility.

use crate::ir::QaFindingSeverity;
use serde::{Deserialize, Serialize};

/// Canonical classification of a deterministic QA finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QaFindingKind {
    ProtectedSpanMissing,
    InlineMarkerMissing,
    InlineMarkerDuplicated,
    InlineMarkerUnknown,
    MarkerStructure,
    BatchBlockMismatch,
    SourceCopyUnchanged,
    TargetLanguageGate,
    ProviderError,
    Interrupted,
    Other,
}

impl QaFindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProtectedSpanMissing => "protected_span_missing",
            Self::InlineMarkerMissing => "inline_marker_missing",
            Self::InlineMarkerDuplicated => "inline_marker_duplicated",
            Self::InlineMarkerUnknown => "inline_marker_unknown",
            Self::MarkerStructure => "marker_structure",
            Self::BatchBlockMismatch => "batch_block_mismatch",
            Self::SourceCopyUnchanged => "source_copy_unchanged",
            Self::TargetLanguageGate => "target_language_gate",
            Self::ProviderError => "provider_error",
            Self::Interrupted => "interrupted",
            Self::Other => "other",
        }
    }

    /// Parse the canonical string form. Unknown strings map to [`Self::Other`]
    /// so legacy rows never fail to load.
    pub fn from_db_str(value: &str) -> Self {
        match value {
            "protected_span_missing" => Self::ProtectedSpanMissing,
            "inline_marker_missing" => Self::InlineMarkerMissing,
            "inline_marker_duplicated" => Self::InlineMarkerDuplicated,
            "inline_marker_unknown" => Self::InlineMarkerUnknown,
            "marker_structure" => Self::MarkerStructure,
            "batch_block_mismatch" => Self::BatchBlockMismatch,
            "source_copy_unchanged" => Self::SourceCopyUnchanged,
            "target_language_gate" => Self::TargetLanguageGate,
            "provider_error" => Self::ProviderError,
            "interrupted" => Self::Interrupted,
            _ => Self::Other,
        }
    }

    /// Default severity for the kind. Individual findings may override it —
    /// for example a source-copy hit on a section title is editorially
    /// expected and stays a warning, while unchanged prose is an error.
    pub fn default_severity(self) -> QaFindingSeverity {
        match self {
            Self::SourceCopyUnchanged | Self::TargetLanguageGate | Self::Interrupted => {
                QaFindingSeverity::Warning
            }
            _ => QaFindingSeverity::Error,
        }
    }
}

/// One structured, block-attributable finding produced by the engine or the
/// finalization pipeline. `block_id` is `Some` whenever the finding can be
/// pinned to a single block; segment-level findings leave it `None`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineFinding {
    pub kind: QaFindingKind,
    pub severity: QaFindingSeverity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_id: Option<String>,
}

impl EngineFinding {
    pub fn new(kind: QaFindingKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            severity: kind.default_severity(),
            message: message.into(),
            block_id: None,
        }
    }

    pub fn with_block_id(mut self, block_id: impl Into<String>) -> Self {
        self.block_id = Some(block_id.into());
        self
    }

    pub fn with_severity(mut self, severity: QaFindingSeverity) -> Self {
        self.severity = severity;
        self
    }
}

/// Decompose a legacy concatenated segment error string (the pre-3.0 format
/// the engine persisted into `segments.error`) into structured findings so
/// reports can render old rows with the same vocabulary. The parser mirrors
/// the phrases the engine has emitted historically; unknown text becomes a
/// single `Other` finding rather than being dropped.
pub fn findings_from_legacy_error_text(error: &str) -> Vec<EngineFinding> {
    let trimmed = error.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut findings = Vec::new();
    let mut rest = trimmed;
    while let Some(pos) = rest.find("; ") {
        push_legacy_fragment(&rest[..pos], &mut findings);
        rest = &rest[pos + 2..];
    }
    push_legacy_fragment(rest, &mut findings);
    if findings.is_empty() {
        findings.push(EngineFinding::new(QaFindingKind::Other, trimmed));
    }
    findings
}

fn push_legacy_fragment(fragment: &str, findings: &mut Vec<EngineFinding>) {
    let fragment = fragment.trim();
    if fragment.is_empty() {
        return;
    }
    // Legacy rows prefix the whole concatenation with "error: " once; strip it
    // so the first fragment still classifies by its actual phrase.
    let fragment = fragment.strip_prefix("error: ").unwrap_or(fragment);
    let kind = if fragment.starts_with("translation is unchanged from the source-language prose") {
        QaFindingKind::SourceCopyUnchanged
    } else if fragment.starts_with("batch translation block mismatch") {
        QaFindingKind::BatchBlockMismatch
    } else if fragment.contains("protected span") {
        QaFindingKind::ProtectedSpanMissing
    } else if fragment.contains("inline marker") || fragment.contains("marker") {
        QaFindingKind::MarkerStructure
    } else if fragment.starts_with("error:") {
        QaFindingKind::Other
    } else {
        QaFindingKind::Other
    };
    findings.push(EngineFinding::new(kind, fragment));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_legacy_concatenated_segment_error() {
        let findings = findings_from_legacy_error_text(
            "error: translation is unchanged from the source-language prose; \
             batch translation block mismatch: missing=[\"b_000026\"], extra=[], duplicate=[]",
        );
        assert_eq!(findings.len(), 2);
        assert_eq!(findings[0].kind, QaFindingKind::SourceCopyUnchanged);
        assert_eq!(findings[0].severity, QaFindingSeverity::Warning);
        assert_eq!(findings[1].kind, QaFindingKind::BatchBlockMismatch);
        assert_eq!(findings[1].severity, QaFindingSeverity::Error);
    }

    #[test]
    fn unknown_text_becomes_other_not_empty() {
        let findings = findings_from_legacy_error_text("something unfamiliar happened");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, QaFindingKind::Other);
    }

    #[test]
    fn kind_round_trips_through_db_strings() {
        for kind in [
            QaFindingKind::ProtectedSpanMissing,
            QaFindingKind::SourceCopyUnchanged,
            QaFindingKind::Other,
        ] {
            assert_eq!(QaFindingKind::from_db_str(kind.as_str()), kind);
        }
        assert_eq!(
            QaFindingKind::from_db_str("mystery"),
            QaFindingKind::Other
        );
    }
}
