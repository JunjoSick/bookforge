use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedMarkerOpen {
    pub tag_name: String,
    pub id: String,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmptyMarker {
    pub id: String,
    pub len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerClose {
    pub tag_name: String,
    pub len: usize,
}

pub fn marker_ids_in_text(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if let Some(open) = parse_paired_marker_open(tag) {
            ids.push(open.id);
            rest = &tag[open.len..];
        } else if let Some(empty) = parse_empty_marker(tag) {
            ids.push(empty.id);
            rest = &tag[empty.len..];
        } else if let Some(close) = parse_marker_close(tag) {
            rest = &tag[close.len..];
        } else {
            rest = &tag[1..];
        }
    }

    ids
}

pub fn extract_marker_id(tag: &str) -> Option<String> {
    extract_marker_id_attr(tag).or_else(|| short_marker_name(tag).map(ToString::to_string))
}

fn extract_marker_id_attr(tag: &str) -> Option<String> {
    let id_offset = tag.find("id=")? + 3;
    let quote = tag[id_offset..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value_start = id_offset + quote.len_utf8();
    let value_end = tag[value_start..].find(quote)? + value_start;
    Some(tag[value_start..value_end].to_string())
}

pub fn parse_paired_marker_open(text: &str) -> Option<PairedMarkerOpen> {
    if !text.starts_with('<') {
        return None;
    }
    for tag_name in ["m", "keep"] {
        let prefix = format!("<{tag_name} ");
        if !text.starts_with(&prefix) {
            continue;
        }
        let open_end = text.find('>')?;
        if text[..open_end].ends_with('/') {
            return None;
        }
        let id = extract_marker_id_attr(&text[..=open_end])?;
        return Some(PairedMarkerOpen {
            tag_name: tag_name.to_string(),
            id,
            len: open_end + 1,
        });
    }

    let open_end = text.find('>')?;
    if open_end == 0 {
        return None;
    }
    if text[..open_end].ends_with('/') {
        return None;
    }
    let name = &text[1..open_end];
    if is_short_paired_marker_name(name) {
        return Some(PairedMarkerOpen {
            tag_name: name.to_string(),
            id: name.to_string(),
            len: open_end + 1,
        });
    }

    None
}

pub fn parse_empty_marker(text: &str) -> Option<EmptyMarker> {
    if !text.starts_with('<') {
        return None;
    }
    for tag_name in ["ref", "m", "keep"] {
        let prefix = format!("<{tag_name} ");
        if !text.starts_with(&prefix) {
            continue;
        }
        let end = text.find('>')?;
        let tag = &text[..=end];
        if !tag.ends_with("/>") {
            return None;
        }
        let id = extract_marker_id_attr(tag)?;
        return Some(EmptyMarker { id, len: end + 1 });
    }

    let end = text.find('>')?;
    if end < 2 {
        return None;
    }
    let tag = &text[..=end];
    if !tag.ends_with("/>") {
        return None;
    }
    let name = &text[1..end - 1];
    if is_short_empty_marker_name(name) || is_short_paired_marker_name(name) {
        return Some(EmptyMarker {
            id: name.to_string(),
            len: end + 1,
        });
    }

    None
}

pub fn parse_marker_close(text: &str) -> Option<MarkerClose> {
    if !text.starts_with("</") {
        return None;
    }
    for tag_name in ["m", "keep"] {
        let close = format!("</{tag_name}>");
        if text.starts_with(&close) {
            return Some(MarkerClose {
                tag_name: tag_name.to_string(),
                len: close.len(),
            });
        }
    }

    let end = text.find('>')?;
    let name = &text[2..end];
    if is_short_paired_marker_name(name) {
        return Some(MarkerClose {
            tag_name: name.to_string(),
            len: end + 1,
        });
    }

    None
}

pub fn is_marker_token(text: &str) -> bool {
    let text = text.trim();
    parse_paired_marker_open(text).is_some_and(|marker| marker.len == text.len())
        || parse_empty_marker(text).is_some_and(|marker| marker.len == text.len())
        || parse_marker_close(text).is_some_and(|marker| marker.len == text.len())
}

pub fn strip_marker_tokens(text: &str) -> String {
    let mut output = String::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        output.push_str(&rest[..index]);
        let tag = &rest[index..];

        if let Some(open) = parse_paired_marker_open(tag) {
            rest = &tag[open.len..];
        } else if let Some(empty) = parse_empty_marker(tag) {
            rest = &tag[empty.len..];
        } else if let Some(close) = parse_marker_close(tag) {
            rest = &tag[close.len..];
        } else {
            output.push('<');
            rest = &tag[1..];
        }
    }

    output.push_str(rest);
    output
}

fn short_marker_name(tag: &str) -> Option<&str> {
    if let Some(open) = tag.strip_prefix("</") {
        let name = open.strip_suffix('>')?;
        return is_short_paired_marker_name(name).then_some(name);
    }
    let body = tag.strip_prefix('<')?.strip_suffix('>')?;
    let name = body.strip_suffix('/').unwrap_or(body);
    (is_short_paired_marker_name(name) || is_short_empty_marker_name(name)).then_some(name)
}

fn is_short_paired_marker_name(name: &str) -> bool {
    name.strip_prefix('m')
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_short_empty_marker_name(name: &str) -> bool {
    name.strip_prefix('r')
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

pub fn has_markers_in_expected_set(text: &str, expected: &HashSet<String>) -> bool {
    let actual_set: HashSet<String> = marker_ids_in_text(text).into_iter().collect();
    actual_set == *expected
}

pub fn all_markers_present(text: &str, required: &[String]) -> bool {
    required.iter().all(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_ids_include_short_and_legacy_markers() {
        let ids =
            marker_ids_in_text(r#"A <m1>bold <r1/> text</m1> and <m id="m000000_000">old</m>."#);

        assert_eq!(ids, vec!["m1", "r1", "m000000_000"]);
    }

    #[test]
    fn parses_short_marker_tokens() {
        let open = parse_paired_marker_open("<m12>text</m12>").expect("short paired marker");
        assert_eq!(open.tag_name, "m12");
        assert_eq!(open.id, "m12");
        assert_eq!(open.len, "<m12>".len());

        let empty = parse_empty_marker("<r3/>tail").expect("short empty marker");
        assert_eq!(empty.id, "r3");
        assert_eq!(empty.len, "<r3/>".len());

        let close = parse_marker_close("</m12>").expect("short close marker");
        assert_eq!(close.tag_name, "m12");
        assert_eq!(close.len, "</m12>".len());
    }

    #[test]
    fn strips_short_and_legacy_marker_tokens() {
        let stripped = strip_marker_tokens(
            r#"Hello <m1>wide <ref id="r000000_000"/> world</m1> and <m id="m000000_000">old</m>."#,
        );

        assert_eq!(stripped, "Hello wide  world and old.");
    }
}
