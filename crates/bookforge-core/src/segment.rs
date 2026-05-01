use serde::{Deserialize, Serialize};

use crate::ir::{BlockId, SectionId};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SegmentId(pub String);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub id: SegmentId,
    pub section_id: SectionId,
    pub ordinal: usize,
    pub block_ids: Vec<BlockId>,
    pub source: SegmentSource,
    pub context: SegmentContext,
    pub constraints: SegmentConstraints,
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentSource {
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentContext {
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SegmentConstraints {
    pub preserve_markers: Vec<String>,
    pub preserve_spans: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SegmentStatus {
    Queued,
    InFlight,
    Succeeded,
    Failed,
    RetryPending,
    NeedsReview,
    SkippedCached,
}
