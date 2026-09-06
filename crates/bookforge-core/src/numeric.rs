//! Shared numeric normalization; callers choose their own matching policy.

pub fn dangling_numeric_span(value: &str) -> bool {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    trimmed.ends_with('-') && trimmed.chars().any(|ch| ch.is_ascii_digit())
}

pub fn compact_numeric_punctuation_span(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    let digits = trimmed.chars().filter(|ch| ch.is_ascii_digit()).count();
    if digits < 2 {
        return None;
    }
    if !trimmed.chars().all(|ch| {
        ch.is_ascii_digit()
            || ch.is_ascii_whitespace()
            || matches!(
                ch,
                '.' | ',' | ';' | ':' | '/' | '-' | '+' | '%' | '$' | '(' | ')'
            )
    }) {
        return None;
    }
    Some(compact_ascii_whitespace(trimmed))
}

pub fn compact_ascii_whitespace(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect()
}

pub fn canonical_decimal_number(value: &str) -> Option<String> {
    let normalized = normalize_number_signs(value);
    let trimmed = normalized.trim_matches(|ch: char| {
        matches!(
            ch,
            ',' | ';' | ':' | '.' | '!' | '?' | '(' | ')' | '[' | ']' | '"' | '\''
        )
    });
    if trimmed.is_empty() || !trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        return None;
    }
    let percent = trimmed.ends_with('%');
    let numeric = trimmed.strip_suffix('%').unwrap_or(trimmed);
    if !numeric
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | ',' | '-' | '+'))
    {
        return None;
    }
    if numeric.matches('.').count() + numeric.matches(',').count() > 1 {
        return None;
    }
    let separator = numeric.find('.').or_else(|| numeric.find(','));
    let mut canonical = match separator {
        Some(index) => {
            let (whole, fractional_with_separator) = numeric.split_at(index);
            let fractional = &fractional_with_separator[1..];
            if whole.is_empty()
                || fractional.is_empty()
                || !whole
                    .trim_start_matches(['-', '+'])
                    .chars()
                    .all(|ch| ch.is_ascii_digit())
                || !fractional.chars().all(|ch| ch.is_ascii_digit())
            {
                return None;
            }
            format!("{whole}.{fractional}")
        }
        None => numeric.to_string(),
    };
    if percent {
        canonical.push('%');
    }
    Some(canonical)
}

pub fn normalize_number_signs(value: &str) -> String {
    value.chars().map(normalize_number_sign).collect()
}

pub fn normalize_number_sign(ch: char) -> char {
    match ch {
        '−' | '–' | '—' => '-',
        _ => ch,
    }
}
