use super::*;
use bookforge_core::{
    ir::{ProtectedSpan, ProtectedSpanKind},
    segment::{
        SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
        SegmentSource, SegmentTextRun,
    },
};

fn make_segment(id: &str, blocks: Vec<SegmentBlock>, markers: Vec<String>) -> Segment {
    make_segment_in_section(id, "sec_000000", 0, blocks, markers)
}

fn make_segment_in_section(
    id: &str,
    section_id: &str,
    ordinal: usize,
    blocks: Vec<SegmentBlock>,
    markers: Vec<String>,
) -> Segment {
    Segment {
        id: SegmentId(id.to_string()),
        section_id: bookforge_core::ir::SectionId(section_id.to_string()),
        ordinal,
        block_ids: blocks.iter().map(|b| b.block_id.clone()).collect(),
        source: SegmentSource {
            text: blocks
                .iter()
                .map(|b| b.text.clone())
                .collect::<Vec<_>>()
                .join("\n"),
            blocks,
            token_estimate: 50,
        },
        context: SegmentContext::default(),
        metadata: SegmentMetadata::default(),
        constraints: SegmentConstraints {
            preserve_markers: markers,
            ..Default::default()
        },
        checksum: format!("checksum_{id}"),
    }
}

fn plain_block(text: &str) -> SegmentBlock {
    SegmentBlock {
        block_id: bookforge_core::ir::BlockId(text.to_string()),
        kind: "paragraph".to_string(),
        text: text.to_string(),
        text_runs: vec![SegmentTextRun {
            id: "r0".to_string(),
            text: text.to_string(),
        }],
        protected_spans: Vec::new(),
    }
}

fn protected_block(text: &str, spans: Vec<String>) -> SegmentBlock {
    SegmentBlock {
        block_id: bookforge_core::ir::BlockId(text.to_string()),
        kind: "paragraph".to_string(),
        text: text.to_string(),
        text_runs: vec![SegmentTextRun {
            id: "r0".to_string(),
            text: text.to_string(),
        }],
        protected_spans: spans
            .into_iter()
            .map(|text| ProtectedSpan {
                kind: ProtectedSpanKind::Number,
                text,
            })
            .collect(),
    }
}

fn single_item_batch_with_protected_span(span: &str) -> TranslationBatch {
    let seg = make_segment(
        "seg1",
        vec![protected_block("Protected number", vec![span.to_string()])],
        vec![],
    );
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    build_translation_batches(&[seg], &config, TranslationProfile::Balanced)
        .into_iter()
        .next()
        .expect("single batch")
}

fn batch_item(id: &str, source_text: &str) -> TranslationBatchItem {
    TranslationBatchItem {
        item_id: id.to_string(),
        segment_id: SegmentId(format!("seg_{id}")),
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
        block_id: bookforge_core::ir::BlockId(format!("block_{id}")),
        ordinal: 0,
        kind: "paragraph".to_string(),
        source_text: source_text.to_string(),
        text_runs: Vec::new(),
        protected_spans: Vec::new(),
        required_markers: Vec::new(),
        checksum: format!("checksum_{id}"),
    }
}

fn run_preserving_batch_with_runs(run_texts: &[&str]) -> TranslationBatch {
    let mut item = batch_item("runs", &run_texts.join(""));
    item.text_runs = run_texts
        .iter()
        .enumerate()
        .map(|(index, text)| SegmentTextRun {
            id: format!("r{index}"),
            text: (*text).to_string(),
        })
        .collect();
    TranslationBatch {
        id: "run-preserving".to_string(),
        ordinal: 0,
        mode: BatchMode::RunPreserving,
        kind: BatchKind::Translation,
        token_estimate: 100,
        items: vec![item.clone()],
        section_id: item.section_id,
    }
}

#[test]
fn plain_blocks_batch_together() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello world")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye world")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].items.len(), 2);
}

#[test]
fn expected_output_bound_is_opt_in_and_only_splits_the_oversized_batch() {
    let oversized = make_segment_in_section(
        "oversized",
        "sec_oversized",
        0,
        (0..45)
            .map(|index| plain_block(&format!("{index:02}:{}", "x".repeat(768))))
            .collect(),
        vec![],
    );
    let ordinary = make_segment_in_section(
        "ordinary",
        "sec_ordinary",
        1,
        (0..19)
            .map(|index| plain_block(&format!("{index:02}: small")))
            .collect(),
        vec![],
    );
    let config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let initial = build_translation_batches(
        &[oversized, ordinary],
        &config,
        TranslationProfile::Balanced,
    );
    assert_eq!(initial.len(), 2);

    let without_bound =
        account_for_batch_prompt_overhead(initial.clone(), &config, &test_run_config());
    assert_eq!(
        without_bound
            .iter()
            .filter(|batch| batch.section_id.0 == "sec_oversized")
            .count(),
        1,
        "no expected-output bound is enabled by default"
    );

    let mut bounded_config = test_run_config();
    bounded_config.batch_max_output_tokens = Some(8_000);
    let bounded = account_for_batch_prompt_overhead(initial, &config, &bounded_config);
    assert!(
        bounded
            .iter()
            .filter(|batch| batch.section_id.0 == "sec_oversized")
            .count()
            > 1,
        "the measured 45-block response shape should be repacked"
    );
    assert_eq!(
        bounded
            .iter()
            .filter(|batch| batch.section_id.0 == "sec_ordinary")
            .count(),
        1,
        "ordinary small batches must not be shrunk by the output bound"
    );
    assert_eq!(
        bounded.iter().map(|batch| batch.items.len()).sum::<usize>(),
        64,
        "repacking must preserve every item"
    );
}

#[test]
fn batch_construction_uses_only_block_local_markers() {
    let seg = make_segment(
        "seg1",
        vec![plain_block("<m1>Marked</m1>"), plain_block("Plain sibling")],
        vec!["m1".to_string()],
    );
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };

    let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
    let items = batches
        .iter()
        .flat_map(|batch| batch.items.iter())
        .collect::<Vec<_>>();
    let marked = items
        .iter()
        .find(|item| item.source_text.contains("Marked"))
        .expect("marked block");
    let plain = items
        .iter()
        .find(|item| item.source_text.contains("Plain sibling"))
        .expect("plain block");

    assert_eq!(marked.required_markers, vec!["m1"]);
    assert!(plain.required_markers.is_empty());
    assert_eq!(plain.mode(), BatchMode::Plain);
}

#[test]
fn batches_never_cross_section_boundaries() {
    // PR5 invariant: build_translation_batches must partition by section
    // before grouping by token budget, so sliding-context awaiting in
    // batch mode can't deadlock on a sibling item in the same batch.
    let seg_a1 = make_segment_in_section("a1", "sec_A", 0, vec![plain_block("Alpha one")], vec![]);
    let seg_a2 = make_segment_in_section("a2", "sec_A", 1, vec![plain_block("Alpha two")], vec![]);
    let seg_b1 = make_segment_in_section("b1", "sec_B", 2, vec![plain_block("Bravo one")], vec![]);
    let seg_b2 = make_segment_in_section("b2", "sec_B", 3, vec![plain_block("Bravo two")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 100_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(
        &[seg_a1, seg_a2, seg_b1, seg_b2],
        &config,
        TranslationProfile::Balanced,
    );
    // Token budget could fit all four in one batch — but section
    // partitioning forces two batches, one per section.
    assert_eq!(batches.len(), 2);
    for batch in &batches {
        let section_set: std::collections::HashSet<&str> = batch
            .items
            .iter()
            .map(|item| item.section_id.0.as_str())
            .collect();
        assert_eq!(
            section_set.len(),
            1,
            "batch {} mixes sections: {:?}",
            batch.id,
            section_set
        );
        // Batch.section_id matches its items'.
        assert_eq!(
            batch.section_id.0, batch.items[0].section_id.0,
            "batch.section_id must match its items"
        );
    }
}

#[test]
fn batches_emerge_in_input_order_across_sections() {
    // build_translation_batches respects the input order of `segments`
    // (which `build_segments` produces in document order). The dispatcher
    // pulls batches FIFO from the queue, so earlier-input sections get
    // dispatched first.
    let seg_a = make_segment_in_section("a", "sec_A", 0, vec![plain_block("Alpha")], vec![]);
    let seg_b = make_segment_in_section("b", "sec_B", 1, vec![plain_block("Bravo")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 100_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg_a, seg_b], &config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].section_id.0, "sec_A");
    assert_eq!(batches[1].section_id.0, "sec_B");
}

#[test]
fn batch_sizer_reduces_after_truncation() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_truncation_for_mode(BatchMode::Plain);
    assert_eq!(sizer.target_tokens(), 10_400);
    assert_eq!(sizer.max_items(), 96);
}

#[test]
fn batch_sizer_reduces_after_invalid_json() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_invalid_json_for_mode(BatchMode::Plain);
    assert_eq!(sizer.target_tokens(), 12_000);
    assert_eq!(sizer.max_items(), 108);
}

#[test]
fn batch_sizer_reduces_after_high_latency() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_high_latency_for_mode(BatchMode::Plain, 40_000);
    assert_eq!(sizer.target_tokens(), 13_600);
    assert_eq!(sizer.max_items(), 128);
}

#[test]
fn batch_sizer_increases_after_stable_success() {
    let mut sizer = BatchSizer::new(16_000, 128);
    for _ in 0..20 {
        sizer.on_success_for_mode(BatchMode::Plain, 100);
    }
    assert_eq!(sizer.target_tokens(), 17_600);
    assert_eq!(sizer.max_items(), 144);
}

#[test]
fn batch_sizer_does_not_grow_after_single_success() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_success_for_mode(BatchMode::Plain, 100);
    assert_eq!(sizer.target_tokens(), 16_000);
    assert_eq!(sizer.max_items(), 128);
}

#[test]
fn batch_sizer_does_not_grow_when_recent_invalid_json_exists() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_invalid_json_for_mode(BatchMode::Plain);
    let after_failure = sizer.target_tokens();
    for _ in 0..19 {
        sizer.on_success_for_mode(BatchMode::Plain, 100);
    }
    assert_eq!(sizer.target_tokens(), after_failure);
}

#[test]
fn batch_sizer_does_not_grow_when_recent_truncation_exists() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_truncation_for_mode(BatchMode::Plain);
    let after_failure = sizer.target_tokens();
    for _ in 0..19 {
        sizer.on_success_for_mode(BatchMode::Plain, 100);
    }
    assert_eq!(sizer.target_tokens(), after_failure);
}

#[test]
fn batch_sizer_does_not_grow_when_p95_latency_is_high() {
    let mut sizer = BatchSizer::new(16_000, 128);
    for _ in 0..18 {
        sizer.on_success_for_mode(BatchMode::Plain, 100);
    }
    sizer.on_success_for_mode(BatchMode::Plain, 40_000);
    assert!(sizer.target_tokens() < 16_000);
}

#[test]
fn one_slow_outlier_does_not_immediately_shrink_if_window_p95_healthy() {
    let mut sizer = BatchSizer::new(16_000, 128);
    for _ in 0..19 {
        sizer.on_success_for_mode(BatchMode::Plain, 100);
    }
    sizer.on_success_for_mode(BatchMode::Plain, 40_000);
    assert_eq!(sizer.target_tokens(), 17_600);
}

#[test]
fn batch_sizer_keeps_independent_plain_and_run_preserving_state() {
    let mut sizer = BatchSizer::new(16_000, 128);
    let plain_before = sizer.target_tokens_for_mode(BatchMode::Plain);
    let run_before = sizer.target_tokens_for_mode(BatchMode::RunPreserving);

    sizer.on_invalid_json_for_mode(BatchMode::RunPreserving);

    assert_eq!(sizer.target_tokens_for_mode(BatchMode::Plain), plain_before);
    assert!(sizer.target_tokens_for_mode(BatchMode::RunPreserving) < run_before);
}

#[test]
fn marker_safe_clamp_does_not_affect_turbo_target() {
    let mut sizer = BatchSizer::new(32_000, 256);
    let turbo_before = sizer.target_tokens_for_mode(BatchMode::TurboTextOnly);

    sizer.on_truncation_for_mode(BatchMode::MarkerSafe);

    assert_eq!(
        sizer.target_tokens_for_mode(BatchMode::TurboTextOnly),
        turbo_before
    );
    assert!(sizer.target_tokens_for_mode(BatchMode::MarkerSafe) < 16_000);
}

#[test]
fn toki_pona_repairs_are_single_item_to_bound_output_and_isolate_failures() {
    assert_eq!(repair_batch_item_limit("Toki Pona"), 1);
    assert_eq!(repair_batch_item_limit("toki pona"), 1);
    assert_eq!(repair_batch_item_limit("Italian"), 16);
}

#[test]
fn batch_sizer_respects_plain_mode_clamps() {
    let sizer = BatchSizer::new(64_000, 512);
    assert_eq!(sizer.target_tokens_for_mode(BatchMode::Plain), 32_000);
    assert_eq!(sizer.max_items_for_mode(BatchMode::Plain), 256);
}

