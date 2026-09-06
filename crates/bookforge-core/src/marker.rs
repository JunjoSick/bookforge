use std::collections::{HashMap, HashSet};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerInnerText {
    pub id: String,
    pub text: String,
}

/// A reversible prompt-only view of marker-bearing prose.
///
/// The document IR keeps every marker. This projection removes only a directly
/// nested marker whose opening tag immediately follows its parent's opening
/// tag and whose closing tag immediately precedes its parent's closing tag.
/// Such a parent and child cover the same structural range. Responses can be
/// expanded back to the original marker tree with [`Self::restore`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerPromptProjection {
    pub text: String,
    omitted_ids: HashSet<String>,
    restorations: HashMap<String, MarkerRestoration>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkerRestoration {
    opens: String,
    closes: String,
}

impl MarkerPromptProjection {
    pub fn is_omitted(&self, id: &str) -> bool {
        self.omitted_ids.contains(id)
    }

    /// Restore markers omitted from the prompt around their retained parent.
    ///
    /// Callers still validate the expanded text against the original marker
    /// set. This method deliberately preserves every model-supplied byte and
    /// only injects the source marker tokens recorded by the projection.
    pub fn restore(&self, text: &str) -> String {
        if self.restorations.is_empty() {
            return text.to_string();
        }

        let mut output = String::with_capacity(
            text.len()
                + self
                    .restorations
                    .values()
                    .map(|restoration| restoration.opens.len() + restoration.closes.len())
                    .sum::<usize>(),
        );
        let mut stack = Vec::<(String, Option<&MarkerRestoration>)>::new();
        let mut rest = text;

        while let Some(index) = rest.find('<') {
            output.push_str(&rest[..index]);
            let tag = &rest[index..];

            if let Some(open) = parse_paired_marker_open(tag) {
                let restoration = self.restorations.get(&open.id);
                output.push_str(&tag[..open.len]);
                if let Some(restoration) = restoration {
                    output.push_str(&restoration.opens);
                }
                stack.push((open.tag_name, restoration));
                rest = &tag[open.len..];
            } else if let Some(empty) = parse_empty_marker(tag) {
                output.push_str(&tag[..empty.len]);
                rest = &tag[empty.len..];
            } else if let Some(close) = parse_marker_close(tag) {
                if let Some((open_name, restoration)) = stack.pop()
                    && open_name == close.tag_name
                    && let Some(restoration) = restoration
                {
                    output.push_str(&restoration.closes);
                }
                output.push_str(&tag[..close.len]);
                rest = &tag[close.len..];
            } else {
                output.push('<');
                rest = &tag[1..];
            }
        }

        output.push_str(rest);
        output
    }
}

#[derive(Debug)]
struct PairedMarkerRange {
    id: String,
    open_start: usize,
    open_end: usize,
    open_token: String,
    close_start: usize,
    close_end: usize,
    close_token: String,
    parent: Option<usize>,
}

