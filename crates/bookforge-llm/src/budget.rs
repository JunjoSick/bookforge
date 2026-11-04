//! Runtime request sizing. These estimates size one request against the
//! configured context window and output limits; they never change persisted
//! segment boundaries and are never substituted for provider-reported usage.

use crate::Rendered;

/// Estimated prompt tokens of a rendered request: system scaffold plus user
/// payload, measured with the canonical script-aware estimator. Everything
/// the template injected (style guide, entity table, glossary, prior context,
/// retry guidance) is included because it is measured after rendering.
pub(crate) fn estimate_rendered_tokens(rendered: &Rendered) -> usize {
    bookforge_core::segment::estimate_tokens(&rendered.system)
        .saturating_add(bookforge_core::segment::estimate_tokens(&rendered.user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_estimate_counts_both_messages_and_unspaced_text() {
        let rendered = Rendered {
            system: "a".repeat(400),
            user: "春".repeat(10),
        };
        assert_eq!(estimate_rendered_tokens(&rendered), 100 + 10);
    }
}