#[test]
fn batch_sizer_respects_marker_safe_clamps() {
    let sizer = BatchSizer::new(64_000, 512);
    assert_eq!(sizer.target_tokens_for_mode(BatchMode::MarkerSafe), 16_000);
    assert_eq!(sizer.max_items_for_mode(BatchMode::MarkerSafe), 128);
}

#[test]
fn batch_sizer_respects_run_preserving_clamps() {
    let sizer = BatchSizer::new(64_000, 512);
    assert_eq!(
        sizer.target_tokens_for_mode(BatchMode::RunPreserving),
        8_000
    );
    assert_eq!(sizer.max_items_for_mode(BatchMode::RunPreserving), 64);
}

#[test]
fn repack_batch_preserves_item_order_and_ids() {
    let batch = TranslationBatch {
        id: "batch".to_string(),
        ordinal: 7,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        token_estimate: 100,
        items: vec![
            batch_item("a", "one two three four"),
            batch_item("b", "five six seven eight"),
            batch_item("c", "nine ten eleven twelve"),
        ],
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    let parts = repack_batch(batch, 1, 2);
    let ids = parts
        .iter()
        .flat_map(|part| part.items.iter().map(|item| item.item_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["a", "b", "c"]);
    assert!(parts.iter().all(|part| part.items.len() <= 2));
}

#[test]
fn batch_output_budget_accounts_for_many_short_json_items() {
    let items = (0..13)
        .map(|index| batch_item(&format!("item-{index}"), "label"))
        .collect::<Vec<_>>();
    let batch = TranslationBatch {
        id: "short-labels".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        token_estimate: 52,
        items,
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    let budget =
        batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, false, None, None);

    assert!(
        budget >= 1_000,
        "per-item JSON overhead should prevent a 512-token under-budget, got {budget}"
    );
}

#[test]
fn deepseek_batches_can_use_extended_output_budget() {
    let batch = TranslationBatch {
        id: "large".to_string(),
        ordinal: 0,
        mode: BatchMode::RunPreserving,
        kind: BatchKind::Translation,
        token_estimate: 6_000,
        items: (0..30)
            .map(|index| batch_item(&format!("item-{index}"), "longer source text"))
            .collect(),
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    assert_eq!(
        batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, false, None, None,),
        16_384
    );
    assert!(
        batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, true, None, None,)
            > 16_384
    );
}

#[test]
fn highly_expansive_target_gets_safe_initial_batch_output_budget() {
    let batch = TranslationBatch {
        id: "expansive-target".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        token_estimate: 1_500,
        items: vec![batch_item("item", "source text")],
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    let ordinary =
        batch_max_output_tokens(&batch, TranslationProfile::V1Fast, false, true, None, None);
    let toki_pona = batch_max_output_tokens(
        &batch,
        TranslationProfile::V1Fast,
        false,
        true,
        Some(20),
        Some(4_096),
    );
    assert!(toki_pona > ordinary);
    assert!(toki_pona >= 30_000);
}

#[test]
fn highly_expansive_target_has_safe_floor_for_short_batches() {
    let batch = TranslationBatch {
        id: "short-expansive-target".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        token_estimate: 226,
        items: vec![batch_item(
            "item",
            "short but semantically dense source text",
        )],
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    let toki_pona = batch_max_output_tokens(
        &batch,
        TranslationProfile::V1Fast,
        false,
        true,
        Some(20),
        Some(4_096),
    );

    assert_eq!(toki_pona, 4_712);
}

#[test]
fn batch_sizer_shrink_affects_later_pending_batches() {
    let mut sizer = BatchSizer::new(16_000, 128);
    sizer.on_truncation_for_mode(BatchMode::MarkerSafe);
    let batch = TranslationBatch {
        id: "batch".to_string(),
        ordinal: 0,
        mode: BatchMode::MarkerSafe,
        kind: BatchKind::Translation,
        token_estimate: 80_000,
        items: (0..32)
            .map(|idx| batch_item(&format!("{idx}"), &"word ".repeat(2_000)))
            .collect(),
        section_id: bookforge_core::ir::SectionId("test_section".to_string()),
    };

    let normalized = normalize_batch_for_current_sizer(batch, Some(&sizer), None);
    assert!(normalized.len() > 1);
    assert!(normalized.iter().all(|part| {
        part.token_estimate <= sizer.target_tokens_for_mode(BatchMode::MarkerSafe)
            && part.items.len() <= sizer.max_items_for_mode(BatchMode::MarkerSafe)
    }));
}

#[test]
fn request_status_maps_5xx_to_server_error() {
    let status =
        request_status_for_controller::<BatchTranslationResult>(&Err(LlmError::HttpStatus {
            status: 503,
            body: "unavailable".to_string(),
        }));
    assert_eq!(status, RequestStatus::ServerError);
}

#[test]
fn parses_valid_batch_response() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
    let batch = &batches[0];
    let id1 = &batch.items[0].item_id;
    let id2 = &batch.items[1].item_id;

    let response = serde_json::json!({
        "items": [
            {"id": id1, "translation": "Ciao mondo"},
            {"id": id2, "translation": "Addio mondo"},
        ]
    })
    .to_string();

    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 2);
    assert_eq!(result.failures.len(), 0);
}

#[test]
fn missing_short_ordinal_succeeds_with_warning_instead_of_appending() {
    let seg = make_segment(
        "seg1",
        vec![protected_block("Chapter 4th", vec!["4th".to_string()])],
        vec!["<bf:keep/>".to_string()],
    );
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
    let batch = &batches[0];
    let id = &batch.items[0].item_id;

    let response = serde_json::json!({
        "items": [
            {"id": id, "translation": "Capitolo"},
        ]
    })
    .to_string();

    // A short ordinal is a soft Number violation: keep the model output,
    // surface the warning, and never glue the source token onto the text.
    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 1);
    assert_eq!(result.failures.len(), 0);
    assert_eq!(result.translations[0].text, "Capitolo");
    assert!(
        result.translations[0]
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("protected span missing: 4th")),
        "got: {:?}",
        result.translations[0].warning
    );
}

#[test]
fn intact_protected_span_passes_batch_validation_unmodified() {
    let seg = make_segment(
        "seg1",
        vec![protected_block("Chapter 4th", vec!["4th".to_string()])],
        // Segment-wide marker list intentionally names a marker that is
        // NOT in this block's source; per-block validation must not
        // demand it (and must never append it).
        vec!["<bf:keep/>".to_string()],
    );
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg], &config, TranslationProfile::Balanced);
    let batch = &batches[0];
    let id = &batch.items[0].item_id;

    let response = serde_json::json!({
        "items": [
            {"id": id, "translation": "Capitolo 4th"},
        ]
    })
    .to_string();

    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.failures.len(), 0);
    assert_eq!(result.translations.len(), 1);
    assert_eq!(
        result.translations[0].text, "Capitolo 4th",
        "translation must pass through without appended tokens"
    );
}

#[test]
fn localized_numeric_protected_spans_pass_batch_validation() {
    for (span, translation) in [
        ("0.1", "diametro da 0,1 a 1 mm"),
        ("-63.5", "il potenziale era circa –63,5 mV"),
        ("1957,1989", "Skou (1957, 1989) isolò una ATPasi"),
        ("10-", "7,3 × 10⁻⁷ mol cm⁻²"),
    ] {
        let batch = single_item_batch_with_protected_span(span);
        let id = &batch.items[0].item_id;
        let response = serde_json::json!({
            "items": [
                {"id": id, "translation": translation},
            ]
        })
        .to_string();

        let result = parse_batch_response(&batch, &response).expect("parse");
        assert_eq!(
            result.failures.len(),
            0,
            "localized numeric form should pass for span {span}"
        );
        assert_eq!(result.translations.len(), 1);
        assert_eq!(result.translations[0].text, translation);
    }
}

#[test]
fn absent_numeric_protected_span_still_fails_batch_validation() {
    let batch = single_item_batch_with_protected_span("5.16");
    let id = &batch.items[0].item_id;
    let response = serde_json::json!({
            "items": [
                {"id": id, "translation": "Si noti che questa forma di rettificazione deriva dai canali aperti."},
            ]
        })
        .to_string();

    let result = parse_batch_response(&batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 0);
    assert_eq!(result.failures.len(), 1);
    assert!(
        result.failures[0]
            .error
            .contains("protected span missing: 5.16"),
        "got: {}",
        result.failures[0].error
    );
}

#[test]
fn missing_marker_close_fails_batch_item_validation() {
    let mut item = batch_item("marked", "<m1>source</m1>");
    item.required_markers = vec!["m1".to_string()];

    let error = batch_item_validation_error(&item, "<m1>translated", false, None, None)
        .expect("missing marker close should fail");

    assert!(error.contains("missing closing tag"), "got: {error}");
}

#[test]
fn copied_source_prose_fails_batch_item_validation() {
    let source = "This deliberately long English paragraph contains enough ordinary prose to \
            exercise untranslated-copy detection in a real batch response. The provider returned \
            the entire source paragraph unchanged instead of translating it into the requested \
            target language, so this item must enter the normal retry and review pipeline.";
    let item = batch_item("copied", source);

    let error = batch_item_validation_error(&item, source, true, Some("Chapter 1"), None)
        .expect("long unchanged source prose should fail");

    assert!(error.contains("unchanged from the source-language prose"));
}

#[test]
fn copied_source_prose_fails_internal_batch_response_validation() {
    let source = "This deliberately long English paragraph contains enough ordinary prose to \
            exercise untranslated-copy detection in a real batch response. The provider returned \
            the entire source paragraph unchanged instead of translating it into the requested \
            target language, so this item must enter the normal retry and review pipeline.";
    let item = batch_item("copied-response", source);
    let response = serde_json::json!({
        "items": [{
            "id": item.item_id,
            "translation": source,
        }]
    })
    .to_string();
    let batch = TranslationBatch {
        id: "copied-response".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        token_estimate: 100,
        section_id: item.section_id.clone(),
        items: vec![item.clone()],
    };
    let section_titles = HashMap::from([(item.segment_id.0.clone(), "Chapter 1".to_string())]);

    let result =
        parse_batch_response_with_validation(&batch, &response, true, Some(&section_titles), None)
            .expect("valid JSON should parse");

    assert!(result.translations.is_empty());
    assert_eq!(result.failures.len(), 1);
    assert!(
        result.failures[0]
            .error
            .contains("unchanged from the source-language prose")
    );
}

#[test]
fn run_preserving_batch_rejects_unknown_run_id_without_success() {
    let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
    let item = &batch.items[0];
    let response = serde_json::json!({
        "items": [{
            "id": item.item_id,
            "runs": [
                {"id": "r0", "text": "Ciao "},
                {"id": "unknown", "text": "mondo"},
            ],
        }]
    })
    .to_string();

    let result = parse_batch_response(&batch, &response).expect("parse");

    assert_eq!(result.translations.len(), 0);
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].error.contains("unknown run ID"));
}

#[test]
fn run_preserving_batch_rejects_duplicate_run_id_without_success() {
    let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
    let item = &batch.items[0];
    let response = serde_json::json!({
        "items": [{
            "id": item.item_id,
            "runs": [
                {"id": "r0", "text": "Ciao "},
                {"id": "r0", "text": "mondo"},
            ],
        }]
    })
    .to_string();

    let result = parse_batch_response(&batch, &response).expect("parse");

    assert_eq!(result.translations.len(), 0);
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].error.contains("duplicate run ID"));
}

#[test]
fn run_preserving_batch_joins_in_source_run_order() {
    let batch = run_preserving_batch_with_runs(&["Hello ", "world"]);
    let item = &batch.items[0];
    let response = serde_json::json!({
        "items": [{
            "id": item.item_id,
            "runs": [
                {"id": "r1", "text": "mondo"},
                {"id": "r0", "text": "Ciao "},
            ],
        }]
    })
    .to_string();

    let result = parse_batch_response(&batch, &response).expect("parse");

    assert_eq!(result.failures.len(), 0);
    assert_eq!(result.translations.len(), 1);
    assert_eq!(result.translations[0].text, "Ciao mondo");
}

#[test]
fn run_preserving_batch_rejects_malformed_joined_marker_structure() {
    let mut item = batch_item("marked-runs", "<m1>source</m1>");
    item.text_runs = (0..13)
        .map(|index| SegmentTextRun {
            id: format!("r{index}"),
            text: String::new(),
        })
        .collect();
    item.required_markers = vec!["m1".to_string()];
    let batch = TranslationBatch {
        id: "run-preserving".to_string(),
        ordinal: 0,
        mode: BatchMode::RunPreserving,
        kind: BatchKind::Translation,
        token_estimate: 100,
        items: vec![item.clone()],
        section_id: item.section_id.clone(),
    };
    let runs = (0..13)
        .map(|index| {
            serde_json::json!({
                "id": format!("r{index}"),
                "text": if index == 0 { "<m1>translated" } else { "" },
            })
        })
        .collect::<Vec<_>>();
    let response = serde_json::json!({
        "items": [{
            "id": item.item_id,
            "runs": runs,
        }]
    })
    .to_string();

    let result = parse_batch_response(&batch, &response).expect("parse");

    assert_eq!(result.translations.len(), 0);
    assert_eq!(result.failures.len(), 1);
    assert!(
        result.failures[0].error.contains("missing closing tag"),
        "got: {}",
        result.failures[0].error
    );
}

