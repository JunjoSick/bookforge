//! Bounded-decompression budget for untrusted EPUB archives.
//!
//! Production code paths construct budgets exclusively through
//! [`validate_archive_metadata`] with [`crate::archive_limits::DEFAULT_ARCHIVE_LIMITS`].
//! The module is additionally exposed publicly under `#[doc(hidden)]` as a
//! test-support surface: integration-test harnesses (hostile corpus,
//! property harness) inject tiny explicit bounds via
//! [`ArchiveReadBudget::new`] so zip-bomb and ratio-lie cases run in
//! milliseconds instead of needing ≥64 MiB fixtures. Nothing in this module
//! consults the injected limits at any other call site, and the production
//! constants stay unchanged.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

use bookforge_core::{BookforgeError, Result};
use zip::ZipArchive;

/// EPUBs commonly contain hundreds of resources; ten thousand leaves ample room for
/// image-heavy and highly segmented books while rejecting entry-count denial of service.
pub(crate) const MAX_ARCHIVE_ENTRIES: usize = 10_000;
/// A single 64 MiB document is far larger than a normal EPUB chapter (including the
/// War and Peace corpus fixture) but keeps one decompression allocation bounded.
pub(crate) const MAX_ENTRY_UNCOMPRESSED_SIZE: u64 = 64 * 1024 * 1024;
/// Images and fonts can make a legitimate EPUB much larger than its text. A 512 MiB
/// expanded archive accommodates those books without allowing gigabyte-scale expansion.
pub(crate) const MAX_TOTAL_UNCOMPRESSED_SIZE: u64 = 512 * 1024 * 1024;
/// Ordinary DEFLATE-compressed prose is usually well below 10:1. This generous ceiling
/// permits unusually repetitive XML/CSS while rejecting classic highly-compressible bombs.
pub(crate) const MAX_ENTRY_COMPRESSION_RATIO: u64 = 200;
/// The archive-wide ceiling catches many moderately suspicious entries whose combined
/// expansion would otherwise be excessive while leaving substantial headroom for books.
pub(crate) const MAX_ARCHIVE_COMPRESSION_RATIO: u64 = 100;
/// Do not allow an archive so large that metadata itself becomes an easy
/// process/disk exhaustion vector before entry budgets are examined.
pub(crate) const MAX_ARCHIVE_BYTES: u64 = 1_024 * 1024 * 1024;
/// The central directory is parsed and indexed by `ZipArchive::new`, before
/// our per-entry limits run. Keep that allocation bounded up front.
pub(crate) const MAX_CENTRAL_DIRECTORY_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_entry_uncompressed_size: u64,
    pub max_total_uncompressed_size: u64,
    pub max_entry_compression_ratio: u64,
    pub max_archive_compression_ratio: u64,
}

pub(crate) const DEFAULT_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    max_entries: MAX_ARCHIVE_ENTRIES,
    max_entry_uncompressed_size: MAX_ENTRY_UNCOMPRESSED_SIZE,
    max_total_uncompressed_size: MAX_TOTAL_UNCOMPRESSED_SIZE,
    max_entry_compression_ratio: MAX_ENTRY_COMPRESSION_RATIO,
    max_archive_compression_ratio: MAX_ARCHIVE_COMPRESSION_RATIO,
};

