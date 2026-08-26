//! The one canonical token estimator used across BookForge.
//!
//! Every subsystem that must reason about model tokens before sending a
//! request — segmentation, batch packing, scheduler context budgets,
//! glossary selection, QA/double-check chunking, cost reporting — measures
//! text through [`estimate_tokens`]. Before the audit this lived as eight
//! divergent private helpers (chars/4, bytes/4, words×4/3, word count), so
//! the same segment could be sized differently by batch packing than by
//! glossary budgeting and none of them agreed with printed costs.
//!
//! # Formula
//!
//! One pass over the characters; each character contributes an integer
//! weight in quarters of a token:
//!
//! * Characters of unspaced scripts ([`crate::script::is_unspaced_script`]:
//!   Han ideographs, Kana, Hangul) weigh `4/4` — approximately one model
//!   token per character. These scripts do not delimit words with spaces
//!   (`script::is_space_delimited` is false for their prose) and BPE-style
//!   vocabularies spend roughly one whole token per character on them.
//! * Every other character weighs `1/4`. For space-delimited alphabetic
//!   prose this reproduces the classic four-characters-per-token rule of
//!   thumb for English BPE tokenizers, and it intentionally counts spaces,
//!   digits and punctuation too, so the estimate covers the entire payload.
//!
//! The total is the ceiling of the summed weights divided by four:
//!
//! ```text
//! tokens = ceil((4 * unspaced_chars + other_chars) / 4)
//! ```
//!
//! All arithmetic is integer, single-pass over the input, and allocation
//! free. Because weights are per-character additive, the estimate is
//! additive up to per-piece ceiling rounding: summing estimates of pieces
//! never undercounts an estimate of their concatenation.
//!
//! # Why proportion weighting instead of dominant-class classification
//!
//! The script module separates two axes deliberately (see its crate docs):
//! cased/caseless and spaced/unspaced. Classifying a whole text by its
//! dominant case class — the pre-audit behaviour — mispriced genuinely
//! mixed books at one end (a Latin-titled Chinese book was counted as pure
//! CJK, inflating it ~4×) and mispriced caseless-but-spaced languages at
//! the other (Arabic and Hebrew are caseless yet space-delimited, so they
//! belong on the spaced-script side). Weighting each character by its own
//! script makes mixed-script text follow its counted proportions and keeps
//! the CJK coefficient confined to the scripts that actually lack word
//! spacing. [`ScriptClass`] and [`script_counts`] remain available for the
//! callers whose decision really is about case handling, not size.
//!
//! Undetermined inputs fall out naturally: text without alphabetic evidence
//! (digits, punctuation) weighs everything at ¼, matching the historical
//! fallback rather than guessing a script.

/// Quarters-of-token contributed by one character of an unspaced script
/// (Han, Kana, Hangul): approximately one whole token per character.
pub const UNSPACED_SCRIPT_WEIGHT_X4: usize = 4;

/// Quarters-of-token contributed by every other character: one quarter of
/// a token, i.e. four characters per token including spaces and punctuation.
pub const SPACED_DEFAULT_WEIGHT_X4: usize = 1;

const WEIGHT_DIVISOR: usize = 4;

/// Estimate the number of model tokens needed to represent `text`.
///
/// See the [module documentation](self) for the coefficients and why they
/// are weighted per character. Empty input estimates to zero.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut weight_x4 = 0usize;
    for ch in text.chars() {
        if crate::script::is_unspaced_script(ch) {
            weight_x4 += UNSPACED_SCRIPT_WEIGHT_X4;
        } else {
            weight_x4 += SPACED_DEFAULT_WEIGHT_X4;
        }
    }
    weight_x4.div_ceil(WEIGHT_DIVISOR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_text_is_zero_tokens() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn english_prose_falls_back_to_four_chars_per_token() {
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("The quick brown fox."), 5);
        assert_eq!(estimate_tokens("1234"), 1);
    }

    #[test]
    fn pure_unspaced_scripts_count_one_token_per_char() {
        // Han: eight characters -> eight tokens, not the ~2 a naive
        // chars/4 would claim (the ~2 is the inflate-4x in reverse; either
        // direction of confusion made planning unreliable).
        assert_eq!(estimate_tokens("矛盾是普遍存在的"), 8);
        assert_eq!(estimate_tokens("実験は成功した"), 7);
        assert_eq!(estimate_tokens("한국어문장입니다"), 8);
    }

    #[test]
    fn caseless_but_space_delimited_stays_on_the_spaced_side() {
        // Arabic and Hebrew are caseless yet write with spaces; PR #108's
        // split between the two axes exists precisely so they are not
        // priced like Han. They keep the four-chars-per-token fallback.
        let arabic = "العربية لغة جميلة";
        let hebrew = "עברית שפה יפה";
        assert_eq!(estimate_tokens(arabic), arabic.chars().count().div_ceil(4));
        assert_eq!(estimate_tokens(hebrew), hebrew.chars().count().div_ceil(4));
    }

    #[test]
    fn mixed_script_weights_follow_counted_proportions() {
        // 18 Latin characters (incl. separator) at 1/4 plus 18 Han
        // characters at 1 => ceil(18/4 + 18) = ceil(22.5) = 23 tokens.
        let mixed = "Project Gutenberg 矛盾是普遍存在的实践是检验真理的标准";
        let unspaced = mixed
            .chars()
            .filter(|ch| crate::script::is_unspaced_script(*ch))
            .count();
        let other = mixed.chars().count() - unspaced;
        assert_eq!(unspaced, 18);
        assert_eq!(other, 18);
        assert_eq!(estimate_tokens(mixed), 23);
    }

    #[test]
    fn additivity_only_ever_rounds_up() {
        let parts = ["Hello ", "world ", "矛盾", "の物語"];
        let joined = parts.concat();
        let summed: usize = parts.iter().map(|part| estimate_tokens(part)).sum();
        assert!(summed >= estimate_tokens(&joined));
        // Whole-text estimate stays close to the piecewise sum (within one
        // rounding unit per piece).
        assert!(summed - estimate_tokens(&joined) < parts.len());
    }

    #[test]
    fn cjk_only_text_is_neither_inflated_nor_deflated_by_shared_fences() {
        // Markers and markup around an East-Asian payload weigh the payload
        // at one token per char and the scaffolding at one quarter.
        let fenced = "<p>矛盾論</p>";
        let han: usize = fenced
            .chars()
            .filter(|ch| crate::script::is_unspaced_script(*ch))
            .count();
        assert_eq!(han, 3);
        assert_eq!(estimate_tokens(fenced), fenced.chars().count() / 4 + 3);
    }

    #[test]
    fn long_cjk_segment_estimates_above_naive_chars_over_four() {
        // The worst case that motivated DUP-1/LLM-5: a segment estimated at
        // chars/4 packs into batches sized ~4x too small for the tokens the
        // provider will actually charge, causing truncation churn.
        let long = "矛盾是普遍存在的，而特殊性寓于普遍性之中。".repeat(20);
        let naive_chars_per_four = long.chars().count() / 4;
        assert!(estimate_tokens(&long) > naive_chars_per_four);
    }
}