#[test]
fn detects_missing_items_in_batch_response() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced);
    let batch = &batches[0];
    let id1 = &batch.items[0].item_id;

    let response = serde_json::json!({
        "items": [
            {"id": id1, "translation": "Ciao mondo"},
        ]
    })
    .to_string();

    let error = parse_batch_response(batch, &response)
        .expect_err("missing requested items must fail the whole response");
    assert!(error.contains("batch response incomplete"));
    assert!(error.contains("requested 2 items, returned 1"));
}

#[test]
fn present_empty_batch_item_remains_a_per_item_failure() {
    let batch = make_two_item_batch();
    let first_id = batch.items[0].item_id.clone();
    let second_id = batch.items[1].item_id.clone();
    let response = serde_json::json!({
        "items": [
            {"id": first_id, "translation": "Ciao"},
            {"id": second_id, "translation": " \n\t"},
        ]
    })
    .to_string();

    let result = parse_batch_response(&batch, &response)
        .expect("a complete envelope with a bad item stays on the per-item path");

    assert_eq!(result.translations.len(), 1);
    assert_eq!(result.failures.len(), 1);
    assert_eq!(result.failures[0].item_id, second_id);
    assert!(
        result.failures[0]
            .error
            .contains("empty translation for non-empty source")
    );
}

#[test]
fn detects_duplicate_ids_in_batch_response() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&[seg1], &config, TranslationProfile::Balanced);
    let batch = &batches[0];
    let id1 = &batch.items[0].item_id;

    let response = serde_json::json!({
        "items": [
            {"id": id1, "translation": "Ciao mondo"},
            {"id": id1, "translation": "Duplicato"},
        ]
    })
    .to_string();

    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 1);
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].error.contains("duplicate"));
}

#[test]
fn splits_batch_in_half() {
    let seg1 = make_segment("seg1", vec![plain_block("A")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("B")], vec![]);
    let seg3 = make_segment("seg3", vec![plain_block("C")], vec![]);
    let seg4 = make_segment("seg4", vec![plain_block("D")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(
        &[seg1, seg2, seg3, seg4],
        &config,
        TranslationProfile::Balanced,
    );
    let split = split_batch(&batches[0]);
    assert_eq!(split.len(), 2);
    assert_eq!(split[0].items.len(), 2);
    assert_eq!(split[1].items.len(), 2);
}

use crate::EngineRuntimeSettings;
use crate::provider::{
    CompletionRequest, CompletionResponse, LlmProvider as LlmProviderTrait, ProviderCapabilities,
    Result as ProviderResult,
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

enum StubBehavior {
    FinishLength,
    ErrInvalid(String),
    ItemsFromBatch(Vec<(String, String)>),
}

struct StubProvider {
    behavior: Mutex<Option<StubBehavior>>,
}

impl StubProvider {
    fn new(behavior: StubBehavior) -> Self {
        Self {
            behavior: Mutex::new(Some(behavior)),
        }
    }
}

enum RecordedResponse {
    FinishLength,
    ItemsFromBatch(Vec<(String, String)>),
}

struct RecordingSequenceProvider {
    responses: Mutex<Vec<RecordedResponse>>,
    max_output_tokens: Arc<Mutex<Vec<Option<u32>>>>,
}

struct DelayedSecondBatchProvider {
    first_item_id: String,
    second_item_id: String,
}

impl LlmProviderTrait for DelayedSecondBatchProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let (item_id, translation) = if request.user.contains(&self.first_item_id) {
            (self.first_item_id.clone(), "Ciao")
        } else {
            assert!(request.user.contains(&self.second_item_id));
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            (self.second_item_id.clone(), "Addio")
        };
        Ok(CompletionResponse {
            content: serde_json::json!({
                "items": [{"id": item_id, "translation": translation}]
            })
            .to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

impl RecordingSequenceProvider {
    fn new(
        responses: Vec<RecordedResponse>,
        max_output_tokens: Arc<Mutex<Vec<Option<u32>>>>,
    ) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().rev().collect()),
            max_output_tokens,
        }
    }
}

impl LlmProviderTrait for RecordingSequenceProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        self.max_output_tokens
            .lock()
            .unwrap()
            .push(request.max_output_tokens);
        let response = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .unwrap_or(RecordedResponse::FinishLength);
        match response {
            RecordedResponse::FinishLength => Ok(CompletionResponse {
                content: "{\"items\":[]}".to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Length,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            }),
            RecordedResponse::ItemsFromBatch(items) => {
                let json = serde_json::json!({
                    "items": items
                        .into_iter()
                        .map(|(id, t)| serde_json::json!({"id": id, "translation": t}))
                        .collect::<Vec<_>>(),
                });
                Ok(CompletionResponse {
                    content: json.to_string(),
                    input_tokens: Some(1),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(1),
                    finish_reason: FinishReason::Stop,
                    provider_latency_ms: 0,
                    raw: serde_json::json!({}),
                })
            }
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

struct RecordingProgress {
    events: Arc<Mutex<Vec<bookforge_core::ProgressEvent>>>,
}

impl bookforge_core::ProgressSink for RecordingProgress {
    fn emit(&self, event: bookforge_core::ProgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

impl LlmProviderTrait for StubProvider {
    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let behavior = self
            .behavior
            .lock()
            .unwrap()
            .take()
            .expect("stub used twice");
        match behavior {
            StubBehavior::FinishLength => Ok(CompletionResponse {
                content: "{\"items\":[]}".to_string(),
                input_tokens: Some(1),
                input_cached_tokens: Some(0),
                output_tokens: Some(1),
                finish_reason: FinishReason::Length,
                provider_latency_ms: 0,
                raw: serde_json::json!({}),
            }),
            StubBehavior::ErrInvalid(msg) => Err(LlmError::InvalidResponse(msg)),
            StubBehavior::ItemsFromBatch(items) => {
                let json = serde_json::json!({
                    "items": items
                        .into_iter()
                        .map(|(id, t)| serde_json::json!({"id": id, "translation": t}))
                        .collect::<Vec<_>>(),
                });
                Ok(CompletionResponse {
                    content: json.to_string(),
                    input_tokens: Some(1),
                    input_cached_tokens: Some(0),
                    output_tokens: Some(1),
                    finish_reason: FinishReason::Stop,
                    provider_latency_ms: 0,
                    raw: serde_json::json!({}),
                })
            }
        }
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

fn test_run_config() -> TranslationRunConfig {
    TranslationRunConfig {
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        provider: "stub".to_string(),
        model: "stub".to_string(),
        prompt_version: "v1".to_string(),
        temperature: 0.2,
        scheduler: bookforge_core::scheduler::SchedulerConfig::default(),
        profile: TranslationProfile::Balanced,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        compact_prompts: false,
        glossary: crate::GlossaryRunConfig::default(),
        context: crate::ContextRunConfig::default(),
        context_registry: None,
        style: None,
        entities: None,
        pause_signal: None,
        runtime_settings: None,
    }
}

fn make_two_item_batch() -> TranslationBatch {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    build_translation_batches(&[seg1, seg2], &config, TranslationProfile::Balanced)
        .into_iter()
        .next()
        .unwrap()
}

#[test]
fn batch_prompt_shares_segment_glossary_and_keeps_item_guidance() {
    let batch = make_two_item_batch();
    let mut config = test_run_config();
    config.glossary.entries_by_segment.insert(
        "seg1".to_string(),
        vec![bookforge_core::GlossaryPromptTerm {
            source: "Hello".to_string(),
            target: "Ciao".to_string(),
            category: bookforge_core::GlossaryCategory::Phrase,
            note: None,
            term_id: Some(7),
            case_sensitive: false,
        }],
    );
    config.glossary.prompt_extra = Some("Use informal register.".to_string());
    config.glossary.guidance_by_segment.insert(
        "seg1".to_string(),
        "Translate the greeting less literally.".to_string(),
    );

    let rendered_items = render_batch_items(&batch, &config);
    let prompt_extra = render_batch_prompt_extra(&batch.items, &config);
    assert!(!rendered_items.contains("\"glossary\""));
    assert!(rendered_items.contains("\"retry_guidance\""));
    assert!(rendered_items.contains("Translate the greeting less literally."));
    assert!(!rendered_items.contains("\"target\":\"Ciao\""));
    assert!(prompt_extra.contains("Active batch glossary constraints"));
    assert!(prompt_extra.contains("\"source\":\"Hello\""));
    assert!(prompt_extra.contains("\"target\":\"Ciao\""));
    assert!(prompt_extra.contains("Use informal register."));
}

#[test]
fn turbo_batches_keep_internal_markers_but_hide_them_from_the_model() {
    let block = SegmentBlock {
        block_id: bookforge_core::ir::BlockId("block1".to_string()),
        kind: "paragraph".to_string(),
        text: "Before <m1>bold</m1> 42".to_string(),
        text_runs: Vec::new(),
        protected_spans: vec![ProtectedSpan {
            kind: ProtectedSpanKind::Number,
            text: "42".to_string(),
        }],
    };
    let segment = make_segment("seg1", vec![block], vec!["m1".to_string()]);
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batch =
        build_translation_batches(&[segment], &batch_config, TranslationProfile::TurboTextOnly)
            .into_iter()
            .next()
            .unwrap();

    assert_eq!(batch.mode, BatchMode::TurboTextOnly);
    assert_eq!(batch.items[0].source_text, "Before <m1>bold</m1> 42");
    assert!(batch.items[0].required_markers.is_empty());
    assert_eq!(
        batch.items[0]
            .protected_spans
            .iter()
            .map(|span| (span.kind, span.text.as_str()))
            .collect::<Vec<_>>(),
        [(ProtectedSpanKind::Number, "42")]
    );

    let rendered = render_batch_items(&batch, &test_run_config());
    assert!(!rendered.contains("<m1>"));
    assert!(!rendered.contains("</m1>"));
    assert!(rendered.contains("Before bold 42"));
    assert!(rendered.contains("\"protected\":[\"42\"]"));
    assert!(!rendered.contains("\"kind\":\"Number\""));
}

#[test]
fn batch_prompt_overhead_repacks_glossary_heavy_items() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 120,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches =
        build_translation_batches(&[seg1, seg2], &batch_config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].items.len(), 2);

    let mut config = test_run_config();
    for segment_id in ["seg1", "seg2"] {
        config.glossary.entries_by_segment.insert(
            segment_id.to_string(),
            vec![bookforge_core::GlossaryPromptTerm {
                source: format!("{segment_id}_source"),
                target: format!("{segment_id}_target"),
                category: bookforge_core::GlossaryCategory::Phrase,
                note: Some("x".repeat(480)),
                term_id: None,
                case_sensitive: false,
            }],
        );
    }
    config.glossary.prompt_extra = Some("y".repeat(160));

    let adjusted = account_for_batch_prompt_overhead(batches, &batch_config, &config);

    assert_eq!(adjusted.len(), 2);
    assert!(adjusted.iter().all(|batch| batch.items.len() == 1));
    assert!(adjusted.iter().all(|batch| batch.token_estimate > 120));
}

#[test]
fn glossary_does_not_multiply_fixed_corpus_batch_prompts() {
    let segments = (0..8)
        .map(|segment_index| {
            let blocks = (0..64)
                .map(|block_index| {
                    plain_block(&format!(
                        "Segment {segment_index} block {block_index}: a compact sentence for a fixed corpus."
                    ))
                })
                .collect();
            make_segment(&format!("seg{segment_index}"), blocks, vec![])
        })
        .collect::<Vec<_>>();
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let initial = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);

    let plain_config = test_run_config();
    let plain_batches =
        account_for_batch_prompt_overhead(initial.clone(), &batch_config, &plain_config);
    let library = PromptLibrary::global();
    let plain_chars = plain_batches
        .iter()
        .map(|batch| {
            let rendered = render_batch_prompt(batch, &plain_config, library, "", 0)
                .expect("plain prompt renders");
            rendered.system.chars().count() + rendered.user.chars().count()
        })
        .sum::<usize>();

    let mut glossary_config = test_run_config();
    for segment_index in 0..segments.len() {
        glossary_config.glossary.entries_by_segment.insert(
            format!("seg{segment_index}"),
            (0..24)
                .map(|term_index| bookforge_core::GlossaryPromptTerm {
                    source: format!("source term {segment_index}-{term_index}"),
                    target: format!("target term {segment_index}-{term_index}"),
                    category: bookforge_core::GlossaryCategory::Phrase,
                    note: None,
                    term_id: Some((segment_index * 100 + term_index) as i64),
                    case_sensitive: false,
                })
                .collect(),
        );
    }
    let glossary_batches =
        account_for_batch_prompt_overhead(initial, &batch_config, &glossary_config);
    let glossary_chars = glossary_batches
        .iter()
        .map(|batch| {
            let rendered = render_batch_prompt(batch, &glossary_config, library, "", 0)
                .expect("glossary prompt renders");
            rendered.system.chars().count() + rendered.user.chars().count()
        })
        .sum::<usize>();

    assert!(
        glossary_batches.len() <= plain_batches.len() * 2,
        "glossary expanded {} plain batches to {} and the rendered prompts from {plain_chars} to {glossary_chars} chars ({:.2}x)",
        plain_batches.len(),
        glossary_batches.len(),
        glossary_chars as f64 / plain_chars as f64
    );
    assert!(
        glossary_chars <= plain_chars * 2,
        "glossary expanded the rendered prompts from {plain_chars} to {glossary_chars} chars ({:.2}x)",
        glossary_chars as f64 / plain_chars as f64
    );
}

#[tokio::test]
async fn batch_length_finish_reason_returns_invalid_response() {
    let batch = make_two_item_batch();
    let provider = Arc::new(StubProvider::new(StubBehavior::FinishLength));
    let library = Arc::new(PromptLibrary::global().clone());
    let config = test_run_config();
    let section_titles = HashMap::new();

    let result = translate_one_batch(
        provider,
        library,
        batch,
        BatchTranslationRequest {
            config: &config,
            max_output_tokens_override: None,
            context_pairs: Vec::new(),
            validate_source_copy: false,
            section_titles: &section_titles,
            compact_retry_attempt: 0,
        },
    )
    .await;
    match result {
        Err(LlmError::InvalidResponse(msg)) => {
            assert!(msg.contains("truncated"), "unexpected msg: {msg}")
        }
        other => panic!("expected InvalidResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn batch_truncated_error_is_not_swallowed() {
    let batch = make_two_item_batch();
    let provider = Arc::new(StubProvider::new(StubBehavior::ErrInvalid(
        "output was truncated".to_string(),
    )));
    let library = Arc::new(PromptLibrary::global().clone());
    let config = test_run_config();
    let section_titles = HashMap::new();

    let result = translate_one_batch(
        provider,
        library,
        batch,
        BatchTranslationRequest {
            config: &config,
            max_output_tokens_override: None,
            context_pairs: Vec::new(),
            validate_source_copy: false,
            section_titles: &section_titles,
            compact_retry_attempt: 0,
        },
    )
    .await;
    match result {
        Err(LlmError::InvalidResponse(msg)) => {
            assert!(msg.contains("truncated"), "unexpected msg: {msg}")
        }
        Ok(_) => panic!("truncated error must not be swallowed into Ok"),
        other => panic!("expected InvalidResponse, got {other:?}"),
    }
}

#[tokio::test]
async fn batch_truncation_retries_same_batch_with_escalated_budget() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let segments = vec![seg1, seg2];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_ids = batches[0]
        .items
        .iter()
        .map(|item| {
            (
                item.item_id.clone(),
                format!("Tradotto {}", item.source_text),
            )
        })
        .collect::<Vec<_>>();
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingSequenceProvider::new(
        vec![
            RecordedResponse::FinishLength,
            RecordedResponse::ItemsFromBatch(item_ids),
        ],
        max_tokens.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let progress = Arc::new(RecordingProgress {
        events: events.clone(),
    });

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        progress,
        None,
        |_| Ok(()),
    )
    .await
    .expect("escalated retry should succeed");

    assert_eq!(translations.len(), 2);
    let budgets = max_tokens.lock().unwrap().clone();
    assert_eq!(budgets.len(), 2);
    assert!(
        budgets[1].unwrap() > budgets[0].unwrap(),
        "second request should use escalated output budget: {budgets:?}"
    );
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. }
                if kind == "batch_truncation_escalated_retry"
        )
    }));
    assert!(
        !events
            .iter()
            .any(|event| { matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }) })
    );
}