/// Check fixed-size ZIP metadata before `ZipArchive::new` allocates its entry
/// index. This is intentionally path-based because all production archive
/// entry points start from a filesystem path; the public metadata validator
/// remains useful for in-memory test archives.
pub(crate) fn preflight_archive_path(path: &Path) -> Result<()> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len > MAX_ARCHIVE_BYTES {
        return Err(limit_error(format!(
            "archive is {file_len} bytes, exceeding the {MAX_ARCHIVE_BYTES}-byte limit"
        )));
    }
    const EOCD_LEN: u64 = 22;
    const MAX_COMMENT: u64 = 65_535;
    if file_len < EOCD_LEN {
        return Err(limit_error(
            "archive is too small to contain a ZIP end record".to_string(),
        ));
    }
    let tail_len = file_len.min(EOCD_LEN + MAX_COMMENT) as usize;
    file.seek(SeekFrom::Start(file_len - tail_len as u64))?;
    let mut tail = vec![0; tail_len];
    file.read_exact(&mut tail)?;
    // The end record may be followed by an up-to-64 KiB comment, which is
    // attacker-controlled and may itself contain the EOCD signature bytes.
    // Picking the *last* signature is therefore not enough: only a candidate
    // whose declared comment length lands exactly on the end of the archive
    // can be the real end record. Anything else is truncated or a fake.
    let eocd_rel = find_eocd_record(&tail)
        .ok_or_else(|| limit_error("archive has no valid ZIP end record".to_string()))?;
    let eocd = &tail[eocd_rel..eocd_rel + EOCD_LEN as usize];
    let mut entries = u64::from(u16::from_le_bytes([eocd[10], eocd[11]]));
    let mut central_size = u64::from(u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]));
    let mut central_offset =
        u64::from(u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]));

    // ZIP64 stores the real fields in its preceding end record when any
    // classic field is saturated. Read only the fixed 56-byte record.
    let eocd_pos = file_len - tail_len as u64 + eocd_rel as u64;
    if entries == u16::MAX as u64
        || central_size == u32::MAX as u64
        || central_offset == u32::MAX as u64
    {
        let locator_pos = eocd_pos
            .checked_sub(20)
            .ok_or_else(|| limit_error("archive ZIP64 locator is truncated".to_string()))?;
        let mut locator = [0; 20];
        file.seek(SeekFrom::Start(locator_pos))?;
        file.read_exact(&mut locator)
            .map_err(|_| limit_error("archive ZIP64 locator is truncated".to_string()))?;
        if &locator[..4] != b"PK\x06\x07" {
            return Err(limit_error(
                "archive requires ZIP64 metadata but has no locator".to_string(),
            ));
        }
        let zip64_offset = u64::from_le_bytes(locator[8..16].try_into().expect("locator length"));
        let fixed_zip64_end = zip64_offset
            .checked_add(56)
            .ok_or_else(|| limit_error("archive ZIP64 end record offset overflowed".to_string()))?;
        // The fixed fields must end before the locator. Checking only against
        // the later classic end record would permit a forged ZIP64 record to
        // overlap the locator whose offset we just trusted.
        if fixed_zip64_end > locator_pos {
            return Err(limit_error(
                "archive ZIP64 end record overlaps its locator".to_string(),
            ));
        }
        let mut zip64 = [0; 56];
        file.seek(SeekFrom::Start(zip64_offset))?;
        file.read_exact(&mut zip64)
            .map_err(|_| limit_error("archive ZIP64 end record is truncated".to_string()))?;
        if &zip64[..4] != b"PK\x06\x06" {
            return Err(limit_error(
                "archive ZIP64 end record is invalid".to_string(),
            ));
        }
        let zip64_payload_size = u64::from_le_bytes(
            zip64[4..12]
                .try_into()
                .expect("ZIP64 record-size field length"),
        );
        if zip64_payload_size < 44 {
            return Err(limit_error(
                "archive ZIP64 end record is shorter than its fixed fields".to_string(),
            ));
        }
        let zip64_end = zip64_offset
            .checked_add(12)
            .and_then(|offset| offset.checked_add(zip64_payload_size))
            .ok_or_else(|| limit_error("archive ZIP64 end record size overflowed".to_string()))?;
        if zip64_end != locator_pos {
            return Err(limit_error(
                "archive ZIP64 end record does not end at its locator".to_string(),
            ));
        }
        entries = u64::from_le_bytes(zip64[32..40].try_into().expect("ZIP64 count length"));
        central_size = u64::from_le_bytes(zip64[40..48].try_into().expect("ZIP64 size length"));
        central_offset = u64::from_le_bytes(zip64[48..56].try_into().expect("ZIP64 offset length"));
    }

    if entries > MAX_ARCHIVE_ENTRIES as u64 {
        return Err(limit_error(format!(
            "entry count limit exceeded: archive declares {entries} entries, maximum is {MAX_ARCHIVE_ENTRIES}"
        )));
    }
    if central_size > MAX_CENTRAL_DIRECTORY_BYTES {
        return Err(limit_error(format!(
            "central directory is {central_size} bytes, exceeding the {MAX_CENTRAL_DIRECTORY_BYTES}-byte limit"
        )));
    }
    let central_end = central_offset
        .checked_add(central_size)
        .ok_or_else(|| limit_error("central directory offset overflowed".to_string()))?;
    if central_end > file_len {
        return Err(limit_error(
            "central directory extends past end of archive".to_string(),
        ));
    }
    // The central directory ends before whatever sits between it and the end
    // record (archive extra data, digital signature, ZIP64 records). A
    // directory that reaches into the end record is malformed or lying.
    if central_end > eocd_pos {
        return Err(limit_error(
            "central directory overlaps the ZIP end record".to_string(),
        ));
    }
    Ok(())
}

