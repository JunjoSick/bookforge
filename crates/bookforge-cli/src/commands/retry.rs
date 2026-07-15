use anyhow::Result;
use bookforge_store::{JobStore, RetryScope as StoreRetryScope};
use clap::{Args, ValueEnum};

const TOKI_PONA_RETRY_VOCABULARY: &str = "a, akesi, ala, alasa, ale, ali, anpa, ante, anu, awen, e, en, epiku, esun, ijo, ike, ilo, insa, jaki, jan, jasima, jelo, jo, kala, kalama, kama, kasi, ken, kepeken, kijetesantakalu, kili, kin, kipisi, kiwen, ko, kokosila, kon, ku, kule, kulupu, kute, la, lanpan, lape, laso, lawa, leko, len, lete, li, lili, linja, linluwi, lipu, loje, lon, luka, lukin, lupa, ma, mama, mani, meli, meso, mi, mije, misikeke, moku, moli, monsi, monsuta, mu, mun, musi, mute, n, namako, nanpa, nasa, nasin, nena, ni, nimi, noka, o, oko, olin, ona, open, pake, pakala, pali, palisa, pan, pana, pi, pilin, pimeja, pini, pipi, poka, poki, pona, pu, sama, seli, selo, seme, sewi, sijelo, sike, sin, sina, sinpin, sitelen, soko, sona, soweli, suli, suno, supa, suwi, tan, taso, tawa, telo, tenpo, toki, tomo, tonsi, tu, unpa, uta, utala, walo, wan, waso, wawa, weka, wile";

#[derive(Debug, Args)]
pub struct RetryArgs {
    pub job_id: String,

    #[arg(long, value_enum, default_value_t = RetryScope::Failed)]
    pub only: RetryScope,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RetryScope {
    Failed,
    NeedsReview,
    All,
}

pub async fn run(args: RetryArgs) -> Result<()> {
    let store = JobStore::open_default()?;
    let Some(job) = store.get_job(&args.job_id)? else {
        anyhow::bail!("job '{}' was not found", args.job_id);
    };

    let mut guided = 0usize;
    if job.target_lang.eq_ignore_ascii_case("Toki Pona")
        && matches!(args.only, RetryScope::NeedsReview | RetryScope::All)
    {
        for record in store.load_terminal_segment_translations(&args.job_id)? {
            if record.status != "needs_review" {
                continue;
            }
            let Some(error) = record.error.as_deref() else {
                continue;
            };
            let guidance = toki_pona_retry_guidance(error);
            store.request_segment_retry(&args.job_id, &record.segment_id, Some(&guidance))?;
            guided += 1;
        }
    }

    let count = guided + store.retry_segments(&args.job_id, args.only.into())?;
    println!("Job: {}", args.job_id);
    println!("Retry scope: {:?}", args.only);
    println!("Marked retry_pending: {count}");
    if guided > 0 {
        println!("Toki Pona error-guided retries: {guided}");
    }
    Ok(())
}

fn toki_pona_retry_guidance(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    let text_only = lower.contains("marker")
        || lower.contains("run count mismatch")
        || lower.contains("unapproved lowercase word")
        || lower.contains("italian function words");
    let prefix = if text_only {
        "[bookforge:text-only] Inline formatting failed previously; return plain translated text and let BookForge restore the inline template. "
    } else {
        ""
    };
    let concise_error = error.chars().take(900).collect::<String>();
    format!(
        "{prefix}Return a fresh, complete Toki Pona translation. Correct every listed validation failure. Translate quoted prose and citation titles too; preserve only protected numbers, URLs, labels, acronyms, and proper names exactly once. Do not copy Italian or English prose and do not repeat text to fill the response. Except for protected source labels and capitalized proper names, use only this closed lowercase vocabulary: {TOKI_PONA_RETRY_VOCABULARY}. Previous validation: {concise_error}"
    )
}

impl From<RetryScope> for StoreRetryScope {
    fn from(value: RetryScope) -> Self {
        match value {
            RetryScope::Failed => StoreRetryScope::Failed,
            RetryScope::NeedsReview => StoreRetryScope::NeedsReview,
            RetryScope::All => StoreRetryScope::All,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::toki_pona_retry_guidance;

    #[test]
    fn toki_retry_guidance_uses_text_only_for_shape_and_foreign_prose_failures() {
        let marker = toki_pona_retry_guidance("inline marker missing: m1");
        assert!(marker.contains("[bookforge:text-only]"));
        assert!(marker.contains("inline marker missing: m1"));

        let grammar = toki_pona_retry_guidance("pi must group at least two following words");
        assert!(!grammar.contains("[bookforge:text-only]"));
        assert!(grammar.contains("pi must group at least two following words"));

        let foreign = toki_pona_retry_guidance(
            "unapproved lowercase word in strict Toki Pona output: stages",
        );
        assert!(foreign.contains("[bookforge:text-only]"));
        assert!(foreign.contains("Translate quoted prose and citation titles too"));
    }
}