// Audit LLM-P3a: the escalation ladder's ceiling used to be computed without
// the user cap (`None` passed through), letting an escalated retry exceed an
// explicitly configured max-output-tokens limit. The cap outranks everything,
// including the profile/context ceiling used for escalation.
#[test]
fn escalation_ladder_tops_out_at_the_explicit_user_cap() {
    let batch = make_two_item_batch();
    const USER_CAP: u32 = 600;
    let mut config = test_run_config();
    config.batch_max_output_tokens = Some(USER_CAP);

    let mut budgets = vec![capped_batch_max_output_tokens(&batch, &config, false)];
    while let Some(next) = next_escalated_batch_max_output_tokens(
        *budgets.last().expect("seeded"),
        &batch,
        &config,
        false,
    ) {
        budgets.push(next);
    }
    assert!(
        budgets.iter().all(|budget| *budget <= USER_CAP),
        "every escalated budget must respect the user cap: {budgets:?}"
    );
    assert_eq!(
        budgets.last().copied(),
        Some(USER_CAP),
        "ladder should still climb freely up to exactly the user cap: {budgets:?}"
    );
}

// Audit LLM-P3b: the context-window remainder deduction used only the packed
// item payload (`token_estimate`); the fixed template scaffold was invisible
// to the remainder math. The planner now adds a documented constant.
#[test]
fn batch_context_remainder_deducts_the_template_scaffold() {
    let batch = make_two_item_batch();
    let window = Some(8_000_u32);
    let bare_payload =
        crate::scheduler::clamped_output_budget(16_384, batch.token_estimate, window, None);
    let planned = crate::scheduler::clamped_output_budget(
        16_384,
        crate::batch::planning::batch_prompt_estimate(&batch),
        window,
        None,
    );
    assert_eq!(
        bare_payload.saturating_sub(planned),
        crate::batch::planning::BATCH_TEMPLATE_OVERHEAD_TOKENS as u32,
        "planning must deduct exactly the documented template overhead"
    );
}

#[tokio::test]
async fn single_item_truncation_retries_once_with_same_compact_budget() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_id = batches[0].items[0].item_id.clone();
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingSequenceProvider::new(
        vec![
            RecordedResponse::FinishLength,
            RecordedResponse::ItemsFromBatch(vec![(item_id, "Ciao".to_string())]),
        ],
        max_tokens.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let progress = Arc::new(RecordingProgress {
        events: events.clone(),
    });

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        progress,
        None,
        |_| Ok(()),
    )
    .await
    .expect("compact retry should succeed");

    assert_eq!(translations.len(), 1);
    let budgets = max_tokens.lock().unwrap().clone();
    assert_eq!(budgets.len(), 2);
    assert_eq!(budgets[1], budgets[0]);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| {
        matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. }
                if kind == "single_item_batch_compact_retry"
        )
    }));
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }))
    );
}

#[tokio::test]
async fn completed_batch_segments_are_published_before_the_whole_run_finishes() {
    let segments = vec![
        make_segment("seg1", vec![plain_block("Hello")], vec![]),
        make_segment("seg2", vec![plain_block("Goodbye")], vec![]),
    ];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 2);
    let provider = DelayedSecondBatchProvider {
        first_item_id: batches[0].items[0].item_id.clone(),
        second_item_id: batches[1].items[0].item_id.clone(),
    };
    let published = Arc::new(AtomicUsize::new(0));
    let published_for_callback = published.clone();
    let run_config = test_run_config();
    let run = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &run_config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        move |_| {
            published_for_callback.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    tokio::pin!(run);

    tokio::select! {
        result = &mut run => panic!("run finished before delayed batch: {result:?}"),
        () = tokio::time::sleep(std::time::Duration::from_millis(75)) => {}
    }
    assert_eq!(published.load(Ordering::SeqCst), 1);

    let translations = run.await.expect("batch run");
    assert_eq!(translations.len(), 2);
    assert_eq!(published.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn single_item_compact_retry_survives_adaptive_renaming() {
    let long_source = "long ".repeat(3_600);
    let seg = make_segment("seg1", vec![plain_block(&long_source)], vec![]);
    let segments = vec![seg];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: true,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    assert!(
        batches[0].token_estimate
            > BatchSizer::new(cfg.target_tokens, cfg.max_items).target_tokens(),
        "fixture must force adaptive normalization to rename the single-item batch"
    );
    let item_ids = batches[0]
        .items
        .iter()
        .map(|item| (item.item_id.clone(), "Tradotto lungo".to_string()))
        .collect::<Vec<_>>();
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingSequenceProvider::new(
        vec![
            RecordedResponse::FinishLength,
            RecordedResponse::ItemsFromBatch(item_ids),
        ],
        max_tokens.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let progress = Arc::new(RecordingProgress {
        events: events.clone(),
    });
    let mut sizer = BatchSizer::new(cfg.target_tokens, cfg.max_items);

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        Some(&mut sizer),
        progress,
        None,
        |_| Ok(()),
    )
    .await
    .expect("compact retry should survive adaptive renaming");

    assert_eq!(translations.len(), 1);
    let budgets = max_tokens.lock().unwrap().clone();
    assert_eq!(budgets.len(), 2);
    assert_eq!(
        budgets[1], budgets[0],
        "compact anti-repetition retry should keep the bounded budget after adaptive renaming"
    );
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| { matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }) }),
        "batch should not split before the escalated retry"
    );
}

#[tokio::test]
async fn systemic_truncation_emits_alert_after_escalated_failures() {
    let segments = (0..6)
        .map(|idx| make_segment(&format!("seg{idx}"), vec![plain_block("Hello")], vec![]))
        .collect::<Vec<_>>();
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 2,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert!(
        batches.len() >= 3,
        "fixture should build at least 3 batches"
    );
    let max_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingSequenceProvider::new(
        std::iter::repeat_with(|| RecordedResponse::FinishLength)
            .take(64)
            .collect(),
        max_tokens,
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let progress = Arc::new(RecordingProgress {
        events: events.clone(),
    });

    let _translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        progress,
        None,
        |_| Ok(()),
    )
    .await
    .expect("systemic truncation should become bounded failures");

    let events = events.lock().unwrap();
    assert!(
        events.iter().any(|event| {
            matches!(
                event,
                bookforge_core::ProgressEvent::Warning { kind, message, .. }
                    if kind == "systemic_truncation"
                        && message.contains("--batch-max-output-tokens")
                        && message.contains("--batch-max-items")
            )
        }),
        "systemic truncation alert should be emitted"
    );
}

#[tokio::test]
async fn batch_translation_preserves_original_block_ids() {
    let seg1 = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let seg2 = make_segment("seg2", vec![plain_block("Goodbye")], vec![]);
    let segments = vec![seg1.clone(), seg2.clone()];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    let item_ids: Vec<(String, String)> = batches[0]
        .items
        .iter()
        .map(|i| (i.item_id.clone(), format!("[it] {}", i.source_text)))
        .collect();

    let provider = StubProvider::new(StubBehavior::ItemsFromBatch(item_ids));
    let telemetry = Arc::new(TelemetryLog::new());
    let config = test_run_config();
    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        telemetry,
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("translate");

    assert_eq!(translations.len(), 2);
    for translation in translations {
        for block in &translation.blocks {
            assert!(
                !block.block_id.0.contains(':'),
                "block_id leaked compound item id: {}",
                block.block_id.0,
            );
        }
    }
}

struct SequenceProvider {
    responses: Mutex<Vec<String>>,
}

impl SequenceProvider {
    fn new(responses: Vec<String>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().rev().collect()),
        }
    }
}