/// Locate the end-of-central-directory record inside the archive tail.
///
/// The record carries its own comment length, and the comment runs to the
/// end of the archive. Scanning backward for the last signature is not
/// enough because the comment is attacker-controlled and may embed the
/// signature bytes; only a candidate whose record plus declared comment
/// reach the archive end exactly can be the genuine record.
fn find_eocd_record(tail: &[u8]) -> Option<usize> {
    const EOCD_LEN: usize = 22;
    if tail.len() < EOCD_LEN {
        return None;
    }
    for start in (0..=tail.len() - 4).rev() {
        if &tail[start..start + 4] != b"PK\x05\x06" {
            continue;
        }
        if start + EOCD_LEN > tail.len() {
            continue;
        }
        let comment_len = u16::from_le_bytes([tail[start + 20], tail[start + 21]]) as usize;
        if start + EOCD_LEN + comment_len == tail.len() {
            return Some(start);
        }
    }
    None
}

#[derive(Debug)]
pub struct ArchiveReadBudget {
    limits: ArchiveLimits,
    total_uncompressed_read: u64,
}

impl ArchiveReadBudget {
    /// Test-support constructor (see module docs): builds a budget with
    /// caller-supplied bounds. Production callers must keep going through
    /// [`validate_archive_metadata`], which also checks declared archive
    /// metadata before any entry bytes move.
    pub fn new(limits: ArchiveLimits) -> Self {
        Self {
            limits,
            total_uncompressed_read: 0,
        }
    }

    /// Read one entry through this budget's limits. Reads one byte past
    /// the effective limit only, so lying compressed streams cannot balloon
    /// allocations.
    pub fn read_entry<R: Read + ?Sized>(
        &mut self,
        reader: &mut R,
        name: &str,
        compressed_size: u64,
    ) -> Result<Vec<u8>> {
        let total_remaining = self
            .limits
            .max_total_uncompressed_size
            .saturating_sub(self.total_uncompressed_read);
        let ratio_limit = compressed_size.saturating_mul(self.limits.max_entry_compression_ratio);
        let read_limit = self
            .limits
            .max_entry_uncompressed_size
            .min(total_remaining)
            .min(ratio_limit);

        // Read one byte past the effective limit. This proves that the entry is too large
        // without allowing the decompressor or Vec to consume the rest of an attacker stream.
        let mut bounded = reader.take(read_limit.saturating_add(1));
        let mut bytes = Vec::with_capacity(read_limit.min(64 * 1024) as usize);
        bounded.read_to_end(&mut bytes)?;
        let bytes_read = bytes.len() as u64;

        if bytes_read > read_limit {
            if ratio_limit == read_limit {
                return Err(limit_error(format!(
                    "per-entry compression ratio limit exceeded for '{name}': expanded data exceeds {}:1",
                    self.limits.max_entry_compression_ratio
                )));
            }
            if self.limits.max_entry_uncompressed_size == read_limit {
                return Err(limit_error(format!(
                    "per-entry uncompressed size limit exceeded for '{name}': expanded data exceeds {} bytes",
                    self.limits.max_entry_uncompressed_size
                )));
            }
            return Err(limit_error(format!(
                "total uncompressed size limit exceeded while reading '{name}': expanded data exceeds {} bytes",
                self.limits.max_total_uncompressed_size
            )));
        }

        self.total_uncompressed_read = self
            .total_uncompressed_read
            .checked_add(bytes_read)
            .ok_or_else(|| limit_error("total uncompressed size limit overflowed".to_string()))?;
        Ok(bytes)
    }
}

