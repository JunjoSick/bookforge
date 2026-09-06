//! Canonical single-implementation home for helpers that were previously
//! duplicated (with diverging behaviour) between the reader, writer, and
//! reflow modules (audit EPUB-11). Patch matching depends on these being
//! consistent, so every module routes through this one copy.

use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use bookforge_core::{BookforgeError, Result};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use zip::DateTime;

/// Reject input/output pairs that resolve to the same filesystem destination
/// (identical path, symlink, hardlink, or lexical alias) before any staged
/// output can rename over the source. Shared by reflow and rebuild, the two
/// entry points that read from one path and publish to another.
pub(crate) fn ensure_distinct_paths(label: &str, input: &Path, output: &Path) -> Result<()> {
    if bookforge_core::path::paths_are_aliases(input, output)? {
        return Err(BookforgeError::InvalidInput(format!(
            "{label} paths must be different: {} / {}",
            input.display(),
            output.display()
        )));
    }
    Ok(())
}

/// Translation-marker ids are generated as the prefix followed by a
/// 1-based ordinal; reader and writer must agree on the spelling.
pub(crate) fn marker_id(prefix: &str, marker_ordinal: usize) -> String {
    format!("{prefix}{}", marker_ordinal + 1)
}

/// Elements whose raw content must never be translated: scripts, styles,
/// SVG graphics, and MathML. Reader and writer must agree on this set for
/// marker ordinals to stay aligned (audit EPUB-3).
pub(crate) fn never_translate_element(name: &[u8]) -> bool {
    matches!(name, b"script" | b"style" | b"svg" | b"math")
}

pub(crate) fn local_name(name: &[u8]) -> &[u8] {
    name.rsplit(|byte| *byte == b':').next().unwrap_or(name)
}

pub(crate) fn attr_value_unescaped(
    element: &BytesStart<'_>,
    attr_name: &[u8],
) -> Result<Option<String>> {
    for attr in element.attributes() {
        let attr = attr.map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
        if local_name(attr.key.as_ref()) == attr_name {
            return Ok(Some(
                attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                    .into_owned(),
            ));
        }
    }
    Ok(None)
}

/// Resolve a general entity reference to its replacement text: numeric
/// character references and the HTML5 named set. Unresolvable references
/// are preserved verbatim (`&bad;`) rather than dropped, so data never
/// disappears silently from translatable text; they escape safely when the
/// patched document is written back out.
pub(crate) fn resolve_general_ref(reference: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    if let Some(ch) = reference
        .resolve_char_ref()
        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?
    {
        return Ok(ch.to_string());
    }
    let name = reference
        .decode()
        .map_err(|err| BookforgeError::InvalidInput(err.to_string()))?;
    if let Some(resolved) = quick_xml::escape::resolve_html5_entity(&name) {
        return Ok(resolved.to_string());
    }
    tracing::warn!(entity = %name, "preserving unresolvable entity reference literally");
    Ok(format!("&{name};"))
}

/// Normalize space runs to single spaces (reader/reflow visible text rule).
pub(crate) fn normalize_space(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Names treated as XHTML resources eligible for block extraction and
/// language patching. Matching is case-insensitive on the extension.
pub(crate) fn is_xhtml_resource_name(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, extension)| {
        matches!(
            extension.to_ascii_lowercase().as_str(),
            "xhtml" | "html" | "htm"
        )
    })
}

/// Fixed DOS-epoch timestamp so rebuilt archives are byte-for-byte
/// reproducible for identical inputs.
pub(crate) fn deterministic_zip_time() -> DateTime {
    DateTime::from_date_and_time(1980, 1, 1, 0, 0, 0).expect("DOS epoch timestamp should be valid")
}

pub(crate) fn sibling_work_path(output: &Path, label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let name = output
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("book.epub");
    output.with_file_name(format!(
        ".{name}.bookforge-{label}-{}-{nonce}",
        std::process::id()
    ))
}

/// Atomically reserve a sibling staging file. The caller can safely write to
/// the returned handle: a pre-existing symlink or concurrent writer cannot be
/// followed, and collisions are retried with a distinct suffix.
pub(crate) fn create_sibling_work_file(output: &Path, label: &str) -> Result<(PathBuf, File)> {
    for attempt in 0..128u32 {
        let path = sibling_work_path(output, &format!("{label}-{attempt}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(BookforgeError::InvalidInput(format!(
        "could not reserve a unique temporary path beside {}",
        output.display()
    )))
}

pub(crate) fn commit_staged_output(what: &str, staged: &Path, output: &Path) -> Result<()> {
    if !output.exists() {
        fs::rename(staged, output)?;
        return Ok(());
    }

    let backup = sibling_work_path(output, "backup");
    fs::rename(output, &backup)?;
    match fs::rename(staged, output) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(&backup) {
                tracing::warn!(
                    backup = %backup.display(),
                    error = %error,
                    "{what} EPUB is committed but its backup could not be removed"
                );
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, output);
            Err(error.into())
        }
    }
}

/// Well-formedness check used before committing any regenerated resource.
pub(crate) fn validate_xml(xml: &str) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    loop {
        match reader.read_event()? {
            Event::Eof => return Ok(()),
            _ => continue,
        }
    }
}