impl LlmProviderTrait for SequenceProvider {
    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let next = self
            .responses
            .lock()
            .unwrap()
            .pop()
            .expect("SequenceProvider ran out of responses");
        Ok(CompletionResponse {
            content: next,
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

struct FirstInvalidThenPromptEchoProvider {
    calls: Mutex<usize>,
}

impl FirstInvalidThenPromptEchoProvider {
    fn new() -> Self {
        Self {
            calls: Mutex::new(0),
        }
    }
}

impl LlmProviderTrait for FirstInvalidThenPromptEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let mut calls = self.calls.lock().unwrap();
        let call = *calls;
        *calls += 1;
        drop(calls);

        let content = if call == 0 {
            "{not valid json".to_string()
        } else {
            let item_ids = item_ids_from_batch_prompt(&request.user);
            serde_json::json!({
                "items": item_ids
                    .into_iter()
                    .map(|id| serde_json::json!({
                        "id": id,
                        "translation": format!("[it] {id}"),
                    }))
                    .collect::<Vec<_>>(),
            })
            .to_string()
        };

        Ok(CompletionResponse {
            content,
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

struct FirstPartialThenPromptEchoProvider {
    calls: Mutex<usize>,
    requested_item_counts: Arc<Mutex<Vec<usize>>>,
}

impl FirstPartialThenPromptEchoProvider {
    fn new(requested_item_counts: Arc<Mutex<Vec<usize>>>) -> Self {
        Self {
            calls: Mutex::new(0),
            requested_item_counts,
        }
    }
}

impl LlmProviderTrait for FirstPartialThenPromptEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let mut calls = self.calls.lock().unwrap();
        let call = *calls;
        *calls += 1;
        drop(calls);

        let item_ids = item_ids_from_batch_prompt(&request.user);
        self.requested_item_counts
            .lock()
            .unwrap()
            .push(item_ids.len());
        let returned_ids = if call == 0 {
            item_ids.into_iter().take(1).collect::<Vec<_>>()
        } else {
            item_ids
        };
        Ok(CompletionResponse {
            content: serde_json::json!({
                "items": returned_ids
                    .into_iter()
                    .map(|id| serde_json::json!({
                        "translation": format!("[it] {id}"),
                        "id": id,
                    }))
                    .collect::<Vec<_>>(),
            })
            .to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

struct AlwaysTransientProvider {
    calls: AtomicUsize,
}

impl AlwaysTransientProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl LlmProviderTrait for Arc<AlwaysTransientProvider> {
    async fn complete(&self, _request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LlmError::HttpStatus {
            status: 503,
            body: "unavailable".to_string(),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

#[derive(Clone, Copy)]
enum DecodeFailureMode {
    FirstRequest,
    MultiItemRequests,
}

struct DecodeThenPromptEchoProvider {
    calls: AtomicUsize,
    decode_url: String,
    mode: DecodeFailureMode,
    requested_item_counts: Arc<Mutex<Vec<usize>>>,
}

impl DecodeThenPromptEchoProvider {
    fn new(
        decode_url: String,
        mode: DecodeFailureMode,
        requested_item_counts: Arc<Mutex<Vec<usize>>>,
    ) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            decode_url,
            mode,
            requested_item_counts,
        }
    }
}

impl LlmProviderTrait for DecodeThenPromptEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let item_ids = item_ids_from_batch_prompt(&request.user);
        self.requested_item_counts
            .lock()
            .unwrap()
            .push(item_ids.len());
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let should_decode = match self.mode {
            DecodeFailureMode::FirstRequest => call == 0,
            DecodeFailureMode::MultiItemRequests => item_ids.len() > 1,
        };
        if should_decode {
            let response = reqwest::get(&self.decode_url).await?;
            let _: serde_json::Value = response.json().await?;
            unreachable!("invalid JSON endpoint unexpectedly decoded")
        }

        Ok(CompletionResponse {
            content: serde_json::json!({
                "items": item_ids
                    .into_iter()
                    .map(|id| serde_json::json!({
                        "translation": format!("[it] {id}"),
                        "id": id,
                    }))
                    .collect::<Vec<_>>(),
            })
            .to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

struct FirstEmptyThenPromptEchoProvider {
    calls: AtomicUsize,
}

impl LlmProviderTrait for Arc<FirstEmptyThenPromptEchoProvider> {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(LlmError::InvalidResponse(
                "the model produced no content".to_string(),
            ));
        }
        let item_ids = item_ids_from_batch_prompt(&request.user);
        Ok(CompletionResponse {
            content: serde_json::json!({
                "items": item_ids
                    .into_iter()
                    .map(|id| serde_json::json!({
                        "translation": format!("[it] {id}"),
                        "id": id,
                    }))
                    .collect::<Vec<_>>(),
            })
            .to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

async fn invalid_json_endpoint(
    expected_requests: usize,
) -> Option<(String, tokio::task::JoinHandle<()>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = match TcpListener::bind("127.0.0.1:0").await {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
        Err(error) => panic!("test listener should bind: {error}"),
    };
    let address = listener.local_addr().expect("listener address");
    let handle = tokio::spawn(async move {
        for _ in 0..expected_requests {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0_u8; 8_192];
            let _ = stream.read(&mut request).await;
            let body = b"{not-json";
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.shutdown().await;
        }
    });
    Some((format!("http://{address}"), handle))
}

struct DelayedPromptEchoProvider;

impl LlmProviderTrait for DelayedPromptEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let item_ids = item_ids_from_batch_prompt(&request.user);
        if item_ids.iter().any(|id| id.contains("First")) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let json = serde_json::json!({
            "items": item_ids
                .into_iter()
                .map(|id| {
                    let text = if id.contains("First") {
                        "[it] First"
                    } else if id.contains("Second") {
                        "[it] Second"
                    } else {
                        "[it] Unknown"
                    };
                    serde_json::json!({"id": id, "translation": text})
                })
                .collect::<Vec<_>>(),
        });
        Ok(CompletionResponse {
            content: json.to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

type CapturedRequestBudgets = Arc<Mutex<Vec<(usize, Option<u32>)>>>;

#[derive(Clone)]
struct GatedPromptEchoProvider {
    started: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
    budgets: CapturedRequestBudgets,
}

impl GatedPromptEchoProvider {
    fn new() -> Self {
        Self {
            started: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(tokio::sync::Semaphore::new(0)),
            budgets: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl LlmProviderTrait for GatedPromptEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let request_index = self.started.fetch_add(1, Ordering::AcqRel);
        self.budgets
            .lock()
            .unwrap()
            .push((request_index, request.max_output_tokens));
        self.release
            .acquire()
            .await
            .expect("test gate should remain open")
            .forget();
        let item_ids = item_ids_from_batch_prompt(&request.user);
        Ok(CompletionResponse {
            content: serde_json::json!({
                "items": item_ids
                    .into_iter()
                    .map(|id| serde_json::json!({
                        "id": id,
                        "translation": format!("[it] {id}"),
                    }))
                    .collect::<Vec<_>>(),
            })
            .to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

fn item_ids_from_batch_prompt(user_prompt: &str) -> Vec<String> {
    let Some(after_input) = user_prompt.split("Input:\n").nth(1) else {
        return Vec::new();
    };
    let json_text = after_input
        .split("\n\nReturn JSON only.")
        .next()
        .unwrap_or(after_input)
        .trim();
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(json_text) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| item.get("id")?.as_str().map(ToString::to_string))
        .collect()
}

#[tokio::test]
async fn partial_batch_response_splits_smaller_without_recording_success() {
    let segment = make_segment(
        "seg1",
        vec![plain_block("Hello"), plain_block("World")],
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let requested_item_counts = Arc::new(Mutex::new(Vec::new()));
    let provider = FirstPartialThenPromptEchoProvider::new(requested_item_counts.clone());
    let events = Arc::new(Mutex::new(Vec::new()));
    let telemetry = Arc::new(TelemetryLog::new());
    let mut sizer = BatchSizer::new(16_000, 128);

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        telemetry.clone(),
        None,
        Some(&mut sizer),
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("split retries should complete");

    assert_eq!(*requested_item_counts.lock().unwrap(), [2, 1, 1]);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    let observations = &sizer.modes[&BatchMode::Plain].recent;
    assert!(matches!(
        observations.front(),
        Some(BatchSizingObservation::InvalidJson)
    ));
    assert_eq!(
        observations
            .iter()
            .filter(|observation| matches!(observation, BatchSizingObservation::Success { .. }))
            .count(),
        2,
        "only the two complete split retries should be recorded as successes"
    );
    let metrics = telemetry.snapshot();
    assert_eq!(metrics[0].status, "incomplete:1/2");
    assert_eq!(metrics[0].items, 2);
    assert_eq!(metrics[0].finish_reason.as_deref(), Some("stop"));
    assert!(events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. }
                if kind == "batch_incomplete_response_split"
        )
    }));
}

#[tokio::test]
async fn partial_repair_response_is_retried_and_present_bad_item_stays_per_item() {
    let segment = make_segment(
        "seg1",
        vec![plain_block("Hello"), plain_block("World")],
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    let first_item_id = batches[0].items[0].item_id.clone();
    let second_item_id = batches[0].items[1].item_id.clone();
    let provider = SequenceProvider::new(vec![
        serde_json::json!({
            "items": [
                {"id": first_item_id, "translation": "[it] Hello"},
                {"id": second_item_id, "translation": " \n"},
            ]
        })
        .to_string(),
        serde_json::json!({"items": []}).to_string(),
        serde_json::json!({
            "items": [
                {"id": second_item_id, "translation": "[it] World"},
            ]
        })
        .to_string(),
    ]);
    let telemetry = Arc::new(TelemetryLog::new());
    let config = test_run_config();
    let events = Arc::new(Mutex::new(Vec::new()));
    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        telemetry,
        None,
        None,
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("translate");

    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(translations[0].joined_text(), "[it] Hello\n\n[it] World");
    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. })),
        "a present-but-invalid item must not split the complete initial envelope"
    );
    assert!(events.iter().any(|event| {
        matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. }
                if kind == "repair_batch_invalid_response_retry"
        )
    }));
}

#[tokio::test]
async fn duplicate_item_id_echo_is_reattributed_and_repaired_not_left_phantom() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_id = batches[0].items[0].item_id.clone();
    // The model echoes the same item ID twice: the parser keeps the first
    // translation and flags the echo as a failure whose segment is the
    // placeholder "unknown". Aggregation must re-attribute that failure to
    // the requested segment instead of letting a phantom "unknown" record
    // reach persistence (its checkpoint INSERT violates the segments FK).
    let duplicated = serde_json::json!({
        "items": [
            {"id": item_id, "translation": "[it] Hello"},
            {"id": item_id, "translation": "[it] Hello"},
        ]
    })
    .to_string();
    let provider = SequenceProvider::new(vec![
        duplicated,
        serde_json::json!({
            "items": [{"id": item_id, "translation": "[it] Hello"}]
        })
        .to_string(),
    ]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("duplicate-echo failure must not abort aggregation");

    assert_eq!(
        translations.len(),
        1,
        "no phantom \"unknown\" segment record may be produced"
    );
    assert_eq!(translations[0].segment_id.0, "seg1");
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        bookforge_core::ProgressEvent::Warning { kind, .. }
            if kind == "batch_unknown_segment_failure_reattributed"
    )));
}

#[tokio::test]
async fn duplicate_echo_of_unrequested_item_id_is_dropped_without_phantom_segment() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_id = batches[0].items[0].item_id.clone();
    // An echo of an ID that was never requested cannot be attributed to any
    // segment; it must be dropped with a warning rather than aggregated as
    // a phantom "unknown" segment.
    let response = serde_json::json!({
        "items": [
            {"id": item_id, "translation": "[it] Hello"},
            {"id": "ghost_item", "translation": "boo"},
            {"id": "ghost_item", "translation": "boo again"},
        ]
    })
    .to_string();
    let provider = SequenceProvider::new(vec![response]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("unattributable echo must not abort aggregation");

    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].segment_id.0, "seg1");
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        bookforge_core::ProgressEvent::Warning { kind, .. }
            if kind == "batch_unknown_segment_failure_dropped"
    )));
}

#[tokio::test]
async fn length_finished_repair_response_is_retried() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_id = batches[0].items[0].item_id.clone();
    let max_output_tokens = Arc::new(Mutex::new(Vec::new()));
    let provider = RecordingSequenceProvider::new(
        vec![
            RecordedResponse::ItemsFromBatch(vec![(item_id.clone(), " \t".to_string())]),
            RecordedResponse::FinishLength,
            RecordedResponse::ItemsFromBatch(vec![(item_id, "[it] Hello".to_string())]),
        ],
        max_output_tokens.clone(),
    );
    let telemetry = Arc::new(TelemetryLog::new());

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        telemetry.clone(),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("length-finished repair should retry");

    assert_eq!(max_output_tokens.lock().unwrap().len(), 3);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    let metrics = telemetry.snapshot();
    assert_eq!(metrics[1].finish_reason.as_deref(), Some("length"));
    assert_eq!(metrics[1].status, "truncated");
    assert_eq!(metrics[2].retry_count, 1);
}

#[tokio::test]
async fn single_item_invalid_response_retries_before_needs_review() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    let item_id = batches[0].items[0].item_id.clone();
    let provider = SequenceProvider::new(vec![
        "{not valid json".to_string(),
        serde_json::json!({
            "items": [
                {"id": item_id, "translation": "[it] Hello"},
            ],
        })
        .to_string(),
    ]);
    let telemetry = Arc::new(TelemetryLog::new());
    let mut config = test_run_config();
    config.scheduler.max_attempts = 2;

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        telemetry,
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("single-item invalid response should retry and succeed");

    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(translations[0].joined_text(), "[it] Hello");
}

#[tokio::test]
async fn transient_batch_errors_stop_after_max_attempts() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let provider = Arc::new(AlwaysTransientProvider::new());
    let telemetry = Arc::new(TelemetryLog::new());
    let mut config = test_run_config();
    config.scheduler.max_attempts = 2;

    let translations = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        translate_batches_with_callback(
            provider.clone(),
            batches,
            &segments,
            &config,
            telemetry.clone(),
            None,
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        ),
    )
    .await
    .expect("transient retries must be capped")
    .expect("batch run should return needs-review translations");

    assert_eq!(provider.calls(), 2);
    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].status, SegmentStatus::NeedsReview);
    // Persisted per-item errors are compact summaries now, not repeated
    // multi-KB dumps; the meaningful head survives truncation unharmed here.
    assert!(
        translations[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("HTTP status 503")),
        "got: {:?}",
        translations[0].error
    );

    // Audit ⚪ group / LLM-4: telemetry carries real values — actual status
    // code, a nonzero retry count after the requeue, and the bounded
    // inter-round backoff that actually elapsed (0.5s base ±20%).
    let metrics = telemetry.snapshot();
    assert_eq!(metrics.len(), 2);
    assert_eq!(metrics[0].retry_count, 0);
    assert_eq!(metrics[1].retry_count, 1);
    assert_eq!(
        metrics[1].status_code,
        Some(503),
        "the recorded status code must be the real one"
    );
    assert!(
        metrics[1].backoff_ms >= 300
            && metrics[1].backoff_ms <= super::execution::MAX_BATCH_RETRY_BACKOFF_MS,
        "backoff should be the bounded exponential+jitter wait, got {}ms",
        metrics[1].backoff_ms
    );
    assert!(
        matches!(
            metrics[1].error_kind,
            Some(bookforge_core::config::ProviderErrorKind::Server)
        ),
        "got: {:?}",
        metrics[1].error_kind
    );
}