/// Validate the archive's declared metadata against `limits` and return the
/// per-entry read budget. Exposed publicly under the module's test-support
/// contract; production callers always pass [`DEFAULT_ARCHIVE_LIMITS`].
pub fn validate_archive_metadata<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    limits: ArchiveLimits,
) -> Result<ArchiveReadBudget> {
    if archive.len() > limits.max_entries {
        return Err(limit_error(format!(
            "entry count limit exceeded: archive has {} entries, maximum is {}",
            archive.len(),
            limits.max_entries
        )));
    }
    let mut total_uncompressed = 0u64;
    let mut total_compressed = 0u64;
    for index in 0..archive.len() {
        let file = archive.by_index_raw(index)?;
        let name = file.name().to_string();
        let uncompressed = file.size();
        let compressed = file.compressed_size();
        if uncompressed > limits.max_entry_uncompressed_size {
            return Err(limit_error(format!(
                "per-entry uncompressed size limit exceeded for '{name}': declared {uncompressed} bytes, maximum is {} bytes",
                limits.max_entry_uncompressed_size
            )));
        }
        if exceeds_ratio(uncompressed, compressed, limits.max_entry_compression_ratio) {
            return Err(limit_error(format!(
                "per-entry compression ratio limit exceeded for '{name}': declared {uncompressed} uncompressed bytes from {compressed} compressed bytes, maximum is {}:1",
                limits.max_entry_compression_ratio
            )));
        }

        total_uncompressed = total_uncompressed
            .checked_add(uncompressed)
            .ok_or_else(|| limit_error("total uncompressed size limit overflowed".to_string()))?;
        if total_uncompressed > limits.max_total_uncompressed_size {
            return Err(limit_error(format!(
                "total uncompressed size limit exceeded: declared {total_uncompressed} bytes, maximum is {} bytes",
                limits.max_total_uncompressed_size
            )));
        }
        total_compressed = total_compressed.checked_add(compressed).ok_or_else(|| {
            limit_error("total compressed size overflowed while checking ratio".to_string())
        })?;
    }

    if exceeds_ratio(
        total_uncompressed,
        total_compressed,
        limits.max_archive_compression_ratio,
    ) {
        return Err(limit_error(format!(
            "overall compression ratio limit exceeded: declared {total_uncompressed} uncompressed bytes from {total_compressed} compressed bytes, maximum is {}:1",
            limits.max_archive_compression_ratio
        )));
    }

    Ok(ArchiveReadBudget::new(limits))
}

fn exceeds_ratio(uncompressed: u64, compressed: u64, maximum_ratio: u64) -> bool {
    uncompressed > compressed.saturating_mul(maximum_ratio)
}

/// Read one text entry through the caller's budget and decode UTF-8.
/// Every untrusted-archive read in the crate funnels here so the
/// bounded-decompression guarantee has no exceptions. ZIP-level failures are
/// re-raised with the entry name attached: `zip::ZipError` alone reports
/// "specified file not found in archive" with no hint of which declared
/// path lied, leaving hostile manifests indistinguishable from code bugs.
pub fn read_archive_text<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    read_budget: &mut ArchiveReadBudget,
    name: &str,
) -> Result<String> {
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(error) => {
            return Err(BookforgeError::InvalidInput(format!(
                "EPUB entry '{name}' could not be opened: {error}"
            )));
        }
    };
    let compressed_size = file.compressed_size();
    let bytes = read_budget.read_entry(&mut file, name, compressed_size)?;
    String::from_utf8(bytes).map_err(|error| {
        BookforgeError::InvalidInput(format!("EPUB text entry '{name}' is not UTF-8: {error}"))
    })
}

