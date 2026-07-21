//! Identify and remove audio chunk files left over from previous synthesis
//! runs.
//!
//! Chunk file names encode every synthesis setting that can change the audio
//! (`chapter-<NNN>-part-<NNN>-<hash16>.<ext>`), so changing the voice, model,
//! speed, format, or the source text produces a different name and the old
//! file is never referenced again. Nothing deletes those orphans automatically
//! — resume treats any existing non-empty file as done — so they accumulate in
//! the output directory. This module lets the CLI report and, only when asked,
//! delete them.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use crate::builder::ChunkRecord;

/// Audio container extensions BookForge can emit. Used to keep pruning to
/// files this crate actually manages.
const MANAGED_EXTENSIONS: &[&str] = &["mp3", "opus", "aac", "flac", "wav", "pcm"];

/// A managed chunk file in the output directory that the current plan will not
/// use — safe to delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleChunk {
    pub path: PathBuf,
    pub bytes: u64,
}

/// True when `name` matches the managed chunk pattern
/// `chapter-<digits>-part-<digits>-<16 hex>.<audio ext>`.
///
/// This deliberately excludes stitched per-chapter outputs (`chapter-NNN.ext`),
/// the assembled `.m4b`, and `manifest.json`: none of them carry the `-part-`
/// segment and the synthesis hash, so none can be misclassified as a stale
/// chunk.
fn is_managed_chunk_name(name: &str) -> bool {
    let Some((stem, ext)) = name.rsplit_once('.') else {
        return false;
    };
    if !MANAGED_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()) {
        return false;
    }
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    parts[0] == "chapter"
        && parts[2] == "part"
        && !parts[1].is_empty()
        && parts[1].bytes().all(|b| b.is_ascii_digit())
        && !parts[3].is_empty()
        && parts[3].bytes().all(|b| b.is_ascii_digit())
        && parts[4].len() == 16
        && parts[4].bytes().all(|b| b.is_ascii_hexdigit())
}

/// List managed chunk files in `out_dir` that the current `plan` does not
/// reference. A resume never uses these, so they are safe to remove.
///
/// Returns an empty list if `out_dir` does not exist. Results are sorted by
/// path for stable reporting.
pub fn find_stale_chunks(out_dir: &Path, plan: &[ChunkRecord]) -> std::io::Result<Vec<StaleChunk>> {
    let kept: HashSet<&str> = plan.iter().map(|chunk| chunk.file.as_str()).collect();
    let entries = match fs::read_dir(out_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut stale = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if is_managed_chunk_name(name) && !kept.contains(name) {
            let bytes = entry.metadata()?.len();
            stale.push(StaleChunk {
                path: entry.path(),
                bytes,
            });
        }
    }
    stale.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(stale)
}

/// Delete the given stale files, returning the number removed and total bytes
/// freed. Files that are already gone are counted as removed; any other IO
/// error is returned.
pub fn remove_stale_chunks(stale: &[StaleChunk]) -> std::io::Result<(usize, u64)> {
    let mut removed = 0usize;
    let mut freed = 0u64;
    for chunk in stale {
        match fs::remove_file(&chunk.path) {
            Ok(()) => {
                removed += 1;
                freed = freed.saturating_add(chunk.bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                removed += 1;
            }
            Err(error) => return Err(error),
        }
    }
    Ok((removed, freed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::{ChunkRecord, ChunkStatus};

    fn record(file: &str) -> ChunkRecord {
        ChunkRecord {
            chapter_index: 0,
            chapter_title: "Ch".to_string(),
            part: 1,
            kind: crate::text::ChunkKind::Body,
            file: file.to_string(),
            chars: 10,
            synthesis_sha256: "0".repeat(64),
            status: ChunkStatus::Pending,
            audio_sha256: None,
            bytes: None,
            error: None,
        }
    }

    #[test]
    fn recognizes_managed_chunk_names_only() {
        assert!(is_managed_chunk_name(
            "chapter-001-part-002-0123456789abcdef.mp3"
        ));
        assert!(is_managed_chunk_name(
            "chapter-012-part-001-fedcba9876543210.wav"
        ));
        // Stitched per-chapter output and the assembled book are not chunks.
        assert!(!is_managed_chunk_name("chapter-001.mp3"));
        assert!(!is_managed_chunk_name("audiobook.m4b"));
        assert!(!is_managed_chunk_name("manifest.json"));
        // Wrong hash length / non-hex / non-audio extension.
        assert!(!is_managed_chunk_name("chapter-001-part-001-short.mp3"));
        assert!(!is_managed_chunk_name(
            "chapter-001-part-001-zzzzzzzzzzzzzzzz.mp3"
        ));
        assert!(!is_managed_chunk_name(
            "chapter-001-part-001-0123456789abcdef.txt"
        ));
    }

    #[test]
    fn finds_only_chunks_absent_from_the_plan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path();
        let keep = "chapter-001-part-001-0123456789abcdef.mp3";
        let stale = "chapter-001-part-001-fedcba9876543210.mp3";
        for name in [
            keep,
            stale,
            "chapter-001.mp3",
            "audiobook.m4b",
            "manifest.json",
        ] {
            fs::write(out.join(name), b"data").expect("write fixture");
        }

        let plan = vec![record(keep)];
        let found = find_stale_chunks(out, &plan).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, out.join(stale));

        let (removed, freed) = remove_stale_chunks(&found).expect("remove");
        assert_eq!(removed, 1);
        assert_eq!(freed, 4);
        assert!(!out.join(stale).exists());
        // Kept chunk and stitched outputs are untouched.
        assert!(out.join(keep).exists());
        assert!(out.join("chapter-001.mp3").exists());
        assert!(out.join("audiobook.m4b").exists());
    }

    #[test]
    fn missing_directory_yields_no_stale_files() {
        let found = find_stale_chunks(Path::new("does-not-exist-xyz"), &[]).expect("scan");
        assert!(found.is_empty());
    }
}
