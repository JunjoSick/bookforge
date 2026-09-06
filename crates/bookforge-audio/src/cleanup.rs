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
use crate::lock::LOCK_FILE_NAME;

/// Audio container extensions BookForge can emit. Used to keep pruning to
/// files this crate actually manages.
const MANAGED_EXTENSIONS: &[&str] = &["mp3", "opus", "aac", "flac", "wav", "pcm"];

/// A managed chunk file in the output directory that the complete current
/// book plan will not use and is therefore safe to delete.
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

/// AUDIO-11: crash debris in the output directory that no run will ever
/// reference again. The shapes are exactly what this crate can leave behind
/// when a build or stitch dies mid-write:
///
/// - `*.part.tmp` — interrupted atomic chunk/manifest writes (current names
///   are `<chunk>.<pid>-<seq>-<rand>.part.tmp`; legacy names from before the
///   AUDIO-1 fix end in `.part.tmp` / `.replace.bak` too);
/// - `*.replace.bak` — legacy backup copies of overwritten outputs;
/// - `.*.part.<ext>` — staged ffmpeg/m4b publish files (dot-prefixed) left
///   by a stitch killed between encode and rename;
/// - `*.concat.txt`, `chapters.ffmeta.txt` — scratch lists and chapter
///   metadata from an interrupted stitch.
///
/// Lock files are never debris (see [`is_protected_lock_name`]): deleting
/// one would let a second build corrupt the first run's directory.
pub fn is_debris_name(name: &str) -> bool {
    if is_protected_lock_name(name) || name == "manifest.json" {
        return false;
    }
    let lowered = name.to_ascii_lowercase();
    lowered.ends_with(".part.tmp")
        || lowered.ends_with(".replace.bak")
        // Staged ffmpeg publishes keep their container extension after a
        // final ".part", e.g. ".audiobook.m4b.1234-7.part.m4b".
        || (name.starts_with('.') && lowered.contains(".part."))
        // AUDIO-13 intermediates from an interrupted loudnorm stitch.
        || lowered.starts_with(".normalized-chapter-")
        || lowered.ends_with(".concat.txt")
        || lowered == "chapters.ffmeta.txt"
}

/// Files the lock protocol owns; pruning must never see them as deletable.
pub fn is_protected_lock_name(name: &str) -> bool {
    name == LOCK_FILE_NAME || name.ends_with(".bookforge-audio.lock") || name.ends_with(".lock")
}

/// List managed chunk files in `out_dir` that the current `plan` does not
/// reference.
///
/// The caller must provide a complete plan covering every narratable chapter
/// under the current synthesis settings. A chapter-filtered plan is not
/// sufficient: existing chunks for omitted chapters may still be reusable.
/// Given a complete plan, a resume never uses the returned files, so they are
/// safe to remove.
///
/// Crash debris ([`is_debris_name`]) is included in the listing regardless of
/// any plan, so `--prune --dry-run` previews it exactly as it would delete
/// it. A live build holds [`crate::lock::LOCK_FILE_NAME`]; that file (and any
/// lock-suffixed file) is excluded from both listing and removal.
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
        let debris = is_debris_name(name);
        let stale_chunk = is_managed_chunk_name(name) && !kept.contains(name);
        if debris || stale_chunk {
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
    fn complete_plan_keeps_unselected_chapters_and_finds_superseded_chunk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path();
        let chapter_one = "chapter-001-part-001-1111111111111111.wav";
        let chapter_two = "chapter-002-part-001-2222222222222222.wav";
        let chapter_three = "chapter-003-part-001-3333333333333333.wav";
        let superseded = "chapter-002-part-001-aaaaaaaaaaaaaaaa.wav";
        for name in [chapter_one, chapter_two, chapter_three, superseded] {
            fs::write(out.join(name), b"paid audio").expect("write fixture");
        }

        // This is the full-book reference plan used even when the synthesis
        // run itself selects only chapter 2.
        let plan = vec![
            record(chapter_one),
            record(chapter_two),
            record(chapter_three),
        ];
        let found = find_stale_chunks(out, &plan).expect("scan");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, out.join(superseded));

        remove_stale_chunks(&found).expect("remove");
        assert!(!out.join(superseded).exists());
        assert!(out.join(chapter_one).exists());
        assert!(out.join(chapter_two).exists());
        assert!(out.join(chapter_three).exists());
    }

    #[test]
    fn dry_run_listing_does_not_delete_stale_chunks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path();
        let current = "chapter-001-part-001-0123456789abcdef.mp3";
        let superseded = "chapter-001-part-001-fedcba9876543210.mp3";
        fs::write(out.join(current), b"current").expect("write fixture");
        fs::write(out.join(superseded), b"superseded").expect("write fixture");

        // The CLI's dry-run path stops after this read-only listing.
        let found = find_stale_chunks(out, &[record(current)]).expect("scan");
        assert_eq!(found.len(), 1);
        assert!(out.join(current).exists());
        assert!(out.join(superseded).exists());
    }

    #[test]
    fn missing_directory_yields_no_stale_files() {
        let found = find_stale_chunks(Path::new("does-not-exist-xyz"), &[]).expect("scan");
        assert!(found.is_empty());
    }

    #[test]
    fn crash_debris_is_recognized_and_lock_files_protected() {
        assert!(is_debris_name(
            "chapter-001-part-001-0123456789abcdef.wav.1234-0-deadbeef00000000.part.tmp"
        ));
        assert!(is_debris_name(".audiobook.m4b.1234-7.part.m4b"));
        assert!(is_debris_name("manifest.json.replace.bak"));
        assert!(is_debris_name("book.concat.txt"));
        assert!(is_debris_name("chapters.ffmeta.txt"));
        // Managed outputs are never debris.
        assert!(!is_debris_name("chapter-001-part-002-0123456789abcdef.mp3"));
        assert!(!is_debris_name("manifest.json"));
        assert!(!is_debris_name("audiobook.m4b"));
        // Lock files are protected in both name forms.
        assert!(!is_debris_name(LOCK_FILE_NAME));
        assert!(!is_debris_name("out-dir.bookforge-audio.lock"));

        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path();
        let keep = "chapter-001-part-001-0123456789abcdef.mp3";
        let debris = "chapter-001-part-001-fedcba9876543210.mp3.9999-3-abcdefabcdefabcdef.part.tmp";
        let staged = ".audiobook.m4b.1234-7.part.m4b";
        for name in [keep, debris, staged, LOCK_FILE_NAME, "manifest.json"] {
            fs::write(out.join(name), b"data").expect("write fixture");
        }

        let found = find_stale_chunks(out, &[record(keep)]).expect("scan");
        let names: Vec<String> = found
            .iter()
            .map(|chunk| {
                chunk
                    .path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names.contains(&debris.to_string()));
        assert!(names.contains(&staged.to_string()));
        assert!(
            !names
                .iter()
                .any(|name| name == LOCK_FILE_NAME || name == keep || name == "manifest.json")
        );

        let (removed, _) = remove_stale_chunks(&found).expect("remove");
        assert_eq!(removed, 2);
        // The live build's lock file and managed outputs survive.
        assert!(out.join(LOCK_FILE_NAME).exists());
        assert!(out.join(keep).exists());
        assert!(!out.join(debris).exists());
        assert!(!out.join(staged).exists());
    }
}
