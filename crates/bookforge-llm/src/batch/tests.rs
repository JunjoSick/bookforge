use super::*;
use bookforge_core::segment::{
    SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata, SegmentSource,
    SegmentTextRun,
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
        protected_spans: spans,
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
fn missing_protected_span_fails_batch_item_instead_of_appending() {
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

    // The dropped span must surface as an item failure feeding the
    // repair pipeline — never be glued onto the translated text.
    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 0);
    assert_eq!(result.failures.len(), 1);
    assert!(
        result.failures[0]
            .error
            .contains("protected span missing: 4th"),
        "got: {}",
        result.failures[0].error
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

    let result = parse_batch_response(batch, &response).expect("parse");
    assert_eq!(result.translations.len(), 1);
    assert_eq!(result.failures.len(), 1);
    assert!(result.failures[0].error.contains("missing"));
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
fn batch_items_include_segment_glossary() {
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

    let rendered = render_batch_items(&batch, &config);
    assert!(rendered.contains("\"glossary\""));
    assert!(rendered.contains("\"retry_guidance\""));
    assert!(rendered.contains("Translate the greeting less literally."));
    assert!(rendered.contains("\"source\":\"Hello\""));
    assert!(!rendered.contains("Use informal register."));
}

#[test]
fn turbo_batches_keep_internal_markers_but_hide_them_from_the_model() {
    let block = SegmentBlock {
        block_id: bookforge_core::ir::BlockId("block1".to_string()),
        kind: "paragraph".to_string(),
        text: "Before <m1>bold</m1> 42".to_string(),
        text_runs: Vec::new(),
        protected_spans: vec!["42".to_string()],
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
    assert_eq!(batch.items[0].protected_spans, ["42"]);

    let rendered = render_batch_items(&batch, &test_run_config());
    assert!(!rendered.contains("<m1>"));
    assert!(!rendered.contains("</m1>"));
    assert!(rendered.contains("Before bold 42"));
    assert!(rendered.contains("\"protected\":[\"42\"]"));
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

struct RepairOrderingSink(Arc<std::sync::Mutex<Vec<&'static str>>>);

impl bookforge_core::ProgressSink for RepairOrderingSink {
    fn emit(&self, event: bookforge_core::ProgressEvent) {
        if matches!(
            event,
            bookforge_core::ProgressEvent::BatchRepairFinished { .. }
        ) {
            self.0.lock().unwrap().push("repair_finished");
        }
    }
}

#[tokio::test]
async fn completed_repair_publishes_segment_before_repair_phase_finishes() {
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
    let first_item_id = batches[0].items[0].item_id.clone();
    let second_item_id = batches[0].items[1].item_id.clone();
    let provider = SequenceProvider::new(vec![
        serde_json::json!({
            "items": [{"id": first_item_id, "translation": "[it] Hello"}],
        })
        .to_string(),
        serde_json::json!({
            "items": [{"id": second_item_id, "translation": "[it] World"}],
        })
        .to_string(),
    ]);
    let ordering = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::new(RepairOrderingSink(ordering.clone()));
    let callback_ordering = ordering.clone();

    let translations = translate_batches_with_callback(
        provider,
        batches,
        &segments,
        &test_run_config(),
        Arc::new(TelemetryLog::new()),
        None,
        None,
        sink,
        None,
        move |translation| {
            assert_eq!(translation.status, SegmentStatus::Succeeded);
            callback_ordering.lock().unwrap().push("callback");
            Ok(())
        },
    )
    .await
    .expect("repair should complete");

    assert_eq!(translations[0].status, SegmentStatus::Succeeded);
    assert_eq!(*ordering.lock().unwrap(), ["callback", "repair_finished"]);
}

#[tokio::test]
async fn partial_batch_failure_without_successful_repair_marks_segment_needs_review() {
    let seg = make_segment(
        "seg1",
        vec![plain_block("Hello"), plain_block("World")],
        vec![],
    );
    let segments = vec![seg.clone()];
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
    let missing_block_id = batches[0].items[1].block_id.0.clone();

    let initial_response = serde_json::json!({
        "items": [
            {"id": first_item_id, "translation": "[it] Hello"},
        ]
    })
    .to_string();
    // Repair returns malformed JSON so parse_batch_response fails
    // and the missing block stays unrepaired.
    let repair_response = "{not valid json".to_string();

    let provider = SequenceProvider::new(vec![initial_response, repair_response]);
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

    assert_eq!(translations.len(), 1);
    let translation = &translations[0];
    assert_eq!(
        translation.status,
        SegmentStatus::NeedsReview,
        "segment with missing block translation must not be saved as Succeeded",
    );
    let error = translation
        .error
        .as_ref()
        .expect("missing-block segment must carry an error");
    assert!(
        error.contains(&missing_block_id),
        "error must name missing block id {missing_block_id}, got: {error}",
    );
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
        std::time::Duration::from_secs(2),
        translate_batches_with_callback(
            provider.clone(),
            batches,
            &segments,
            &config,
            telemetry,
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
    assert!(
        translations[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("HTTP status 503")),
        "got: {:?}",
        translations[0].error
    );
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