#[test]
fn retry_ledger_tracks_requeues_and_inherited_split_history() {
    use super::execution::BatchRetryLedger;

    let mut ledger = BatchRetryLedger::default();
    assert_eq!(ledger.retry_count("b"), 0);

    ledger.record_round("b", 400);
    ledger.record_round("b", 600);
    assert_eq!(ledger.retry_count("b"), 2);
    assert_eq!(ledger.backoff_ms("b"), 1000);

    // Split children are continuations, not fresh zero-history requests.
    ledger.inherit_history("b", &[String::from("b_l"), String::from("b_r")]);
    assert_eq!(ledger.retry_count("b_l"), 2);
    assert_eq!(ledger.backoff_ms("b_r"), 1000);
}

#[test]
fn oversized_provider_errors_are_summarized_once_for_persistence() {
    let dump = LlmError::HttpStatus {
        status: 500,
        body: "x".repeat(8 * 1024),
    };
    let summarized = super::execution::summarize_error_for_items(&dump);

    assert!(
        summarized.chars().count() < 500,
        "persisted summary must be compact, got {} chars",
        summarized.chars().count()
    );
    assert!(summarized.contains("[error text truncated]"));
    assert!(summarized.contains("HTTP status 500"));

    let small = LlmError::HttpStatus {
        status: 408,
        body: "slow".to_string(),
    };
    assert_eq!(
        super::execution::summarize_error_for_items(&small),
        small.to_string(),
        "short errors pass through untouched"
    );
}

#[tokio::test]
async fn decode_error_retries_the_same_batch_before_response_validation() {
    let Some((decode_url, server)) = invalid_json_endpoint(1).await else {
        return;
    };
    let segment = make_segment(
        "seg1",
        vec![plain_block("Hello"), plain_block("World")],
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let requested_item_counts = Arc::new(Mutex::new(Vec::new()));
    let provider = DecodeThenPromptEchoProvider::new(
        decode_url,
        DecodeFailureMode::FirstRequest,
        requested_item_counts.clone(),
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_run_config();
    config.scheduler.max_attempts = 1;

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("decode retry should recover");

    server.await.expect("invalid JSON server should finish");
    assert_eq!(*requested_item_counts.lock().unwrap(), [2, 2]);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. })),
        "a transport decode error must not be parsed as an empty or partial batch response"
    );
}

#[tokio::test]
async fn persistent_decode_error_bisects_an_oversized_batch_after_retry() {
    let Some((decode_url, server)) = invalid_json_endpoint(2).await else {
        return;
    };
    let segment = make_segment(
        "seg1",
        vec![plain_block("Hello"), plain_block("World")],
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let requested_item_counts = Arc::new(Mutex::new(Vec::new()));
    let provider = DecodeThenPromptEchoProvider::new(
        decode_url,
        DecodeFailureMode::MultiItemRequests,
        requested_item_counts.clone(),
    );
    let mut config = test_run_config();
    config.scheduler.max_attempts = 1;

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("decode bisection should recover");

    server.await.expect("invalid JSON server should finish");
    assert_eq!(*requested_item_counts.lock().unwrap(), [2, 2, 1, 1]);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(translations[0].blocks.len(), 2);
}

#[tokio::test]
async fn empty_http_200_content_is_retried_for_a_single_item_batch() {
    let segment = make_segment("seg1", vec![plain_block("Hello")], vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let provider = Arc::new(FirstEmptyThenPromptEchoProvider {
        calls: AtomicUsize::new(0),
    });
    let mut config = test_run_config();
    config.scheduler.max_attempts = 1;

    let translations = translate_batches_with_callback(
        provider.clone(),
        batches,
        &segments,
        &config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("empty-content retry should recover");

    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
}

#[tokio::test]
async fn batch_finalization_preserves_source_block_order() {
    let segment = make_segment(
        "seg1",
        vec![plain_block("First"), plain_block("Second")],
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 2);
    let telemetry = Arc::new(TelemetryLog::new());
    let mut config = test_run_config();
    config.scheduler.concurrency = 2;

    let translations = translate_batches_with_callback(
        DelayedPromptEchoProvider,
        batches,
        &segments,
        &config,
        telemetry,
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("translation should complete");

    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(
        translations[0]
            .blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>(),
        vec!["[it] First", "[it] Second"]
    );
    assert_eq!(translations[0].joined_text(), "[it] First\n\n[it] Second");
}

#[tokio::test]
async fn split_prerequisite_batch_unblocks_book_scoped_context_waiters() {
    let segments = vec![
        make_segment_in_section("seg0", "sec0", 0, vec![plain_block("Alpha")], vec![]),
        make_segment_in_section("seg1", "sec0", 1, vec![plain_block("Beta")], vec![]),
        make_segment_in_section("seg2", "sec1", 2, vec![plain_block("Gamma")], vec![]),
        make_segment_in_section("seg3", "sec2", 3, vec![plain_block("Delta")], vec![]),
    ];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert!(batches.len() >= 3, "expected section-partitioned batches");
    assert!(batches[0].items.len() > 1, "first batch must be splittable");

    let provider = FirstInvalidThenPromptEchoProvider::new();
    let telemetry = Arc::new(TelemetryLog::new());
    let mut config = test_run_config();
    config.scheduler.concurrency = 4;
    config.context = crate::ContextRunConfig {
        window: 1,
        budget_tokens: 1000,
        scope: bookforge_core::config::ContextScope::Book,
        strict: true,
    };
    config.context_registry = Some(Arc::new(crate::ContextRegistry::new(&segments)));

    let run = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        telemetry,
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    );

    let translations = tokio::time::timeout(std::time::Duration::from_secs(5), run)
        .await
        .expect("split prerequisite batch must not deadlock context waiters")
        .expect("translation should complete");

    assert_eq!(translations.len(), segments.len());
    assert!(
        translations
            .iter()
            .all(|translation| translation.status == SegmentStatus::Succeeded)
    );
}

#[tokio::test]
async fn batch_scheduler_does_not_deadlock_when_work_and_result_queues_are_bounded() {
    // Create enough batches to stress both bounded work/result queues.
    let mut blocks = Vec::new();
    for i in 0..64 {
        blocks.push(plain_block(&format!("text_{i}")));
    }
    let segment = make_segment("seg_stress", blocks, vec![]);
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    // With max_items = 1, we get many small batches
    assert!(batches.len() > 32, "need many batches to stress queues");

    // Use MockProvider which handles concurrent requests safely.
    use crate::provider::{MockMode, MockProvider};
    let provider = MockProvider::new(MockMode::PrefixTarget, "Italian");
    let telemetry = Arc::new(TelemetryLog::new());
    let config = test_run_config();
    let progress = Arc::new(bookforge_core::NullProgressSink);

    let run = async {
        translate_batches_with_callback(
            provider,
            batches,
            &segments,
            &config,
            telemetry,
            None,
            None,
            progress,
            None,
            |_| Ok(()),
        )
        .await
        .unwrap();
    };

    tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("batch scheduler must not deadlock");
}

#[tokio::test]
async fn live_batch_settings_bound_dispatch_and_update_later_request_budget() {
    let segment = make_segment(
        "seg_live",
        (0..4)
            .map(|index| plain_block(&format!("text_{index}")))
            .collect(),
        vec![],
    );
    let segments = vec![segment];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 4);

    let provider = GatedPromptEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_run_config();
    config.scheduler.concurrency = 2;
    let mut runtime = EngineRuntimeSettings {
        revision: 1,
        batch: batch_config,
        batch_max_output_tokens: Some(256),
        concurrency: 2,
        provider_max_attempts: 1,
        adaptive_concurrency: false,
        timeout_seconds: 1_200,
    };
    let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
    config.runtime_settings = Some(receiver);

    let run_provider = provider.clone();
    let run_segments = segments.clone();
    let run_events = events.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &run_segments,
            &config,
            Arc::new(TelemetryLog::new()),
            None,
            None,
            Arc::new(RecordingProgress { events: run_events }),
            None,
            |_| Ok(()),
        )
        .await
        .expect("batch translation should finish")
    });

    wait_for_atomic_count(&provider.started, 2).await;
    runtime.revision = 2;
    runtime.concurrency = 1;
    runtime.batch_max_output_tokens = Some(1_024);
    sender.send_replace(runtime);
    provider.release.add_permits(2);

    wait_for_atomic_count(&provider.started, 3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        3,
        "fourth batch must wait after the live concurrency shrink"
    );

    provider.release.add_permits(1);
    wait_for_atomic_count(&provider.started, 4).await;
    provider.release.add_permits(1);
    let translations = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run should finish")
        .expect("task should join");
    assert_eq!(translations.len(), 1);
    let mut indexed_budgets = provider.budgets.lock().unwrap().clone();
    indexed_budgets.sort_unstable_by_key(|(request_index, _)| *request_index);
    let budgets = indexed_budgets
        .into_iter()
        .map(|(_, budget)| budget)
        .collect::<Vec<_>>();
    assert_eq!(budgets.len(), 4);
    assert_eq!(budgets[..2], [Some(256), Some(256)]);
    assert_eq!(budgets[2..], [Some(512), Some(512)]);
    let request_revisions = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            bookforge_core::ProgressEvent::RequestStarted {
                runtime_config_revision,
                provider_max_attempts,
                ..
            } => Some((*runtime_config_revision, *provider_max_attempts)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        request_revisions,
        vec![
            (Some(1), Some(1)),
            (Some(1), Some(1)),
            (Some(2), Some(1)),
            (Some(2), Some(1)),
        ]
    );
}

#[tokio::test]
async fn live_batch_revision_merges_only_unstarted_items() {
    let segment = make_segment(
        "seg_repartition",
        (0..4)
            .map(|index| plain_block(&format!("text_{index}")))
            .collect(),
        vec![],
    );
    let segments = vec![segment];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 4);

    let provider = GatedPromptEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_run_config();
    config.scheduler.concurrency = 1;
    let mut runtime = EngineRuntimeSettings {
        revision: 1,
        batch: batch_config,
        batch_max_output_tokens: None,
        concurrency: 1,
        provider_max_attempts: 1,
        adaptive_concurrency: false,
        timeout_seconds: 1_200,
    };
    let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
    config.runtime_settings = Some(receiver);

    let run_provider = provider.clone();
    let run_segments = segments.clone();
    let run_events = events.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &run_segments,
            &config,
            Arc::new(TelemetryLog::new()),
            None,
            None,
            Arc::new(RecordingProgress { events: run_events }),
            None,
            |_| Ok(()),
        )
        .await
        .expect("batch translation should finish")
    });

    wait_for_atomic_count(&provider.started, 1).await;
    runtime.revision = 2;
    runtime.batch.max_items = 3;
    sender.send_replace(runtime);
    provider.release.add_permits(1);

    wait_for_atomic_count(&provider.started, 2).await;
    provider.release.add_permits(1);
    let translations = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run should finish")
        .expect("task should join");
    assert_eq!(translations.len(), 1);
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        2,
        "the three unstarted one-item batches should merge into one request"
    );
    let request_shapes = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            bookforge_core::ProgressEvent::RequestStarted {
                items,
                runtime_config_revision,
                ..
            } => Some((*items, *runtime_config_revision)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(request_shapes, vec![(1, Some(1)), (3, Some(2))]);
}

#[tokio::test]
async fn paused_batch_reconfigure_repartitions_before_resume_dispatch() {
    let segment = make_segment(
        "seg_paused_repartition",
        (0..4)
            .map(|index| plain_block(&format!("text_{index}")))
            .collect(),
        vec![],
    );
    let segments = vec![segment];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 4);

    let provider = GatedPromptEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let signal = crate::PauseSignal::new();
    signal.pause();
    let mut config = test_run_config();
    config.scheduler.concurrency = 1;
    config.pause_signal = Some(signal.clone());
    let mut runtime = EngineRuntimeSettings {
        revision: 1,
        batch: batch_config,
        batch_max_output_tokens: None,
        concurrency: 1,
        provider_max_attempts: 1,
        adaptive_concurrency: false,
        timeout_seconds: 1_200,
    };
    let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
    config.runtime_settings = Some(receiver);

    let run_provider = provider.clone();
    let run_segments = segments.clone();
    let run_events = events.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &run_segments,
            &config,
            Arc::new(TelemetryLog::new()),
            None,
            None,
            Arc::new(RecordingProgress { events: run_events }),
            None,
            |_| Ok(()),
        )
        .await
        .expect("batch translation should finish")
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(provider.started.load(Ordering::Acquire), 0);
    runtime.revision = 2;
    runtime.batch.max_items = 4;
    runtime.provider_max_attempts = 3;
    sender.send_replace(runtime);
    signal.resume();

    wait_for_atomic_count(&provider.started, 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        1,
        "all pending items should repartition into the new one-request shape"
    );
    provider.release.add_permits(1);
    let translations = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run should finish")
        .expect("task should join");
    assert_eq!(translations.len(), 1);
    let request_shapes = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            bookforge_core::ProgressEvent::RequestStarted {
                items,
                runtime_config_revision,
                provider_max_attempts,
                ..
            } => Some((*items, *runtime_config_revision, *provider_max_attempts)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(request_shapes, vec![(4, Some(2), Some(3))]);
}

