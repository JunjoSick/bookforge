//! Shared math-token classification used by EPUB and PDF ingestion.

pub fn is_inline_math_operator(ch: char) -> bool {
    matches!(
        ch,
        '=' | '+'
            | '-'
            | '*'
            | '/'
            | '^'
            | '_'
            | '('
            | ')'
            | '['
            | ']'
            | '{'
            | '}'
            | '|'
            | '<'
            | '>'
            | '\u{2211}'
            | '\u{222b}'
            | '\u{221a}'
            | '\u{2264}'
            | '\u{2265}'
            | '\u{2248}'
            | '\u{2260}'
            | '\u{00b1}'
            | '\u{00d7}'
            | '\u{00f7}'
            | '\u{2202}'
            | '\u{2207}'
            | '\u{221e}'
            | '\u{2208}'
    )
}

pub fn is_strong_inline_math_operator(ch: char) -> bool {
    is_inline_math_operator(ch)
        && !matches!(ch, '_' | '-' | '(' | ')' | '[' | ']' | '{' | '}' | '|')
}
