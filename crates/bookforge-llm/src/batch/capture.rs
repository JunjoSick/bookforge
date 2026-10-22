//! Opt-in capture of raw provider content for batch responses that failed to
//! parse, or that parsed with items rejected by validation.
//!
//! A failed batch response normally survives only as a serde error position
//! ("expected `,` or `}` at line 1 column 3684") or a list of missing item
//! IDs, and a rejected item only as a count; the content itself is dropped,
//! so the cause cannot be diagnosed after the fact. Setting
//! [`CAPTURE_DIR_ENV`] to a directory writes each such response there
//! verbatim, next to the items that were requested and each rejected item's
//! error. Nothing is captured while the variable is unset, and the content is
//! never logged: the files hold book text and stay on the local disk.

use super::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;

/// Directory that receives one JSON file per failed batch response.
pub(super) const CAPTURE_DIR_ENV: &str = "BOOKFORGE_DEBUG_BATCH_RESPONSE_DIR";

static CAPTURE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn capture_failed_batch_response(
    batch: &TranslationBatch,
    content: &str,
    error: &str,
    rejected_items: &[BatchItemFailure],
) {
    let Some(dir) = std::env::var_os(CAPTURE_DIR_ENV).filter(|dir| !dir.is_empty()) else {
        return;
    };
    let dir = PathBuf::from(dir);
    if let Err(write_error) = write_capture(&dir, batch, content, error, rejected_items) {
        tracing::warn!(
            dir = %dir.display(),
            error = %write_error,
            "could not capture failed batch response"
        );
    }
}

pub(super) fn write_capture(
    dir: &Path,
    batch: &TranslationBatch,
    content: &str,
    error: &str,
    rejected_items: &[BatchItemFailure],
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file_stem: String = batch
        .id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let sequence = CAPTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = dir.join(format!(
        "{file_stem}-{}-{sequence}.json",
        bookforge_core::progress::now_ms()
    ));
    let requested: Vec<serde_json::Value> = batch
        .items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.item_id,
                "kind": item.kind,
                "text": item.source_text,
            })
        })
        .collect();
    let rejected: Vec<serde_json::Value> = rejected_items
        .iter()
        .map(|failure| serde_json::json!({"id": failure.item_id, "error": failure.error}))
        .collect();
    let record = serde_json::json!({
        "batch_id": batch.id,
        "mode": format!("{:?}", batch.mode),
        "error": error,
        "rejected_items": rejected,
        "requested": requested,
        "content": content,
    });
    let bytes = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
    std::fs::write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookforge_core::ir::SectionId;

    #[test]
    fn capture_records_the_raw_content_and_the_requested_items() {
        let dir = std::env::temp_dir().join(format!(
            "bookforge-batch-capture-{}-{}",
            std::process::id(),
            bookforge_core::progress::now_ms()
        ));
        let batch = TranslationBatch {
            id: "batch_0001/split 0".to_string(),
            ordinal: 0,
            mode: BatchMode::Plain,
            kind: BatchKind::Translation,
            items: vec![TranslationBatchItem {
                item_id: "seg:b_1".to_string(),
                segment_id: SegmentId("seg".to_string()),
                section_id: SectionId("section".to_string()),
                block_id: BlockId("b_1".to_string()),
                ordinal: 0,
                kind: "paragraph".to_string(),
                source_text: "He said \"no\".".to_string(),
                text_runs: Vec::new(),
                protected_spans: Vec::new(),
                required_markers: Vec::new(),
                checksum: "checksum".to_string(),
            }],
            token_estimate: 10,
            section_id: SectionId("section".to_string()),
        };
        let content = r#"{"items":[{"id":"seg:b_1","translation":"Disse "no"."}]}"#;

        let path = write_capture(
            &dir,
            &batch,
            content,
            "invalid batch JSON: expected `,`",
            &[],
        )
        .expect("capture is written");

        let file_name = path.file_name().and_then(|name| name.to_str()).unwrap();
        assert!(file_name.starts_with("batch_0001_split_0-"), "{file_name}");
        let record: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(record["content"], content);
        assert_eq!(record["error"], "invalid batch JSON: expected `,`");
        assert_eq!(record["requested"][0]["id"], "seg:b_1");
        assert_eq!(record["requested"][0]["text"], "He said \"no\".");
        assert_eq!(record["rejected_items"], serde_json::json!([]));
        std::fs::remove_dir_all(&dir).ok();
    }
}