/// Provider for the paused-repair test: call 0 is the initial translation
/// batch (first item satisfied, all later items blank so they funnel into
/// repair); repair calls echo corrected text. Repair calls 1 and 2 park on
/// a semaphore gate so the test can inject a pause while both first-wave
/// batches are in flight; later repairs run freely once resumed.
#[derive(Clone)]
struct GatedRepairEchoProvider {
    started: Arc<AtomicUsize>,
    release_gated_repairs: Arc<tokio::sync::Semaphore>,
}

impl GatedRepairEchoProvider {
    fn new() -> Self {
        Self {
            started: Arc::new(AtomicUsize::new(0)),
            release_gated_repairs: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }
}

fn item_ids_from_repair_prompt(user_prompt: &str) -> Vec<String> {
    let Some(items_section) = user_prompt.split("Items to repair:\n").nth(1) else {
        return Vec::new();
    };
    let json_text = items_section
        .split("\n\nValidation errors:")
        .next()
        .unwrap_or(items_section);
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(json_text.trim()) else {
        return Vec::new();
    };
    items
        .into_iter()
        .filter_map(|item| item.get("id")?.as_str().map(ToString::to_string))
        .collect()
}

impl LlmProviderTrait for GatedRepairEchoProvider {
    async fn complete(&self, request: CompletionRequest) -> ProviderResult<CompletionResponse> {
        let call_index = self.started.fetch_add(1, Ordering::AcqRel);
        let content = if call_index == 0 {
            let ids = item_ids_from_batch_prompt(&request.user);
            serde_json::json!({
                "items": ids
                    .iter()
                    .enumerate()
                    .map(|(index, id)| {
                        let text = if index == 0 { "[it] ok" } else { " \n" };
                        serde_json::json!({"id": id, "translation": text})
                    })
                    .collect::<Vec<_>>(),
            })
        } else {
            if call_index == 1 || call_index == 2 {
                self.release_gated_repairs
                    .acquire()
                    .await
                    .expect("test gate should remain open")
                    .forget();
            }
            let ids = item_ids_from_repair_prompt(&request.user);
            serde_json::json!({
                "items": ids
                    .iter()
                    .map(|id| serde_json::json!({"id": id, "translation": format!("[fixed] {id}")}))
                    .collect::<Vec<_>>(),
            })
        };
        Ok(CompletionResponse {
            content: content.to_string(),
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            finish_reason: FinishReason::Stop,
            provider_latency_ms: 0,
            raw: serde_json::json!({}),
        })
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            supports_json_response_format: true,
            supports_usage_tokens: true,
        }
    }
}

// Audit LLM-P2: pausing the run while repair batches are in flight used to
// `break` out of the repair loop, dropping the JoinSet — billed completions
// were discarded and queued-not-started repairs silently froze at
// NeedsReview. The drain must mirror the translation phase: no NEW dispatch
// while paused, in-flight results still joined, everything completes after
// resume. The Stopped abort-with-skips path stays as-is.
#[tokio::test]
async fn paused_repair_phase_drains_inflight_results_without_losing_completions() {
    // One good block plus two repair-batch-limits worth of failing items:
    // the 33 failures chunk into three batches [16, 16, 1]. With
    // concurrency=2 both first batches dispatch, so a pause can land while
    // tasks are IN FLIGHT and one batch is still queued-not-started — the
    // exact shape where the old code dropped the JoinSet.
    let total_items = repair_batch_item_limit("Italian") * 2 + 2;
    let failed_items = total_items - 1;
    let segment = make_segment(
        "seg_pause_drain",
        (0..total_items)
            .map(|index| plain_block(&format!("text_{index}")))
            .collect(),
        vec![],
    );
    let segments = vec![segment];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: usize::MAX / 2,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);

    let provider = GatedRepairEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let signal = crate::PauseSignal::new();
    let mut config = test_run_config();
    config.scheduler.concurrency = 2;
    config.pause_signal = Some(signal.clone());

    let run_provider = provider.clone();
    let run_segments = segments.clone();
    let run_events = events.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &run_segments,
            &config,
            Arc::new(TelemetryLog::new()),
            None,
            None,
            Arc::new(RecordingProgress { events: run_events }),
            None,
            |_| Ok(()),
        )
        .await
        .expect("batch translation should finish")
    });

    // Call 0 is the initial translation batch; calls 1 and 2 are the two
    // concurrent repair batches (one parked on its gate).
    wait_for_atomic_count(&provider.started, 3).await;

    // Pause with BOTH repair batches in flight and a third queued.
    signal.pause();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        3,
        "queued-not-started repair must not dispatch while paused"
    );

    // Release ONE gated batch: its billed output must be JOINED (drained)
    // while the pause persists — not dropped — and the second in-flight batch
    // plus the queued-not-started one must stay held.
    provider.release_gated_repairs.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        3,
        "pause must keep holding new dispatch after draining the finished task"
    );
    assert!(
        !run.is_finished(),
        "pausing with in-flight/queued repairs must not finalize the run"
    );

    // Drain the second in-flight batch: still paused, still holding.
    provider.release_gated_repairs.add_permits(1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        3,
        "draining in-flight work while paused must not dispatch queued batches"
    );
    assert!(
        !run.is_finished(),
        "after draining all in-flight repairs, a paused run waits for resume"
    );

    // Resume: the queued-not-started repair runs and every item is repaired.
    signal.resume();
    let translations = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run should finish after resume")
        .expect("task should join");
    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(
        translations[0].joined_text().matches("[fixed] ").count(),
        failed_items,
        "zero lost completions: every failed item must be repaired exactly once"
    );
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        bookforge_core::ProgressEvent::BatchRepairStarted { failed_item_count, .. }
            if *failed_item_count == failed_items
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        bookforge_core::ProgressEvent::BatchRepairFinished {
            repaired_items,
            still_failed_items,
            ..
        } if *repaired_items == failed_items && *still_failed_items == 0
    )));
    assert!(
        !events.iter().any(|event| matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. }
                if kind == "batch_repair_stopped" || kind == "batch_repair_cancelled"
        )),
        "a drained pause must not be recorded as stopped/cancelled"
    );
}

#[tokio::test]
async fn live_adaptive_concurrency_enable_gates_later_requests() {
    let segment = make_segment(
        "seg_adaptive_live",
        (0..4)
            .map(|index| plain_block(&format!("text_{index}")))
            .collect(),
        vec![],
    );
    let segments = vec![segment];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);

    let provider = GatedPromptEchoProvider::new();
    let mut config = test_run_config();
    config.scheduler.concurrency = 2;
    let mut runtime = EngineRuntimeSettings {
        revision: 1,
        batch: batch_config,
        batch_max_output_tokens: None,
        concurrency: 2,
        provider_max_attempts: 1,
        adaptive_concurrency: false,
        timeout_seconds: 1_200,
    };
    let (sender, receiver) = tokio::sync::watch::channel(runtime.clone());
    config.runtime_settings = Some(receiver);
    let adaptive_limiter = Arc::new(AdaptiveLimiter::new_with_bounds(
        1,
        1,
        4,
        Duration::ZERO,
        None,
    ));
    let controller = Arc::new(ProviderRateController::new(
        adaptive_limiter,
        crate::RateControllerConfig::for_target(1),
    ));

    let run_provider = provider.clone();
    let run_segments = segments.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &run_segments,
            &config,
            Arc::new(TelemetryLog::new()),
            Some(controller),
            None,
            Arc::new(bookforge_core::NullProgressSink),
            None,
            |_| Ok(()),
        )
        .await
        .expect("batch translation should finish")
    });

    wait_for_atomic_count(&provider.started, 2).await;
    runtime.revision = 2;
    runtime.adaptive_concurrency = true;
    sender.send_replace(runtime);
    provider.release.add_permits(2);

    wait_for_atomic_count(&provider.started, 3).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        provider.started.load(Ordering::Acquire),
        3,
        "enabling the one-permit adaptive gate must hold the fourth request"
    );
    provider.release.add_permits(1);
    wait_for_atomic_count(&provider.started, 4).await;
    provider.release.add_permits(1);

    let translations = tokio::time::timeout(Duration::from_secs(2), run)
        .await
        .expect("run should finish")
        .expect("task should join");
    assert_eq!(translations.len(), 1);
}

async fn wait_for_atomic_count(value: &AtomicUsize, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while value.load(Ordering::Acquire) < expected {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("request count should be reached");
}

// ---- Structured, block-attributed findings (#8 engine half) ----------------

fn failure_with_findings() -> BatchItemFailure {
    BatchItemFailure {
        item_id: "seg1:block1".to_string(),
        segment_id: SegmentId("seg1".to_string()),
        error: "error: protected span missing: https://example.com".to_string(),
        input_tokens: None,
        input_cached_tokens: None,
        output_tokens: None,
        tokens_estimated: false,
        findings: vec![
            bookforge_core::finding::EngineFinding::new(
                bookforge_core::finding::QaFindingKind::ProtectedSpanMissing,
                "protected span missing: https://example.com",
            )
            .with_block_id("block1"),
        ],
    }
}

#[test]
fn batch_item_failure_findings_round_trip_through_serde() {
    let failure = failure_with_findings();
    let json = serde_json::to_string(&failure).expect("failure serializes");
    let parsed: BatchItemFailure = serde_json::from_str(&json).expect("failure parses");
    assert_eq!(parsed, failure);
    assert_eq!(parsed.findings.len(), 1);
    assert_eq!(parsed.findings[0].block_id.as_deref(), Some("block1"));
}

#[test]
fn legacy_batch_item_failure_payloads_without_findings_still_parse() {
    // A payload written before the findings field existed (no serde marker).
    let legacy = r#"{
        "item_id": "seg1:block1",
        "segment_id": "seg1",
        "error": "error: protected span missing: https://example.com",
        "input_tokens": null,
        "input_cached_tokens": null,
        "output_tokens": null,
        "tokens_estimated": false
    }"#;
    let parsed: BatchItemFailure = serde_json::from_str(legacy).expect("legacy payload parses");
    assert!(parsed.findings.is_empty(), "findings default to empty");
    assert_eq!(parsed.item_id, "seg1:block1");
}

#[test]
fn block_mismatch_produces_per_block_findings_plus_summary() {
    let summary = "batch translation block mismatch: missing=[\"b_000001\", \"b_000002\"], \
                   extra=[\"b_000009\"], duplicate=[\"b_000003\"]";
    let findings = block_mismatch_findings(
        &["b_000001".to_string(), "b_000002".to_string()],
        &["b_000009".to_string()],
        &["b_000003".to_string()],
        summary,
    );

    // One finding per missing/extra/duplicate block id, each block-attributed.
    assert_eq!(findings.len(), 5);
    for (finding, expected_block) in findings[..4]
        .iter()
        .zip(["b_000001", "b_000002", "b_000009", "b_000003"])
    {
        assert_eq!(
            finding.kind,
            bookforge_core::finding::QaFindingKind::BatchBlockMismatch
        );
        assert_eq!(finding.block_id.as_deref(), Some(expected_block));
        assert_eq!(finding.severity, QaFindingSeverity::Error);
    }
    // The summary message itself carries no block attribution.
    assert_eq!(findings[4].message, summary);
    assert_eq!(findings[4].block_id, None);
}