fn limit_error(message: String) -> BookforgeError {
    BookforgeError::InvalidInput(format!("EPUB decompression {message}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    use super::{
        ArchiveLimits, MAX_ARCHIVE_ENTRIES, preflight_archive_path, validate_archive_metadata,
    };

    const TEST_LIMITS: ArchiveLimits = ArchiveLimits {
        max_entries: 4,
        max_entry_uncompressed_size: 64 * 1024,
        max_total_uncompressed_size: 96 * 1024,
        max_entry_compression_ratio: 10_000,
        max_archive_compression_ratio: 10_000,
    };

    #[test]
    fn rejects_declared_entry_over_per_entry_limit_without_large_test_allocation() {
        let archive = archive_with_repeated_entries(&[("huge.xhtml", 1024 * 1024)]);
        let mut archive = ZipArchive::new(Cursor::new(archive)).expect("test ZIP should open");

        let error = validate_archive_metadata(&mut archive, TEST_LIMITS)
            .expect_err("oversized entry must be rejected");

        assert!(
            error
                .to_string()
                .contains("per-entry uncompressed size limit exceeded for 'huge.xhtml'")
        );
    }

    #[test]
    fn bounded_read_rejects_entry_whose_declared_size_is_a_lie() {
        let mut bytes = archive_with_repeated_entries(&[("liar.xhtml", 1024 * 1024)]);
        patch_central_uncompressed_size(&mut bytes, 1024);
        let mut archive = ZipArchive::new(Cursor::new(bytes)).expect("test ZIP should open");
        let mut budget = validate_archive_metadata(&mut archive, TEST_LIMITS)
            .expect("lying metadata should fit the declared limits");

        let mut file = archive
            .by_name("liar.xhtml")
            .expect("test entry should exist");
        let compressed_size = file.compressed_size();
        let error = budget
            .read_entry(&mut file, "liar.xhtml", compressed_size)
            .expect_err("bounded read must catch actual expansion");

        assert!(
            error
                .to_string()
                .contains("per-entry uncompressed size limit exceeded for 'liar.xhtml'")
        );
    }

    #[test]
    fn rejects_entry_count_over_limit() {
        let entries = (0..5)
            .map(|index| (format!("chapter-{index}.xhtml"), 1usize))
            .collect::<Vec<_>>();
        let entry_refs = entries
            .iter()
            .map(|(name, size)| (name.as_str(), *size))
            .collect::<Vec<_>>();
        let archive = archive_with_repeated_entries(&entry_refs);
        let mut archive = ZipArchive::new(Cursor::new(archive)).expect("test ZIP should open");

        let error = validate_archive_metadata(&mut archive, TEST_LIMITS)
            .expect_err("too many entries must be rejected");

        assert!(error.to_string().contains("entry count limit exceeded"));
    }

    #[test]
    fn preflight_rejects_entry_count_before_archive_indexing() {
        let bytes = archive_with_repeated_entries(
            &(0..(MAX_ARCHIVE_ENTRIES + 1))
                .map(|index| (format!("blob-{index}"), 1usize))
                .collect::<Vec<_>>()
                .iter()
                .map(|(name, size)| (name.as_str(), *size))
                .collect::<Vec<_>>(),
        );
        let path = std::env::temp_dir().join(format!(
            "bookforge-archive-preflight-{}-{}.epub",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, bytes).expect("fixture should write");
        let error = preflight_archive_path(&path).expect_err("entry count must be rejected");
        let _ = std::fs::remove_file(&path);
        assert!(error.to_string().contains("entry count limit exceeded"));
    }

    #[test]
    fn preflight_accepts_archive_whose_comment_embeds_the_end_signature() {
        // The recovered EOCD scan must find the *real* end record even when
        // the archive's trailing comment contains the EOCD signature bytes.
        // Naive last-signature matching would misread the comment as the end
        // record and reject a perfectly valid archive.
        let bytes = with_archive_comment(
            archive_with_repeated_entries(&[("mimetype", 11)]),
            b"\xff\xff trailing comment data PK\x05\x06",
        );
        let path = write_temp_archive(&bytes);
        preflight_archive_path(&path)
            .expect("comment that embeds the signature must not break preflight");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn preflight_rejects_archive_without_a_real_end_record() {
        let mut bytes = archive_with_repeated_entries(&[("mimetype", 11)]);
        let eocd = last_comment_free_eocd(&bytes).expect("fixture has an end record");
        // Corrupt the signature so no candidate can satisfy the comment-length
        // check; the archive remains otherwise parseable garbage.
        bytes[eocd] = 0;
        bytes[eocd + 1] = 0;

        let path = write_temp_archive(&bytes);
        let error = preflight_archive_path(&path).expect_err("missing end record must be rejected");
        let _ = std::fs::remove_file(path);
        assert!(error.to_string().contains("no valid ZIP end record"));
    }

    #[test]
    fn preflight_rejects_central_directory_overlapping_the_end_record() {
        let mut bytes = archive_with_repeated_entries(&[("mimetype", 11)]);
        let eocd = last_comment_free_eocd(&bytes).expect("fixture has an end record");
        // Claim a central directory that runs through the end record.
        let len = bytes.len() as u32;
        bytes[eocd + 12..eocd + 16].copy_from_slice(&len.to_le_bytes());

        let path = write_temp_archive(&bytes);
        let error = preflight_archive_path(&path)
            .expect_err("central directory reaching the end record must be rejected");
        let _ = std::fs::remove_file(path);
        assert!(error.to_string().contains("central directory"));
    }

    fn write_temp_archive(bytes: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bookforge-archive-fixture-{}-{}.epub",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::write(&path, bytes).expect("fixture should write");
        path
    }

    /// Append `comment` to a comment-free archive produced by the zip crate,
    /// patching the end record's comment-length field so the result is a
    /// well-formed archive with a trailing comment.
    fn with_archive_comment(mut bytes: Vec<u8>, comment: &[u8]) -> Vec<u8> {
        assert!(comment.len() <= u16::MAX as usize, "comment fits the field");
        let eocd = last_comment_free_eocd(&bytes).expect("fixture has an end record");
        bytes[eocd + 20..eocd + 22].copy_from_slice(&(comment.len() as u16).to_le_bytes());
        bytes.extend_from_slice(comment);
        bytes
    }

    /// Position of the end record in a comment-free archive: the only
    /// signature whose 22-byte record ends exactly on the final byte.
    fn last_comment_free_eocd(bytes: &[u8]) -> Option<usize> {
        (0..=bytes.len() - 4)
            .rev()
            .find(|&start| &bytes[start..start + 4] == b"PK\x05\x06" && start + 22 == bytes.len())
    }

    #[test]
    fn rejects_declared_total_size_over_limit() {
        let archive =
            archive_with_repeated_entries(&[("one.xhtml", 49 * 1024), ("two.xhtml", 49 * 1024)]);
        let mut archive = ZipArchive::new(Cursor::new(archive)).expect("test ZIP should open");

        let error = validate_archive_metadata(&mut archive, TEST_LIMITS)
            .expect_err("oversized total must be rejected");

        assert!(
            error
                .to_string()
                .contains("total uncompressed size limit exceeded")
        );
    }

    #[test]
    fn rejects_declared_compression_ratio_over_limit() {
        let archive = archive_with_repeated_entries(&[("repetitive.xhtml", 32 * 1024)]);
        let mut archive = ZipArchive::new(Cursor::new(archive)).expect("test ZIP should open");
        let limits = ArchiveLimits {
            max_entry_compression_ratio: 2,
            max_archive_compression_ratio: 10_000,
            ..TEST_LIMITS
        };

        let error = validate_archive_metadata(&mut archive, limits)
            .expect_err("suspicious ratio must be rejected");

        assert!(
            error
                .to_string()
                .contains("per-entry compression ratio limit exceeded")
        );
    }

    fn archive_with_repeated_entries(entries: &[(&str, usize)]) -> Vec<u8> {
        let cursor = Cursor::new(Vec::new());
        let mut writer = ZipWriter::new(cursor);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let chunk = [b'x'; 1024];
        for (name, size) in entries {
            writer
                .start_file(*name, options)
                .expect("test entry should start");
            let mut remaining = *size;
            while remaining > 0 {
                let count = remaining.min(chunk.len());
                writer
                    .write_all(&chunk[..count])
                    .expect("test entry should write");
                remaining -= count;
            }
        }
        writer
            .finish()
            .expect("test ZIP should finish")
            .into_inner()
    }

    fn patch_central_uncompressed_size(bytes: &mut [u8], declared_size: u32) {
        const CENTRAL_HEADER: &[u8] = b"PK\x01\x02";
        let offset = bytes
            .windows(CENTRAL_HEADER.len())
            .rposition(|window| window == CENTRAL_HEADER)
            .expect("central directory header should exist");
        bytes[offset + 24..offset + 28].copy_from_slice(&declared_size.to_le_bytes());
    }
}