/// Resolve an OPF `full-path`/manifest href against its base directory:
/// strips fragment/query, percent-decodes, joins, and normalizes to a
/// forward-slash archive path. Platform-neutral by construction (no
/// host-path parsing); `.` components vanish and `..` cannot climb above
/// the archive root, so normalized names always address inside the EPUB.
pub(crate) fn join_epub_path(base: &str, href: &str) -> String {
    let href = href
        .split('#')
        .next()
        .unwrap_or(href)
        .split('?')
        .next()
        .unwrap_or(href);
    let href = percent_decode_epub_path(href);
    if base.is_empty() {
        normalize_epub_path(&href)
    } else {
        normalize_epub_path(&format!("{base}/{href}"))
    }
}

/// Directory part of an archive path using `/` separators only; returns an
/// empty string for root-level resources.
pub(crate) fn package_base_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(base, _)| base.to_string())
        .unwrap_or_default()
}

pub(crate) fn normalize_epub_path(path: &str) -> String {
    let mut normalized = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                // Climbing above the archive root is meaningless for an
                // in-memory zip; dropping the underflow keeps normalization
                // total and platform-neutral without inventing a name.
                normalized.pop();
            }
            value => normalized.push(value.to_string()),
        }
    }
    normalized.join("/")
}

fn percent_decode_epub_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Upper bound on nesting of paired formatting markers accepted inside a
/// stored/resumed translation. Nested `<mN>` pairs are legitimate inline
/// markup; models occasionally echo pathological hundreds-deep stacks, and
/// rendering them recursively aborted the process instead of failing the
/// affected block. Anything past the cap degrades to a per-block error.
pub(crate) const MAX_MARKER_DEPTH: usize = 32;

/// Normalize entity-like sequences in model-produced translation text so
/// that escaping happens exactly once. `&amp;` becomes `&` before the
/// serializer re-escapes it (previously rendered as a literal `&amp;amp;`),
/// numeric and HTML5 named references decode to their characters, and
/// unknown `&name;` tokens pass through literally, matching how the reader
/// now preserves them in source text.
pub(crate) fn normalize_translation_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut normalized = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(ampersand) = rest.find('&') {
        normalized.push_str(&rest[..ampersand]);
        let tail = &rest[ampersand..];
        match resolved_entity_prefix(tail) {
            Some((decoded, consumed)) => {
                normalized.push_str(&decoded);
                rest = &tail[consumed..];
            }
            None => {
                normalized.push('&');
                rest = &tail[1..];
            }
        }
    }
    normalized.push_str(rest);
    normalized
}

/// Length prefix of an entity whose replacement is known, paired with its
/// decoded value and byte length including the terminating semicolon.
fn resolved_entity_prefix(entity: &str) -> Option<(String, usize)> {
    let mut chars = entity.char_indices();
    debug_assert_eq!(chars.next().map(|(_, ch)| ch), Some('&'));
    if let Some((hash_index, '#')) = chars.next() {
        let (radix, digits_start) = if entity[hash_index + 1..].starts_with(['x', 'X']) {
            (16, hash_index + 2)
        } else {
            (10, hash_index + 1)
        };
        let digits_end = entity[digits_start..]
            .find(';')
            .map(|offset| digits_start + offset)?;
        if digits_end > digits_start
            && entity[digits_start..digits_end]
                .chars()
                .all(|ch| ch.is_digit(radix))
        {
            let code = u32::from_str_radix(&entity[digits_start..digits_end], radix).ok()?;
            let decoded = char::from_u32(code)?.to_string();
            return Some((decoded, digits_end + 1));
        }
        return None;
    }

    let name_end = entity[1..]
        .find(|ch: char| !ch.is_ascii_alphanumeric())
        .map(|offset| 1 + offset)?;
    if entity.as_bytes().get(name_end) != Some(&b';') || name_end <= 2 {
        return None;
    }
    let name = &entity[1..name_end];
    let decoded = quick_xml::escape::resolve_html5_entity(name)?;
    Some((decoded.to_string(), name_end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epub_path_normalization_is_platform_neutral() {
        // Archive paths always use forward slashes; backslashes are opaque
        // filename bytes (never host separators).
        assert_eq!(
            normalize_epub_path("OPS\\Text\\ch.xhtml"),
            "OPS\\Text\\ch.xhtml"
        );
        assert_eq!(normalize_epub_path("a/../b/./c"), "b/c");
        assert_eq!(normalize_epub_path("../top"), "top");
        assert_eq!(
            normalize_epub_path("a/b/../../../z"),
            "z",
            "climbing above the archive root drops the surplus .. components"
        );

        assert_eq!(
            join_epub_path("OPS", "Text/ch%201.xhtml#frag"),
            "OPS/Text/ch 1.xhtml"
        );
        assert_eq!(package_base_dir("OEBPS/text/ch.xhtml"), "OEBPS/text");
        assert_eq!(package_base_dir("chapter.xhtml"), "");
    }

    #[test]
    fn translation_entity_normalization_escapes_exactly_once() {
        assert_eq!(
            normalize_translation_entities("A &amp; B &#65; C &#x43; &notanentity;"),
            "A & B A C C &notanentity;"
        );
        assert_eq!(normalize_translation_entities("bare & amp"), "bare & amp");
        assert_eq!(normalize_translation_entities("a&nbsp;b"), "a\u{a0}b");
        assert_eq!(normalize_translation_entities("plain text"), "plain text");
    }

    #[test]
    fn never_translate_set_matches_reader_and_writer_contract() {
        for name in ["script", "style", "svg", "math"] {
            assert!(never_translate_element(name.as_bytes()), "{name}");
        }
        assert!(!never_translate_element(b"span"));
        assert!(!never_translate_element(b"p"));
    }
}