/// End-to-end: an item that fails deterministic validation carries its
/// structured findings all the way onto the returned `SegmentTranslation`,
/// with block attribution intact. The repair round re-fails the same item so
/// it stays NeedsReview and the findings survive aggregation.
#[tokio::test]
async fn validation_failure_findings_reach_the_segment_record() {
    fn marker_block(text: &str) -> SegmentBlock {
        SegmentBlock {
            block_id: bookforge_core::ir::BlockId(text.to_string()),
            kind: "paragraph".to_string(),
            text: text.to_string(),
            text_runs: vec![SegmentTextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            protected_spans: Vec::new(),
        }
    }

    let seg = make_segment(
        "seg_findings",
        vec![marker_block("Before <m1>bold</m1> after")],
        vec![],
    );
    let segments = vec![seg];
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    let item_id = batches[0].items[0].item_id.clone();
    let broken_translation = "Prima e dopo il testo";
    // Both the translation round and the repair round return text that drops
    // the required marker, so the item ends NeedsReview.
    let provider = SequenceProvider::new(vec![
        serde_json::json!({
            "items": [{"id": item_id, "translation": broken_translation}],
        })
        .to_string(),
        serde_json::json!({
            "items": [{"id": item_id, "translation": broken_translation}],
        })
        .to_string(),
    ]);

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("validation failure must not abort the run");

    assert_eq!(translations.len(), 1);
    let translation = &translations[0];
    assert_eq!(translation.segment_id.0, "seg_findings");
    assert_eq!(translation.status, SegmentStatus::NeedsReview);
    // The legacy error string keeps flowing unchanged.
    assert!(
        translation
            .error
            .as_deref()
            .is_some_and(|error| error.contains("inline marker missing")),
        "error string must be preserved, got: {:?}",
        translation.error
    );
    // ...and the structured finding arrives with its block attribution.
    assert_eq!(translation.findings.len(), 1);
    let finding = &translation.findings[0];
    assert_eq!(
        finding.kind,
        bookforge_core::finding::QaFindingKind::InlineMarkerMissing
    );
    assert_eq!(
        finding.block_id.as_deref(),
        Some("Before <m1>bold</m1> after"),
        "the finding must be pinned to the failed item's block"
    );
    assert_eq!(
        finding.severity,
        bookforge_core::ir::QaFindingSeverity::Error
    );
}

/// End-to-end: a segment that fails structurally (one of its blocks never
/// arrives because the item failed twice) gets `block_mismatch_findings`
/// merged into the segment record alongside the per-item findings.
#[tokio::test]
async fn structural_block_mismatch_findings_reach_the_segment_record() {
    fn marker_block(text: &str) -> SegmentBlock {
        SegmentBlock {
            block_id: bookforge_core::ir::BlockId(text.to_string()),
            kind: "paragraph".to_string(),
            text: text.to_string(),
            text_runs: vec![SegmentTextRun {
                id: "r0".to_string(),
                text: text.to_string(),
            }],
            protected_spans: Vec::new(),
        }
    }

    let seg = make_segment(
        "seg_mismatch",
        vec![
            plain_block("First paragraph"),
            marker_block("Second <m1>marked</m1>"),
        ],
        vec![],
    );
    let segments = vec![seg];
    // One item per batch so the two blocks arrive in separate requests, and
    // serial dispatch so the scripted responses pair with their batches.
    let cfg = BatchConfig {
        enabled: true,
        target_tokens: 1000,
        max_items: 1,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &cfg, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 2, "two single-item batches");
    let first_id = batches[0].items[0].item_id.clone();
    let second_id = batches[1].items[0].item_id.clone();
    let broken_translation = "Il secondo numero";
    let provider = SequenceProvider::new(vec![
        serde_json::json!({
            "items": [{"id": first_id, "translation": "Primo paragrafo"}],
        })
        .to_string(),
        serde_json::json!({
            "items": [{"id": second_id, "translation": broken_translation}],
        })
        .to_string(),
        serde_json::json!({
            "items": [{"id": second_id, "translation": broken_translation}],
        })
        .to_string(),
    ]);
    let mut config = test_run_config();
    config.scheduler.concurrency = 1;

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(bookforge_core::NullProgressSink),
        None,
        |_| Ok(()),
    )
    .await
    .expect("partial segment failure must not abort the run");

    assert_eq!(translations.len(), 1);
    let translation = &translations[0];
    assert_eq!(translation.status, SegmentStatus::NeedsReview);
    assert!(
        translation
            .error
            .as_deref()
            .is_some_and(|error| error.contains("batch translation block mismatch")),
        "mismatch summary must stay in the error string, got: {:?}",
        translation.error
    );
    // The missing block shows up both as the item's own validation finding
    // and as a block-attributed mismatch finding, plus the unattributed
    // summary line.
    let mismatch_block = translation
        .findings
        .iter()
        .find(|finding| {
            finding.kind == bookforge_core::finding::QaFindingKind::BatchBlockMismatch
                && finding.block_id.as_deref() == Some("Second <m1>marked</m1>")
        })
        .expect("mismatch finding pinned to the missing block");
    assert_eq!(
        mismatch_block.severity,
        bookforge_core::ir::QaFindingSeverity::Error
    );
    assert!(
        translation.findings.iter().any(|finding| {
            finding.kind == bookforge_core::finding::QaFindingKind::InlineMarkerMissing
        }),
        "the item's own validation finding is present too"
    );
    assert!(
        translation
            .findings
            .iter()
            .any(|finding| finding.block_id.is_none()),
        "the summary finding stays unattributed"
    );
}

// ---- Latency-aware batch budgets (#3) --------------------------------------

#[test]
fn latency_planning_splits_batches_whose_expected_output_overruns_the_timeout() {
    let long_text = "word ".repeat(2_400); // ~2,400 tokens of source prose
    let seg = make_segment(
        "seg_latency",
        vec![plain_block(&long_text), plain_block(&long_text)],
        vec![],
    );
    let batch = TranslationBatch {
        id: "batch_latency".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        items: vec![
            batch_item("a", long_text.trim()),
            batch_item("b", long_text.trim()),
        ],
        token_estimate: 4_800,
        section_id: seg.section_id.clone(),
    };

    // 180s timeout -> latency output cap = 0.8 * 180 * 30 = 4,320 expected
    // output tokens; this batch expects ~4,800 + envelope, so normalization
    // must split it even though no user output cap is configured.
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let mut config = test_run_config();
    config.runtime_settings = Some(
        EngineRuntimeSettings {
            revision: 1,
            batch: batch_config,
            batch_max_output_tokens: None,
            concurrency: 1,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
            timeout_seconds: 180,
        }
        .frozen_receiver(),
    );

    let sizer = BatchSizer::new(16_000, 64);
    let parts = normalize_batch_for_current_sizer(batch, Some(&sizer), Some(&config));
    assert!(
        parts.len() > 1,
        "a batch whose expected output overruns the 180s timeout must be split"
    );

    // A single-item batch cannot split and keeps its budget.
    let single = TranslationBatch {
        id: "batch_latency_single".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        items: vec![batch_item("a", long_text.trim())],
        token_estimate: 2_400,
        section_id: seg.section_id,
    };
    let parts = normalize_batch_for_current_sizer(single, Some(&sizer), Some(&config));
    assert_eq!(parts.len(), 1, "single-item batches keep their budget");
}

#[test]
fn latency_planning_leaves_batches_alone_without_a_timeout() {
    let long_text = "word ".repeat(2_400);
    let batch = TranslationBatch {
        id: "batch_no_timeout".to_string(),
        ordinal: 0,
        mode: BatchMode::Plain,
        kind: BatchKind::Translation,
        items: vec![
            batch_item("a", long_text.trim()),
            batch_item("b", long_text.trim()),
        ],
        token_estimate: 4_800,
        section_id: bookforge_core::ir::SectionId("section".to_string()),
    };
    // No runtime settings: no timeout knowledge, no latency constraint.
    let parts = normalize_batch_for_current_sizer(batch, None, Some(&test_run_config()));
    assert_eq!(parts.len(), 1);
}

/// A request whose computed output budget implies more generation time than
/// the latency share of the configured timeout is split *before dispatch*,
/// instead of being sent with a budget that can only succeed by luck.
#[tokio::test]
async fn latency_aware_dispatch_splits_oversized_output_budgets() {
    let seg = make_segment(
        "seg_dispatch",
        vec![plain_block("Hello"), plain_block("Goodbye")],
        vec![],
    );
    let segments = vec![seg];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].items.len(), 2);

    let provider = GatedPromptEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_run_config();
    config.scheduler.concurrency = 1;
    // 20s timeout: the planning output cap (480 tokens) still admits the
    // two-item batch (expected output ~260), but the computed 512-token
    // request budget implies ~17s of generation at the conservative 30 tok/s
    // floor — beyond 0.8 * 20s — so the batch must be split before dispatch.
    config.runtime_settings = Some(
        EngineRuntimeSettings {
            revision: 1,
            batch: batch_config,
            batch_max_output_tokens: None,
            concurrency: 1,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
            timeout_seconds: 20,
        }
        .frozen_receiver(),
    );

    provider.release.add_permits(2);
    let translations = translate_batches_with_callback(
        provider.clone(),
        batches,
        &segments,
        &config,
        Arc::new(TelemetryLog::new()),
        None,
        None,
        Arc::new(RecordingProgress {
            events: events.clone(),
        }),
        None,
        |_| Ok(()),
    )
    .await
    .expect("latency split should complete successfully");

    // Both items still translated, via two single-item requests.
    assert_eq!(translations.len(), 1);
    assert_eq!(provider.started.load(Ordering::Acquire), 2);
    let mut budgets = provider.budgets.lock().unwrap().clone();
    budgets.sort_by_key(|(index, _)| *index);
    assert_eq!(
        budgets,
        vec![(0, Some(512)), (1, Some(512))],
        "each single-item child dispatches with the floored budget"
    );

    let split_emitted = events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, bookforge_core::ProgressEvent::BatchSplit { .. }));
    assert!(split_emitted, "the latency split must be observable");
    let latency_warning = events.lock().unwrap().iter().any(|event| {
        matches!(
            event,
            bookforge_core::ProgressEvent::Warning { kind, .. } if kind == "batch_latency_split"
        )
    });
    assert!(latency_warning, "the split reason must be surfaced");
}

/// The watchdog emits RequestProgress heartbeats while a batch request is in
/// flight, so dashboards are not blind between RequestStarted and
/// RequestFinished. Real time is used: the heartbeat cadence is 5s, so the
/// request is held in flight for ~5.6s.
#[tokio::test]
async fn watchdog_emits_request_progress_while_requests_are_in_flight() {
    let seg = make_segment("seg_watchdog", vec![plain_block("Hello")], vec![]);
    let segments = vec![seg];
    let batch_config = BatchConfig {
        enabled: true,
        target_tokens: 16_000,
        max_items: 64,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let batches = build_translation_batches(&segments, &batch_config, TranslationProfile::Balanced);

    let provider = GatedPromptEchoProvider::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut config = test_run_config();
    config.runtime_settings = Some(
        EngineRuntimeSettings {
            revision: 1,
            batch: batch_config,
            batch_max_output_tokens: None,
            concurrency: 1,
            provider_max_attempts: 1,
            adaptive_concurrency: false,
            timeout_seconds: 1_200,
        }
        .frozen_receiver(),
    );

    let run_provider = provider.clone();
    let run_events = events.clone();
    let run = tokio::spawn(async move {
        translate_batches_with_callback(
            run_provider,
            batches,
            &segments,
            &config,
            Arc::new(TelemetryLog::new()),
            None,
            None,
            Arc::new(RecordingProgress { events: run_events }),
            None,
            |_| Ok(()),
        )
        .await
        .expect("watchdog run should finish")
    });

    // Hold the request in flight for a little over one heartbeat interval.
    wait_for_atomic_count(&provider.started, 1).await;
    tokio::time::sleep(Duration::from_millis(5_600)).await;
    assert!(!run.is_finished(), "request still gated in flight");
    provider.release.add_permits(1);
    let _translations = tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .expect("run should finish")
        .expect("task should join");

    let heartbeats = events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            bookforge_core::ProgressEvent::RequestProgress {
                batch_id,
                items,
                elapsed_ms,
                ..
            } => Some((batch_id.clone(), *items, *elapsed_ms)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !heartbeats.is_empty(),
        "at least one in-flight heartbeat must be emitted"
    );
    let batch_id = &heartbeats[0].0;
    assert!(!batch_id.is_empty(), "heartbeat carries the batch id");
    assert_eq!(heartbeats[0].1, 1, "heartbeat carries the item count");
    assert!(
        heartbeats[0].2 >= 5_000,
        "heartbeat elapsed_ms should cover the gated window, got {}",
        heartbeats[0].2
    );
    // Emissions are capped: at most one heartbeat per 5s bucket per batch.
    let mut buckets = heartbeats
        .iter()
        .map(|(id, _, elapsed)| (id.clone(), elapsed / 5_000))
        .collect::<Vec<_>>();
    buckets.sort();
    buckets.dedup();
    assert_eq!(buckets.len(), heartbeats.len(), "no duplicate signatures");
}

#[test]
fn invalid_response_errors_persist_friendly_text_not_serde_internals() {
    let raw = LlmError::InvalidResponse(
        "invalid batch JSON: missing field `translation` at line 1 column 157".to_string(),
    );
    let persisted = super::execution::summarize_error_for_items(&raw);
    assert!(
        !persisted.contains("line 1 column"),
        "serde internals must not be persisted: {persisted}"
    );
    assert!(
        persisted.contains("missing a required field"),
        "persisted text should be the friendly sentence: {persisted}"
    );

    // Non invalid-response errors keep their raw text (the provider error
    // chain remains honest in segments.error).
    let status = LlmError::HttpStatus {
        status: 429,
        body: "slow down".to_string(),
    };
    assert_eq!(
        super::execution::summarize_error_for_items(&status),
        "HTTP status 429: slow down"
    );
}
