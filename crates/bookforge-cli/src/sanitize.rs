//! Terminal-safety for externally sourced strings (UI-5).
//!
//! EPUB titles, chapter names, LLM/provider responses, and log-derived
//! warnings are attacker-controllable text. Printed verbatim to a terminal
//! they can rewrite window titles, cursor positions, or play escape games.
//! The ratatui TUI renders through a cell buffer and is immune; every other
//! terminal surface must route externally sourced strings through
//! [`sanitize_terminal`] (or [`sanitize_truncated`]) before printing.

/// Replace C0 control characters except `\n` and `\t`, all C1 controls
/// (including 0x9B, the single-byte CSI introducer), and DEL with `?`.
///
/// Newlines and tabs are preserved so multi-line diagnostics remain readable;
/// nothing else emitted by this crate relies on control sequences.
pub(crate) fn sanitize_terminal(input: &str) -> String {
    input
        .chars()
        .map(|c| match c {
            '\n' | '\t' => c,
            c if c.is_control() => '?',
            c => c,
        })
        .collect()
}

/// Sanitize and truncate to at most `max_chars` characters (char-boundary
/// safe), appending `…` when content was cut. Use for previews of unbounded
/// external payloads such as LLM responses or HTTP bodies.
pub(crate) fn sanitize_truncated(input: &str, max_chars: usize) -> String {
    let mut out = sanitize_terminal(input);
    if out.chars().count() > max_chars {
        out = out.chars().take(max_chars).collect();
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_titles_are_neutralized() {
        // OSC title overwrite + BEL terminator.
        assert_eq!(sanitize_terminal("\u{1b}]0;pwned\u{7}"), "?]0;pwned?");
        // SGR color escape / cursor movement.
        assert_eq!(sanitize_terminal("\u{1b}[31mRED\u{1b}[0m"), "?[31mRED?[0m");
        // C1 CSI (0x9B) bypass attempt.
        assert_eq!(sanitize_terminal("ok\u{9b}5Hmoved"), "ok?5Hmoved");
        // DEL and stray NUL.
        assert_eq!(sanitize_terminal("a\u{7f}b\u{0}c"), "a?b?c");
    }

    #[test]
    fn readable_whitespace_survives() {
        assert_eq!(sanitize_terminal("Chapter One\n\tII"), "Chapter One\n\tII");
        assert_eq!(sanitize_terminal(""), "");
    }

    #[test]
    fn normal_text_passes_through_verbatim() {
        let title = "Il Nome della Rosa — 该 Rose (naïve)";
        assert_eq!(sanitize_terminal(title), title);
    }

    #[test]
    fn truncation_is_char_safe_and_marks_the_cut() {
        let escaped = "\u{1b}]2;pwned\u{7}abcde";
        let preview = sanitize_truncated(escaped, 4);
        assert_eq!(preview, "?]2;…");
        // Never a torn char; the ellipsis only appears on an actual cut.
        assert!(sanitize_truncated("日本語テキスト", 3) == "日本語…");
        assert_eq!(sanitize_truncated("short", 10), "short");
    }
}
