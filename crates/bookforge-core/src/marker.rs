use std::collections::HashSet;

pub fn marker_ids_in_text(text: &str) -> Vec<String> {
    let mut ids = Vec::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if (tag.starts_with("<m ") || tag.starts_with("<keep "))
            && let Some(end) = tag.find('>')
        {
            if let Some(id) = extract_marker_id(&tag[..=end]) {
                ids.push(id);
            }
            rest = &tag[end + 1..];
        } else if tag.starts_with("<ref ") {
            if let Some(end) = tag.find("/>") {
                if let Some(id) = extract_marker_id(&tag[..end + 2]) {
                    ids.push(id);
                }
                rest = &tag[end + 2..];
            } else {
                rest = &tag[1..];
            }
        } else {
            rest = &tag[1..];
        }
    }

    ids
}

pub fn extract_marker_id(tag: &str) -> Option<String> {
    let id_offset = tag.find("id=")? + 3;
    let quote = tag[id_offset..].chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value_start = id_offset + quote.len_utf8();
    let value_end = tag[value_start..].find(quote)? + value_start;
    Some(tag[value_start..value_end].to_string())
}

pub fn is_marker_token(text: &str) -> bool {
    let text = text.trim();
    text == "</m>"
        || text.starts_with("<m ")
        || text.starts_with("<keep ")
        || text.starts_with("<ref ")
}

pub fn has_markers_in_expected_set(text: &str, expected: &HashSet<String>) -> bool {
    let actual_set: HashSet<String> = marker_ids_in_text(text).into_iter().collect();
    actual_set == *expected
}

pub fn all_markers_present(text: &str, required: &[String]) -> bool {
    required.iter().all(|marker| text.contains(marker))
}
