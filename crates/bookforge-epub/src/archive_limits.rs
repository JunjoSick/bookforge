use std::io::{Read, Seek};

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

#[derive(Debug, Clone, Copy)]
pub(crate) struct ArchiveLimits {
    pub(crate) max_entries: usize,
    pub(crate) max_entry_uncompressed_size: u64,
    pub(crate) max_total_uncompressed_size: u64,
    pub(crate) max_entry_compression_ratio: u64,
    pub(crate) max_archive_compression_ratio: u64,
}

pub(crate) const DEFAULT_ARCHIVE_LIMITS: ArchiveLimits = ArchiveLimits {
    max_entries: MAX_ARCHIVE_ENTRIES,
    max_entry_uncompressed_size: MAX_ENTRY_UNCOMPRESSED_SIZE,
    max_total_uncompressed_size: MAX_TOTAL_UNCOMPRESSED_SIZE,
    max_entry_compression_ratio: MAX_ENTRY_COMPRESSION_RATIO,
    max_archive_compression_ratio: MAX_ARCHIVE_COMPRESSION_RATIO,
};

#[derive(Debug)]
pub(crate) struct ArchiveReadBudget {
    limits: ArchiveLimits,
    total_uncompressed_read: u64,
}

impl ArchiveReadBudget {
    fn new(limits: ArchiveLimits) -> Self {
        Self {
            limits,
            total_uncompressed_read: 0,
        }
    }

    pub(crate) fn read_entry<R: Read + ?Sized>(
        &mut self,
        reader: &mut R,
        name: &str,
        compressed_size: u64,
    ) -> Result<Vec<u8>> {
        let total_remaining = self
            .limits
            .max_total_uncompressed_size
            .saturating_sub(self.total_uncompressed_read);
        let ratio_limit = compressed_size
            .checked_mul(self.limits.max_entry_compression_ratio)
            .unwrap_or(u64::MAX);
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

pub(crate) fn validate_archive_metadata<R: Read + Seek>(
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
    uncompressed > compressed.checked_mul(maximum_ratio).unwrap_or(u64::MAX)
}

fn limit_error(message: String) -> BookforgeError {
    BookforgeError::InvalidInput(format!("EPUB decompression {message}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

    use super::{ArchiveLimits, validate_archive_metadata};

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
