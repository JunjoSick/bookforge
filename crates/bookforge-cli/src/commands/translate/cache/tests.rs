use super::*;
use bookforge_core::{
    ir::{BlockId, SectionId},
    segment::{SegmentBlock, SegmentId, SegmentMetadata, SegmentSource},
};
use bookforge_store::{CreateJob, JobRecord, SaveManualCorrection, SaveTranslation};

const SOURCE: &str = "This deliberately long English paragraph explains why the narrator returned to the village after the winter. The people had kept every letter and remembered the promises, but their circumstances had changed and they needed an answer before the next meeting.";
const TRANSLATED: &str = "Questo lungo paragrafo spiega perché il narratore tornò al villaggio dopo l'inverno. Gli abitanti avevano conservato ogni lettera e ricordavano le promesse, ma la situazione era cambiata e avevano bisogno di una risposta prima della riunione successiva.";

struct Fixture {
    _dir: tempfile::TempDir,
    store: JobStore,
    original: JobRecord,
    destination: JobRecord,
    segments: Vec<Segment>,
}

impl Fixture {
    /// An `original` job holding one saved provider result for a one-block
    /// segment, and a `destination` job that could reuse it from the cache.
    fn new(source: &str, translated: &str, provider: &str, target: &str) -> Self {
        Self::with_block(
            source,
            translated,
            provider,
            target,
            "paragraph",
            "Endnotes",
        )
    }

    fn with_block(
        source: &str,
        translated: &str,
        provider: &str,
        target: &str,
        kind: &str,
        section_title: &str,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.epub");
        std::fs::write(&input, b"synthetic cache fixture").unwrap();
        let store = JobStore::open(dir.path().join("jobs.sqlite")).unwrap();
        let make_job = |name: &str| {
            store
                .create_job(CreateJob {
                    input: &input,
                    output: &dir.path().join(name),
                    source_lang: Some("English"),
                    target_lang: target,
                    provider,
                    model: "fixture",
                    base_url: None,
                    api_key_env: None,
                    book_id: None,
                    series_id: None,
                })
                .unwrap()
        };
        let original = make_job("original.epub");
        let destination = make_job("destination.epub");
        let segments = vec![Segment {
            id: SegmentId("seg_a".into()),
            section_id: SectionId("notes".into()),
            ordinal: 0,
            block_ids: vec![BlockId("b_a".into())],
            source: SegmentSource {
                text: source.into(),
                token_estimate: 70,
                blocks: vec![SegmentBlock {
                    block_id: BlockId("b_a".into()),
                    kind: kind.into(),
                    text: source.into(),
                    text_runs: vec![],
                    protected_spans: vec![],
                }],
            },
            metadata: SegmentMetadata {
                section_title: Some(section_title.into()),
                ..Default::default()
            },
            context: Default::default(),
            constraints: Default::default(),
            checksum: "identical-source".into(),
        }];
        for job in [&original, &destination] {
            store
                .insert_segments(&job.id, &segments, "batch_v3", provider, "fixture", "ns")
                .unwrap();
        }
        store
            .save_translation(SaveTranslation {
                job_id: &original.id,
                segment_id: "seg_a",
                translated_text: translated,
                blocks: &[BlockTranslation {
                    block_id: BlockId("b_a".into()),
                    text: translated.into(),
                }],
                provider,
                model: "fixture",
                prompt_version: "batch_v3",
                input_tokens: Some(91),
                input_cached_tokens: Some(17),
                output_tokens: Some(61),
                tokens_estimated: false,
            })
            .unwrap();
        Self {
            _dir: dir,
            store,
            original,
            destination,
            segments,
        }
    }

    fn cache(&self) -> CacheContext<'_> {
        CacheContext {
            store: &self.store,
            job_id: &self.destination.id,
            prompt_version: "batch_v3",
            provider: &self.destination.provider,
            model: "fixture",
            source_lang: Some("English"),
            target_lang: &self.destination.target_lang,
            cache_namespace: "ns",
        }
    }

    fn apply_cache(&self) -> Vec<SegmentTranslation> {
        apply_cached_translations(&self.segments, &self.segments, self.cache()).unwrap()
    }

    fn revalidate(&self, job: &JobRecord) -> usize {
        revalidate_stored_translations(
            &self.store,
            &job.id,
            &self.segments,
            Some("English"),
            &job.target_lang,
        )
        .unwrap()
    }
}

#[test]
fn copied_prose_cache_hits_are_misses_without_modifying_the_original_job() {
    for prefix in ["", "p. 12. ", "The author writes in English: "] {
        let source = format!("{prefix}{SOURCE}");
        let fixture = Fixture::new(&source, &source, "deepseek", "Italian");
        let before = fixture
            .store
            .load_terminal_segment_translations(&fixture.original.id)
            .unwrap();

        assert!(fixture.apply_cache().is_empty(), "{prefix}");
        assert_eq!(
            fixture
                .store
                .resumable_segment_ids(&fixture.destination.id)
                .unwrap(),
            ["seg_a"]
        );
        assert!(
            fixture
                .store
                .load_block_translations(&fixture.destination.id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fixture
                .store
                .load_terminal_segment_translations(&fixture.original.id)
                .unwrap(),
            before
        );
    }
}

#[test]
fn translated_hits_still_reuse_cache_without_new_usage() {
    let fixture = Fixture::new(SOURCE, TRANSLATED, "deepseek", "Italian");
    let hits = fixture.apply_cache();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].status, SegmentStatus::SkippedCached);
    assert_eq!(hits[0].blocks[0].text, TRANSLATED);
    let summary = fixture
        .store
        .summary(&fixture.destination.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (summary.cached, summary.input_tokens, summary.output_tokens),
        (1, 0, 0)
    );
    assert_eq!(fixture.revalidate(&fixture.original), 0);
}