/// Collapse directly nested paired markers that cover exactly the same range.
///
/// Marker IDs and the IR itself are untouched. The returned text is suitable
/// for a prompt, while [`MarkerPromptProjection::restore`] recreates the full
/// source marker nesting before validation, persistence, and EPUB rebuild.
pub fn collapse_nested_markers_for_prompt(text: &str) -> MarkerPromptProjection {
    if marker_structure_error(text).is_some() {
        return identity_projection(text);
    }

    let mut ranges = Vec::<PairedMarkerRange>::new();
    let mut stack = Vec::<(String, usize)>::new();
    let mut offset = 0usize;

    while offset < text.len() {
        let rest = &text[offset..];
        let Some(index) = rest.find('<') else {
            break;
        };
        let tag_start = offset + index;
        let tag = &text[tag_start..];

        if let Some(open) = parse_paired_marker_open(tag) {
            let range_index = ranges.len();
            ranges.push(PairedMarkerRange {
                id: open.id,
                open_start: tag_start,
                open_end: tag_start + open.len,
                open_token: tag[..open.len].to_string(),
                close_start: 0,
                close_end: 0,
                close_token: String::new(),
                parent: stack.last().map(|(_, index)| *index),
            });
            stack.push((open.tag_name, range_index));
            offset = tag_start + open.len;
        } else if let Some(empty) = parse_empty_marker(tag) {
            offset = tag_start + empty.len;
        } else if let Some(close) = parse_marker_close(tag) {
            let Some((open_name, range_index)) = stack.pop() else {
                return identity_projection(text);
            };
            if open_name != close.tag_name {
                return identity_projection(text);
            }
            ranges[range_index].close_start = tag_start;
            ranges[range_index].close_end = tag_start + close.len;
            ranges[range_index].close_token = tag[..close.len].to_string();
            offset = tag_start + close.len;
        } else {
            offset = tag_start + 1;
        }
    }

    if !stack.is_empty() {
        return identity_projection(text);
    }

    let mut unique_ids = HashSet::new();
    if ranges.iter().any(|range| !unique_ids.insert(&range.id)) {
        return identity_projection(text);
    }

    let mut identical_child = vec![None; ranges.len()];
    let mut omitted_ids = HashSet::new();
    for (child_index, child) in ranges.iter().enumerate() {
        let Some(parent_index) = child.parent else {
            continue;
        };
        let parent = &ranges[parent_index];
        if parent.open_end == child.open_start && child.close_end == parent.close_start {
            identical_child[parent_index] = Some(child_index);
            omitted_ids.insert(child.id.clone());
        }
    }

    if omitted_ids.is_empty() {
        return identity_projection(text);
    }

    let mut restorations = HashMap::new();
    for (root_index, root) in ranges.iter().enumerate() {
        if omitted_ids.contains(&root.id) {
            continue;
        }

        let mut child_index = identical_child[root_index];
        let mut opens = String::new();
        let mut closes = Vec::new();
        while let Some(index) = child_index {
            let child = &ranges[index];
            opens.push_str(&child.open_token);
            closes.push(child.close_token.as_str());
            child_index = identical_child[index];
        }
        if !opens.is_empty() {
            restorations.insert(
                root.id.clone(),
                MarkerRestoration {
                    opens,
                    closes: closes.into_iter().rev().collect(),
                },
            );
        }
    }

    let mut omitted_ranges = ranges
        .iter()
        .filter(|range| omitted_ids.contains(&range.id))
        .flat_map(|range| {
            [
                (range.open_start, range.open_end),
                (range.close_start, range.close_end),
            ]
        })
        .collect::<Vec<_>>();
    omitted_ranges.sort_unstable();

    let mut collapsed = String::with_capacity(text.len());
    let mut cursor = 0usize;
    for (start, end) in omitted_ranges {
        collapsed.push_str(&text[cursor..start]);
        cursor = end;
    }
    collapsed.push_str(&text[cursor..]);

    MarkerPromptProjection {
        text: collapsed,
        omitted_ids,
        restorations,
    }
}