#[test]
fn copies_fresh_batch_output_may_keep_remain_reusable() {
    for (source, provider, target, kind, section) in [
        // A short citation in a reference section.
        (
            "Example Author, p. 12.",
            "deepseek",
            "Italian",
            "paragraph",
            "Endnotes",
        ),
        // A heading, which batch acceptance grades as an editorial warning.
        (
            "The Long Road Home",
            "deepseek",
            "Italian",
            "heading",
            "Chapter 1",
        ),
        // Runs where copy detection does not apply at all.
        (SOURCE, "mock", "Italian", "paragraph", "Chapter 1"),
        (SOURCE, "deepseek", "English", "paragraph", "Chapter 1"),
    ] {
        let fixture = Fixture::with_block(source, source, provider, target, kind, section);
        assert_eq!(fixture.apply_cache().len(), 1, "{source} ({kind})");
        assert_eq!(
            fixture.revalidate(&fixture.original),
            0,
            "{source} ({kind})"
        );
    }
}

#[test]
fn resume_flags_saved_copies_once_and_preserves_all_translation_evidence() {
    let fixture = Fixture::new(SOURCE, SOURCE, "deepseek", "Italian");
    let mut expected = fixture
        .store
        .load_terminal_segment_translations(&fixture.original.id)
        .unwrap();
    assert_eq!(fixture.revalidate(&fixture.original), 1);
    assert_eq!(fixture.revalidate(&fixture.original), 0);
    expected[0].status = "needs_review".into();
    expected[0].error = Some(
        "saved translation failed source-copy revalidation: block b_a: translation is \
         unchanged from the source-language prose"
            .into(),
    );
    assert_eq!(
        fixture
            .store
            .load_terminal_segment_translations(&fixture.original.id)
            .unwrap(),
        expected
    );
    assert!(
        fixture
            .store
            .resumable_segment_ids(&fixture.original.id)
            .unwrap()
            .is_empty(),
        "ordinary resume must not retranslate a flagged segment"
    );
    let summary = fixture
        .store
        .summary(&fixture.original.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            summary.needs_review,
            summary.input_tokens,
            summary.output_tokens
        ),
        (1, 91, 61)
    );
    let findings = fixture
        .store
        .segment_qa_findings(&fixture.original.id)
        .unwrap();
    assert!(findings.iter().any(|finding| {
        finding.kind == "source_copy_unchanged"
            && finding.severity == "error"
            && finding.block_id.as_deref() == Some("b_a")
    }));
    assert_eq!(
        fixture
            .store
            .get_job(&fixture.original.id)
            .unwrap()
            .unwrap()
            .status,
        "needs_review"
    );
}

#[test]
fn explicitly_accepted_unchanged_citations_survive_resume_and_stay_job_local() {
    let citation = "Example Author, A Deliberately Long Published Title Retained in Its Original Language: Essays on History, Memory, Literature and the Interpretation of Culture (London: Example Press, 2016), pages 425–452.";
    let fixture = Fixture::new(citation, citation, "deepseek", "Italian");
    assert_eq!(
        fixture.revalidate(&fixture.original),
        1,
        "unreviewed long citations require review"
    );
    fixture
        .store
        .save_manual_correction(SaveManualCorrection {
            job_id: &fixture.original.id,
            segment_id: "seg_a",
            translated_text: citation,
            blocks: &[BlockTranslation {
                block_id: BlockId("b_a".into()),
                text: citation.into(),
            }],
        })
        .unwrap();
    let accepted = fixture
        .store
        .load_terminal_segment_translations(&fixture.original.id)
        .unwrap();
    assert!(accepted[0].human_corrected);
    assert_eq!(accepted[0].status, "succeeded");
    assert_eq!(fixture.revalidate(&fixture.original), 0);
    assert_eq!(
        fixture
            .store
            .load_terminal_segment_translations(&fixture.original.id)
            .unwrap(),
        accepted
    );
    assert!(
        fixture.apply_cache().is_empty(),
        "a reviewer's acceptance is not shared across jobs"
    );
}

#[test]
fn resume_revalidates_results_restored_from_the_cache_too() {
    let fixture = Fixture::new(SOURCE, SOURCE, "deepseek", "Italian");
    fixture
        .store
        .save_cached_translation(SaveCachedTranslation {
            job_id: &fixture.destination.id,
            segment_id: "seg_a",
            translated_text: SOURCE,
            blocks: &[BlockTranslation {
                block_id: BlockId("b_a".into()),
                text: SOURCE.into(),
            }],
            provider: "deepseek",
            model: "fixture",
            prompt_version: "batch_v3",
        })
        .unwrap();
    assert_eq!(fixture.revalidate(&fixture.destination), 1);
    assert_eq!(
        fixture
            .store
            .segment_records(&fixture.destination.id)
            .unwrap()[0]
            .status,
        "needs_review"
    );
}