fn identity_projection(text: &str) -> MarkerPromptProjection {
    MarkerPromptProjection {
        text: text.to_string(),
        omitted_ids: HashSet::new(),
        restorations: HashMap::new(),
    }
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

/// Inner text of every paired inline marker, with nested marker tags removed.
pub fn marker_inner_texts(text: &str) -> Vec<MarkerInnerText> {
    let mut inner_texts = Vec::new();
    let mut stack = Vec::<(String, String, usize)>::new();
    let mut offset = 0;

    while offset < text.len() {
        let rest = &text[offset..];
        let Some(index) = rest.find('<') else {
            break;
        };
        let tag_start = offset + index;
        let tag = &text[tag_start..];

        if let Some(open) = parse_paired_marker_open(tag) {
            offset = tag_start + open.len;
            stack.push((open.tag_name, open.id, offset));
        } else if let Some(empty) = parse_empty_marker(tag) {
            offset = tag_start + empty.len;
        } else if let Some(close) = parse_marker_close(tag) {
            offset = tag_start + close.len;
            if let Some(frame_index) = stack
                .iter()
                .rposition(|(tag_name, _, _)| tag_name == &close.tag_name)
            {
                for (_, id, content_start) in stack.drain(frame_index..) {
                    inner_texts.push(MarkerInnerText {
                        id,
                        text: strip_marker_tokens(&text[content_start..tag_start]),
                    });
                }
            }
        } else {
            offset = tag_start + 1;
        }
    }

    inner_texts
}

/// Paired-marker prose must remain free to change during translation. Only a
/// non-empty, at-most-eight-character token with no letters, at least one
/// digit or reference symbol, and otherwise only reference punctuation is
/// protected as non-translatable marker reference text.
pub fn marker_reference_token(text: &str) -> Option<&str> {
    let token = text.trim();
    let length = token.chars().count();
    if length == 0 || length > 8 || token.chars().any(char::is_alphabetic) {
        return None;
    }
    if !token
        .chars()
        .any(|ch| ch.is_numeric() || is_reference_symbol(ch))
    {
        return None;
    }
    token
        .chars()
        .all(|ch| {
            ch.is_numeric()
                || ch.is_ascii_whitespace()
                || is_reference_symbol(ch)
                || matches!(
                    ch,
                    '[' | ']'
                        | '('
                        | ')'
                        | '{'
                        | '}'
                        | '.'
                        | ','
                        | '-'
                        | '–'
                        | '—'
                        | '/'
                        | ':'
                        | ';'
                )
        })
        .then_some(token)
}

fn is_reference_symbol(ch: char) -> bool {
    matches!(ch, '*' | '†' | '‡' | '§' | '¶' | '#')
}

/// Return a deterministic error when paired inline markers are unbalanced,
/// mis-nested, or closed by the wrong marker tag. ID-presence checks alone
/// cannot detect `<m1>text` with a missing `</m1>`, which is valid prose JSON
/// but cannot be reassembled into the original inline structure.
pub fn marker_structure_error(text: &str) -> Option<String> {
    let mut stack = Vec::<PairedMarkerOpen>::new();
    let mut rest = text;

    while let Some(index) = rest.find('<') {
        let tag = &rest[index..];
        if let Some(open) = parse_paired_marker_open(tag) {
            let len = open.len;
            stack.push(open);
            rest = &tag[len..];
        } else if let Some(empty) = parse_empty_marker(tag) {
            rest = &tag[empty.len..];
        } else if let Some(close) = parse_marker_close(tag) {
            let Some(open) = stack.pop() else {
                return Some(format!(
                    "unexpected inline marker close </{}>",
                    close.tag_name
                ));
            };
            if open.tag_name != close.tag_name {
                return Some(format!(
                    "inline marker <{}> is closed by </{}>",
                    open.tag_name, close.tag_name
                ));
            }
            rest = &tag[close.len..];
        } else {
            rest = &tag[1..];
        }
    }

    stack.last().map(|open| {
        format!(
            "inline marker <{}> is missing closing tag </{}>",
            open.tag_name, open.tag_name
        )
    })
}

pub fn extract_marker_id_attr(tag: &str) -> Option<String> {
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

fn is_short_paired_marker_name(name: &str) -> bool {
    name.strip_prefix('m')
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

fn is_short_empty_marker_name(name: &str) -> bool {
    name.strip_prefix('r')
        .is_some_and(|suffix| !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_projection_collapses_only_identical_nested_ranges() {
        let source = "<m1><m2>eyes</m2></m1> and text";
        let projection = collapse_nested_markers_for_prompt(source);

        assert_eq!(projection.text, "<m1>eyes</m1> and text");
        assert!(projection.is_omitted("m2"));
        assert_eq!(projection.restore(&projection.text), source);
        assert_eq!(
            projection.restore("<m1>occhi</m1> e testo"),
            "<m1><m2>occhi</m2></m1> e testo"
        );
    }

    #[test]
    fn prompt_projection_preserves_nested_different_ranges() {
        let source = "<m1>wide <m2>narrow</m2> range</m1>";
        let projection = collapse_nested_markers_for_prompt(source);

        assert_eq!(projection.text, source);
        assert!(!projection.is_omitted("m2"));
        assert_eq!(projection.restore(source), source);
    }

    #[test]
    fn prompt_projection_restores_identical_marker_chains() {
        let projection = collapse_nested_markers_for_prompt("<m1><m2><m3>eyes</m3></m2></m1>");

        assert_eq!(projection.text, "<m1>eyes</m1>");
        assert!(projection.is_omitted("m2"));
        assert!(projection.is_omitted("m3"));
        assert_eq!(
            projection.restore("<m1>occhi</m1>"),
            "<m1><m2><m3>occhi</m3></m2></m1>"
        );
    }

    #[test]
    fn prompt_projection_restores_legacy_marker_tokens() {
        let source = r#"<m id="outer"><m id="inner">eyes</m></m>"#;
        let projection = collapse_nested_markers_for_prompt(source);

        assert_eq!(projection.text, r#"<m id="outer">eyes</m>"#);
        assert_eq!(projection.restore(&projection.text), source);
    }

    #[test]
    fn prompt_projection_does_not_collapse_duplicate_source_ids() {
        let source = "<m1><m1>eyes</m1></m1>";
        let projection = collapse_nested_markers_for_prompt(source);

        assert_eq!(projection.text, source);
        assert!(!projection.is_omitted("m1"));
    }

    #[test]
    fn marker_ids_include_short_and_legacy_markers() {
        let ids =
            marker_ids_in_text(r#"A <m1>bold <r1/> text</m1> and <m id="m000000_000">old</m>."#);

        assert_eq!(ids, vec!["m1", "r1", "m000000_000"]);
    }

    #[test]
    fn marker_inner_texts_strip_nested_marker_tags() {
        assert_eq!(
            marker_inner_texts("Parola.<m0><m1>*2</m1></m0> Segue."),
            vec![
                MarkerInnerText {
                    id: "m1".to_string(),
                    text: "*2".to_string(),
                },
                MarkerInnerText {
                    id: "m0".to_string(),
                    text: "*2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn marker_inner_texts_support_legacy_markers() {
        assert_eq!(
            marker_inner_texts(r#"A <m id="m000000_000">†3</m> B"#),
            vec![MarkerInnerText {
                id: "m000000_000".to_string(),
                text: "†3".to_string(),
            }]
        );
    }

    #[test]
    fn marker_inner_texts_ignore_empty_markers() {
        assert!(marker_inner_texts("A <r3/> B <m4/> C").is_empty());
    }

    #[test]
    fn marker_reference_tokens_are_narrowly_classified() {
        for token in ["1", "*2", "† 3", "[12]", "12–14", "١٢", "१२"] {
            assert_eq!(marker_reference_token(token), Some(token), "{token}");
        }
        for prose_or_data in ["", "beautiful", "E=mc^2", "42%", "123456789"] {
            assert_eq!(
                marker_reference_token(prose_or_data),
                None,
                "{prose_or_data}"
            );
        }
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

    #[test]
    fn marker_structure_accepts_balanced_nested_and_empty_markers() {
        assert_eq!(
            marker_structure_error("<m1>outer <m2>inner</m2><r1/></m1>"),
            None
        );
        assert_eq!(marker_structure_error(r#"<m id="legacy">text</m>"#), None);
    }

    #[test]
    fn marker_structure_rejects_missing_mismatched_and_orphan_closes() {
        assert!(
            marker_structure_error("<m1>text")
                .expect("missing close should fail")
                .contains("missing closing tag")
        );
        assert!(
            marker_structure_error("<m1><m2>text</m1></m2>")
                .expect("mis-nesting should fail")
                .contains("closed by")
        );
        assert!(
            marker_structure_error("text</m1>")
                .expect("orphan close should fail")
                .contains("unexpected")
        );
    }
}
