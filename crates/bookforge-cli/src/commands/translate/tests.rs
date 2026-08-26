use super::*;
use bookforge_core::{
    ir::{BlockId, SectionId},
    segment::{
        BlockTranslation, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId,
        SegmentMetadata, SegmentSource, SegmentTextRun,
    },
};
use std::{
    fs,
    io::Write,
    sync::Mutex,
    time::{Duration, SystemTime},
};

#[derive(Default)]
struct RecordingProgressSink {
    events: Mutex<Vec<bookforge_core::ProgressEvent>>,
}

impl bookforge_core::ProgressSink for RecordingProgressSink {
    fn emit(&self, event: bookforge_core::ProgressEvent) {
        self.events.lock().unwrap().push(event);
    }
}

const OPENAI_SCENARIO_ATTEMPTS: usize = 5;
const OPENAI_SCENARIO_SETTLE_DELAY: Duration = Duration::from_millis(150);

/// Windows loopback connections are intermittently reset under heavy thread
/// churn (observed ~10% of fresh connections on a 12-core box under CPU
/// saturation, independent of harness behavior). These resets are
/// environmental, so transport-level failures get whole-scenario retries
/// before the test treats them as a real regression.
fn is_transient_transport_error(error: &anyhow::Error) -> bool {
    let detail = format!("{error:#}").to_ascii_lowercase();
    detail.contains("connection reset")
        || detail.contains("forcibly closed")
        || detail.contains("10054")
        || detail.contains("broken pipe")
        || detail.contains("10038")
        || detail.contains("error decoding response body")
}

#[test]
fn toki_pona_text_only_retry_guidance_downgrades_only_the_targeted_batch() {
    let mut retry_segment = segment("retry_seg", 0);
    retry_segment.source.blocks[0].text = "<m1>Source</m1>".to_string();
    retry_segment.source.text = retry_segment.source.blocks[0].text.clone();
    let batch_config = bookforge_core::config::BatchConfig {
        enabled: true,
        target_tokens: 400,
        max_items: 4,
        adaptive_sizing: false,
        split_on_json_failure: true,
        repair_invalid_items: true,
    };
    let mut batches =
        build_translation_batches(&[retry_segment], &batch_config, TranslationProfile::V1Fast);
    assert_eq!(batches[0].mode, BatchMode::MarkerSafe);

    let mut config = TranslationRunConfig {
        source_language: Some("Italian".to_string()),
        target_language: "Toki Pona".to_string(),
        provider: "mock".to_string(),
        model: "mock".to_string(),
        prompt_version: "v1".to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig::default(),
        profile: TranslationProfile::V1Fast,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        compact_prompts: true,
        glossary: GlossaryRunConfig::default(),
        context: ContextRunConfig::default(),
        context_registry: None,
        style: None,
        entities: None,
        pause_signal: None,
        runtime_settings: None,
    };
    config.glossary.guidance_by_segment.insert(
        "retry_seg".to_string(),
        "[bookforge:text-only] preserve meaning over inline formatting".to_string(),
    );

    apply_text_only_retry_guidance(&mut batches, &config);

    assert_eq!(batches[0].mode, BatchMode::TurboTextOnly);
    assert!(batches[0].items[0].source_text.contains("m1"));
    assert_eq!(batches[0].items[0].required_markers, ["m1"]);
}

#[tokio::test]
async fn scheduler_guard_preserves_completed_segments_on_run_level_error() {
    let db_path = temp_path("jobs.sqlite");
    let input_path = temp_path("input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");
    let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    store
        .save_translation(bookforge_store::SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Gia fatto",
            blocks: &[],
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(2),
            tokens_estimated: false,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("completed segment should save");
    let config = TranslationRunConfig {
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        provider: "mock".to_string(),
        model: "mock-prefix".to_string(),
        prompt_version: "v1".to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: 0,
            max_attempts: 1,
        },
        profile: TranslationProfile::Balanced,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        compact_prompts: false,
        glossary: GlossaryRunConfig::default(),
        context: ContextRunConfig::default(),
        context_registry: None,
        style: None,
        entities: None,
        pause_signal: None,
        runtime_settings: None,
    };

    let error = translate_with_scheduler_guard(
        MockProvider::new(MockMode::PrefixTarget, "Italian"),
        &store,
        &job.id,
        &segments,
        &config,
    )
    .await
    .expect_err("zero concurrency is a scheduler-level error");

    assert!(
        error
            .to_string()
            .contains("before producing per-segment results")
    );
    let summary = store
        .summary(&job.id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.succeeded, 1);

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[tokio::test]
async fn mock_and_openai_entry_points_share_events_and_artifacts() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = temp.path().join("input.epub");
    build_translation_fixture(&input);
    let mock_output = temp.path().join("mock.epub");
    let mock_store_path = temp.path().join("mock.sqlite");

    let mut settings = TranslationProfile::V1Fast.resolve();
    settings.batch.enabled = false;
    settings.scheduler.concurrency = 1;
    settings.scheduler.max_attempts = 1;
    settings.provider.provider_max_attempts = 1;
    settings.provider.json_mode = bookforge_core::JsonMode::PromptOnly;

    let mut mock_args = translate_args_with_preset(None);
    mock_args.input = input.clone();
    mock_args.out = Some(mock_output.clone());
    mock_args.language.target = "English".to_string();
    mock_args.provider.provider = "mock".to_string();
    mock_args.provider.model = Some("mock-prefix-target".to_string());
    let mock_config = TranslationConfig {
        source_language: Some("English".to_string()),
        target_language: "English".to_string(),
        provider: "mock".to_string(),
        model: Some("mock-prefix-target".to_string()),
        concurrency: 1,
        max_attempts: 1,
        output: mock_output.clone(),
    };
    let mock_progress = Arc::new(RecordingProgressSink::default());
    orchestration::run_mock_translation_with_store(
        &input,
        &mock_config,
        &mock_args.provider,
        &mock_args,
        &settings,
        None,
        &tokio_util::sync::CancellationToken::new(),
        mock_progress.clone(),
        Some(JobStore::open(&mock_store_path).unwrap()),
    )
    .await
    .expect("mock entry point should finish");

    // Every attempt owns a fresh server port, store, output EPUB, and event
    // sink so a retried scenario cannot leak artifacts from a failed attempt
    // into the assertions below.
    let mut last_failure = String::from("no OpenAI-compatible scenario output was produced");
    let mut attempt_outcome = None;
    for attempt_index in 0..OPENAI_SCENARIO_ATTEMPTS {
        if attempt_index > 0 {
            tokio::time::sleep(OPENAI_SCENARIO_SETTLE_DELAY).await;
        }
        let openai_store_path = temp.path().join(format!("openai-{attempt_index}.sqlite"));
        let openai_output = temp.path().join(format!("openai-{attempt_index}.epub"));
        let openai_progress = Arc::new(RecordingProgressSink::default());
        let (base_url, server) = spawn_openai_prefix_server().await;
        let mut openai_args = translate_args_with_preset(None);
        openai_args.input = input.clone();
        openai_args.out = Some(openai_output.clone());
        openai_args.language.target = "English".to_string();
        openai_args.provider.provider = "openai-compatible".to_string();
        openai_args.provider.model = Some("test-model".to_string());
        openai_args.provider.base_url = Some(base_url);
        openai_args.provider.api_key_env = Some("OLLAMA_API_KEY".to_string());
        let openai_config = TranslationConfig {
            source_language: Some("English".to_string()),
            target_language: "English".to_string(),
            provider: "openai-compatible".to_string(),
            model: Some("test-model".to_string()),
            concurrency: 1,
            max_attempts: 1,
            output: openai_output.clone(),
        };
        let result = orchestration::run_openai_compatible_translation_with_store(
            &input,
            &openai_config,
            &openai_args.provider,
            &openai_args,
            &settings,
            None,
            &tokio_util::sync::CancellationToken::new(),
            openai_progress.clone(),
            Some(JobStore::open(&openai_store_path).unwrap()),
        )
        .await;
        server.abort();

        match result {
            Ok(()) => {
                attempt_outcome = Some((openai_progress, openai_store_path, openai_output));
                break;
            }
            Err(error) if is_transient_transport_error(&error) => {
                last_failure =
                    format!("transient loopback failure on attempt {attempt_index}: {error:#}");
            }
            Err(error) => panic!("OpenAI-compatible entry point should finish: {error:#}"),
        }
    }
    let Some((openai_progress, openai_store_path, openai_output)) = attempt_outcome else {
        panic!(
            "OpenAI-compatible scenario kept failing on transient transport errors: {last_failure}"
        );
    };
    let openai_job_id = openai_progress
        .events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            bookforge_core::ProgressEvent::JobCreated { job_id, .. } => Some(job_id.clone()),
            _ => None,
        })
        .unwrap();
    let openai_records = JobStore::open(&openai_store_path)
        .unwrap()
        .segment_records(&openai_job_id)
        .unwrap();
    assert!(
        openai_records.iter().all(|record| record.error.is_none()),
        "OpenAI-compatible stub should produce valid translations: {openai_records:?}"
    );

    assert_eq!(
        fs::read(mock_output).unwrap(),
        fs::read(openai_output).unwrap(),
        "provider construction must not change the rebuilt EPUB"
    );

    let mock_events = progress_event_kinds(&mock_progress.events.lock().unwrap());
    let openai_events = progress_event_kinds(&openai_progress.events.lock().unwrap())
        .into_iter()
        .filter(|kind| kind != "RuntimeConfigResolved")
        .collect::<Vec<_>>();

    // The checkpoint writer is a separate actor so the hot path never blocks on
    // SQLite, which means `CheckpointQueued` and `CheckpointFlushed` interleave
    // nondeterministically with the scheduler's own events. Asserting a total
    // order over them encodes an invariant the system deliberately does not
    // have, and fails intermittently under different scheduling. Compare the
    // full multiset — so no event can be silently lost or duplicated — and the
    // order of everything else.
    fn kind_counts(kinds: &[String]) -> std::collections::BTreeMap<&str, usize> {
        let mut counts = std::collections::BTreeMap::new();
        for kind in kinds {
            *counts.entry(kind.as_str()).or_insert(0) += 1;
        }
        counts
    }
    assert_eq!(
        kind_counts(&mock_events),
        kind_counts(&openai_events),
        "both entry points must emit the same events; the only intentional \
         difference is runtime provider resolution"
    );

    const CHECKPOINT_ACTOR_EVENTS: [&str; 2] = ["CheckpointQueued", "CheckpointFlushed"];
    let ordered = |kinds: &[String]| {
        kinds
            .iter()
            .filter(|kind| !CHECKPOINT_ACTOR_EVENTS.contains(&kind.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ordered(&mock_events),
        ordered(&openai_events),
        "the deterministic event sequence must match between entry points"
    );
}

#[tokio::test]
async fn fallback_pass_honors_stop_control_file() {
    let db_path = temp_path("fallback_stop.sqlite");
    let input_path = temp_path("fallback_stop_input.epub");
    let output_path = temp_path("fallback_stop_output.epub");
    let control_path = temp_path("fallback_stop_control");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
    bookforge_core::write_control_file(&control_path, bookforge_core::ControlCommand::Stop)
        .expect("stop control should write");

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &output_path,
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "openai-compatible",
            model: "primary-model",
            base_url: Some("https://127.0.0.1:9/v1"),
            api_key_env: Some("OPENAI_API_KEY"),
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");
    let segments = vec![segment("seg_fallback", 0)];
    store
        .insert_segments(
            &job.id,
            &segments,
            "v1",
            "openai-compatible",
            "primary-model",
            "test_ns",
        )
        .expect("segments should insert");
    let translations = vec![translation_for(
        &segments[0],
        "failed before fallback",
        "failed",
        SegmentStatus::Failed,
    )];
    let mut args = translate_args_with_preset(None);
    args.fallback_provider = Some("openai-compatible".to_string());
    args.fallback_model = Some("fallback-model".to_string());
    args.fallback_base_url = Some("https://127.0.0.1:9/v1".to_string());
    args.fallback_api_key_env = Some("OPENAI_API_KEY".to_string());

    let mut settings = TranslationProfile::V1Fast.resolve();
    settings.provider.timeout_seconds = 1;
    settings.provider.provider_max_attempts = 1;
    let fallback_config = FallbackPassConfig {
        provider: args.fallback_provider.clone().unwrap(),
        model: args.fallback_model.clone().unwrap(),
        base_url: args.fallback_base_url.clone(),
        api_key_env: args.fallback_api_key_env.clone(),
        scope: args.fallback_only,
    };
    let run_config = TranslationRunConfig {
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        provider: "openai-compatible".to_string(),
        model: "primary-model".to_string(),
        prompt_version: "v1".to_string(),
        temperature: 0.2,
        scheduler: SchedulerConfig {
            concurrency: 1,
            max_attempts: 1,
        },
        profile: settings.profile,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        compact_prompts: false,
        glossary: GlossaryRunConfig::default(),
        context: ContextRunConfig::default(),
        context_registry: None,
        style: None,
        entities: None,
        pause_signal: None,
        runtime_settings: None,
    };
    let mut control = crate::control::ControlFilePoller::new_with_path(
        &store,
        &job.id,
        control_path.clone(),
        Arc::new(NullProgressSink),
    );
    let result = run_fallback_pass(
        &tokio_util::sync::CancellationToken::new(),
        Some(&fallback_config),
        &segments,
        translations,
        &store,
        &job.id,
        "v1",
        &settings,
        &run_config,
        Some(&mut control),
        Arc::new(NullProgressSink),
        false,
    )
    .await
    .expect("fallback should stop before provider request");

    assert_eq!(result.len(), 1);
    assert_eq!(result[0].status, SegmentStatus::Failed);
    assert_eq!(
        store.get_job(&job.id).unwrap().unwrap().status,
        "stopped",
        "fallback stop control should mark the job stopped"
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
    let _ = fs::remove_file(output_path);
    let _ = fs::remove_file(control_path);
}

#[test]
fn mark_job_finished_keeps_queued_segments_needs_review() {
    let db_path = temp_path("queued_finish.sqlite");
    let input_path = temp_path("queued_finish_input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("queued_finish_output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");
    let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    let translation = translation_for(
        &segments[0],
        "Gia fatto",
        "translate_segment",
        SegmentStatus::Succeeded,
    );
    store
        .save_translation(bookforge_store::SaveTranslation {
            job_id: &job.id,
            segment_id: &translation.segment_id.0,
            translated_text: &translation.joined_text(),
            blocks: &translation.blocks,
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(2),
            tokens_estimated: false,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("completed segment should save");

    assert!(mark_job_finished(&store, &job.id, &[translation]).expect("job finish should succeed"));

    let job = store
        .get_job(&job.id)
        .expect("job should load")
        .expect("job should exist");
    assert_eq!(job.status, "needs_review");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn glossary_file_is_selected_for_matching_segment() {
    let db_path = temp_path("glossary_prepare.sqlite");
    let glossary_path = temp_path("glossary.toml");
    fs::write(
        &glossary_path,
        r#"
[meta]
schema_version = 1
source_language = "English"
target_language = "Italian"

[meta.scope]
kind = "book"
id = "fellowship"

[[term]]
source = "Aragorn"
target = "Aragorn"
category = "person"
case_sensitive = true
"#,
    )
    .expect("glossary fixture should write");
    let store = JobStore::open(&db_path).expect("store should open");
    let mut segment = segment("seg_a", 0);
    segment.source.text = "Aragorn entered the room.".to_string();
    segment.source.blocks[0].text = segment.source.text.clone();

    let prepared = prepare_glossary_run_config(
        &store,
        std::slice::from_ref(&glossary_path),
        Some("English"),
        "Italian",
        Some("fellowship"),
        None,
        GlossaryFormat::Json,
        800,
        None,
        &[segment],
    )
    .expect("glossary should prepare");

    let entries = &prepared.run_config.entries_by_segment["seg_a"];
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, "Aragorn");
    assert_eq!(entries[0].target, "Aragorn");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(glossary_path);
}

#[test]
fn persisted_glossary_is_selected_when_source_is_auto() {
    let db_path = temp_path("glossary_auto_source.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");
    store
        .upsert_glossary_terms(&[GlossaryTerm {
            id: None,
            scope_kind: bookforge_core::GlossaryScopeKind::Book,
            scope_id: Some("fellowship".to_string()),
            source_text: "Aragorn".to_string(),
            target_text: "Aragorn".to_string(),
            category: bookforge_core::GlossaryCategory::Person,
            notes: None,
            case_sensitive: true,
            always_active: false,
            status: bookforge_core::GlossaryStatus::UserSeeded,
            source_language: "English".to_string(),
            target_language: "Italian".to_string(),
            source_count: 0,
        }])
        .expect("persisted glossary should insert");
    let mut segment = segment("seg_auto", 0);
    segment.source.text = "Aragorn entered the room.".to_string();
    segment.source.blocks[0].text = segment.source.text.clone();

    let prepared = prepare_glossary_run_config(
        &store,
        &[],
        None,
        "Italian",
        Some("fellowship"),
        None,
        GlossaryFormat::Json,
        800,
        None,
        &[segment],
    )
    .expect("glossary should prepare without explicit source");

    let entries = &prepared.run_config.entries_by_segment["seg_auto"];
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].source, "Aragorn");
    assert_eq!(entries[0].target, "Aragorn");

    let _ = fs::remove_file(db_path);
}

#[test]
fn toki_pona_translation_automatically_activates_built_in_style() {
    let db_path = temp_path("toki_pona_style.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");

    let prepared = prepare_style_run_config(&store, &[], "Toki Pona", None, None)
        .expect("Toki Pona style should prepare");

    let run_config = prepared
        .run_config
        .expect("built-in style should be active");
    assert!(run_config.rendered_block.contains("Toki Pona grammar"));
    assert!(
        run_config
            .rendered_block
            .contains("Preserve every claim and logical relation")
    );
    assert_eq!(run_config.fingerprint, prepared.fingerprint);

    let _ = fs::remove_file(db_path);
}

#[test]
fn glossary_format_changes_cache_fingerprint() {
    let term = GlossaryTerm {
        id: Some(1),
        scope_kind: bookforge_core::GlossaryScopeKind::Book,
        scope_id: Some("fellowship".to_string()),
        source_text: "Aragorn".to_string(),
        target_text: "Aragorn".to_string(),
        category: bookforge_core::GlossaryCategory::Person,
        notes: None,
        case_sensitive: true,
        always_active: false,
        status: bookforge_core::GlossaryStatus::UserSeeded,
        source_language: "English".to_string(),
        target_language: "Italian".to_string(),
        source_count: 0,
    };

    let json = glossary_fingerprint(GlossaryFormat::Json, 800, None, std::slice::from_ref(&term));
    let prose = glossary_fingerprint(GlossaryFormat::Prose, 800, None, &[term]);

    assert_ne!(json, prose);
}

#[test]
fn applied_double_check_corrections_update_matching_blocks() {
    let mut translations = vec![SegmentTranslation {
        segment_id: SegmentId("seg_a".to_string()),
        ordinal: 0,
        block_ids: vec![BlockId("b_000000".to_string())],
        blocks: vec![BlockTranslation {
            block_id: BlockId("b_000000".to_string()),
            text: "vecchio testo".to_string(),
        }],
        checksum: "checksum".to_string(),
        status: SegmentStatus::Succeeded,
        template: "translate_segment".to_string(),
        error: None,
        input_tokens: Some(10),
        input_cached_tokens: Some(0),
        output_tokens: Some(12),
        tokens_estimated: false,
    }];
    let corrections = vec![
        bookforge_llm::CorrectionRecord {
            item_id: "seg_a:b_000000".to_string(),
            segment_id: SegmentId("seg_a".to_string()),
            block_id: BlockId("b_000000".to_string()),
            original_translation: "vecchio testo".to_string(),
            corrected_translation: Some("testo corretto".to_string()),
            status: bookforge_llm::CorrectionStatus::Applied,
            issues: Vec::new(),
        },
        bookforge_llm::CorrectionRecord {
            item_id: "seg_a:b_000000".to_string(),
            segment_id: SegmentId("seg_a".to_string()),
            block_id: BlockId("b_000000".to_string()),
            original_translation: "testo corretto".to_string(),
            corrected_translation: Some("non applicato".to_string()),
            status: bookforge_llm::CorrectionStatus::Unresolved,
            issues: Vec::new(),
        },
    ];

    let changed = apply_double_check_corrections(&mut translations, &corrections);

    assert_eq!(changed, vec!["seg_a".to_string()]);
    assert_eq!(translations[0].blocks[0].text, "testo corretto");
}

#[test]
fn suspicious_qa_ignores_matching_inline_markers() {
    let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
    let translation = translation_for(
        &segment,
        "<m1>Ciao</m1>",
        "stored",
        SegmentStatus::Succeeded,
    );

    let candidates = suspicious_qa_candidates(&[segment], &[translation]);

    assert!(
        candidates.is_empty(),
        "matching inline markers alone should not make a segment suspicious"
    );
}

#[test]
fn suspicious_qa_includes_marker_id_mismatch() {
    let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
    let translation = translation_for(
        &segment,
        "<m2>Ciao</m2>",
        "stored",
        SegmentStatus::Succeeded,
    );

    let candidates = suspicious_qa_candidates(&[segment], &[translation]);

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].segment_id.0, "seg_marker");
}

#[test]
fn suspicious_qa_includes_marker_shape_mismatch() {
    let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
    let translation = translation_for(&segment, "<m1/>", "stored", SegmentStatus::Succeeded);

    let candidates = suspicious_qa_candidates(&[segment], &[translation]);

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].segment_id.0, "seg_marker");
}

#[test]
fn suspicious_qa_includes_malformed_marker() {
    let segment = marked_segment("seg_marker", 0, "<m1>Hello</m1>");
    let translation = translation_for(&segment, "<m1>Ciao", "stored", SegmentStatus::Succeeded);

    let candidates = suspicious_qa_candidates(&[segment], &[translation]);

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].segment_id.0, "seg_marker");
}

fn marked_segment(id: &str, ordinal: usize, text: &str) -> Segment {
    let mut segment = segment(id, ordinal);
    segment.source.text = text.to_string();
    segment.source.blocks[0].text = text.to_string();
    segment.constraints.preserve_markers = bookforge_core::marker::marker_ids_in_text(text);
    segment
}

fn translation_for(
    segment: &Segment,
    text: &str,
    template: &str,
    status: SegmentStatus,
) -> SegmentTranslation {
    SegmentTranslation {
        segment_id: segment.id.clone(),
        ordinal: segment.ordinal,
        block_ids: segment.block_ids.clone(),
        blocks: vec![BlockTranslation {
            block_id: segment.source.blocks[0].block_id.clone(),
            text: text.to_string(),
        }],
        checksum: segment.checksum.clone(),
        status,
        template: template.to_string(),
        error: None,
        input_tokens: Some(1),
        input_cached_tokens: Some(0),
        output_tokens: Some(1),
        tokens_estimated: false,
    }
}

fn segment(id: &str, ordinal: usize) -> Segment {
    let block_id = BlockId(format!("b_{ordinal:06}"));
    Segment {
        id: SegmentId(id.to_string()),
        section_id: SectionId("sec_000000".to_string()),
        ordinal,
        block_ids: vec![block_id.clone()],
        source: SegmentSource {
            text: format!("Source {ordinal}"),
            blocks: vec![SegmentBlock {
                block_id,
                kind: "paragraph".to_string(),
                text: format!("Source {ordinal}"),
                text_runs: vec![SegmentTextRun {
                    id: format!("r{ordinal}"),
                    text: format!("Source {ordinal}"),
                }],
                protected_spans: Vec::new(),
            }],
            token_estimate: 2,
        },
        context: SegmentContext::default(),
        metadata: SegmentMetadata::default(),
        constraints: SegmentConstraints::default(),
        checksum: format!("checksum_{ordinal}"),
    }
}

fn temp_path(name: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Counter disambiguates calls that share a timestamp (the clock is coarse
    // on some platforms), keeping parallel tests off each other's temp files.
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "bookforge-cli-test-{}-{nanos}-{seq}-{name}",
        std::process::id()
    ))
}

fn build_translation_fixture(path: &std::path::Path) {
    build_epub_fixture(path, "en", &["Hello world.".to_string()]);
}

/// Writes a minimal single-chapter EPUB whose body is `paragraphs`.
///
/// The preflight tests need real books rather than in-memory `Book` values,
/// because the point is to exercise parse -> segmentation -> plan. They must not
/// reach into `tests/`: that directory is gitignored (it holds the owner's real
/// books), so anything reading from it passes locally and fails in CI.
fn build_epub_fixture(path: &std::path::Path, language: &str, paragraphs: &[String]) {
    let body = paragraphs
        .iter()
        .map(|paragraph| format!("<p>{paragraph}</p>"))
        .collect::<String>();
    let file = fs::File::create(path).expect("fixture EPUB should be creatable");
    let mut zip = zip::ZipWriter::new(file);
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let deflated = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#,
    )
    .unwrap();
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">orchestration-fixture</dc:identifier>
    <dc:title></dc:title>
    <dc:language>{language}</dc:language>
  </metadata>
  <manifest>
    <item id="chapter" href="chapter.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="chapter"/></spine>
</package>"#
        )
        .as_bytes(),
    )
    .unwrap();
    zip.start_file("chapter.xhtml", deflated).unwrap();
    zip.write_all(
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Chapter</title></head>
<body>{body}</body>
</html>"#
        )
        .as_bytes(),
    )
    .unwrap();
    zip.finish().unwrap();
}

async fn spawn_openai_prefix_server() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test server should bind");
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            let header_end = loop {
                let Ok(read) = stream.read(&mut chunk).await else {
                    break None;
                };
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break Some(index + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                })
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let Ok(read) = stream.read(&mut chunk).await else {
                    break;
                };
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }

            let body: serde_json::Value =
                serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
            let user = body["messages"][1]["content"].as_str().unwrap();
            let user = user.replace("\r\n", "\n");
            let segment_id = user
                .rsplit_once(r#""segment_id": ""#)
                .and_then(|(_, value)| value.split('"').next())
                .unwrap();
            let completion =
                if let Some((_, source_blocks)) = user.split_once("Source blocks:\n\n```json\n") {
                    let source_blocks = source_blocks.split_once("\n```").unwrap().0;
                    let blocks = serde_json::from_str::<Vec<serde_json::Value>>(source_blocks)
                        .unwrap()
                        .into_iter()
                        .map(|block| {
                            serde_json::json!({
                                "block_id": block["block_id"],
                                "translation": format!(
                                    "[English] {}",
                                    block["text"].as_str().unwrap()
                                ),
                            })
                        })
                        .collect::<Vec<_>>();
                    serde_json::json!({"segment_id": segment_id, "blocks": blocks}).to_string()
                } else {
                    let source = user
                        .split_once("Source segment:\n\n```txt\n")
                        .and_then(|(_, value)| value.split_once("\n```"))
                        .map(|(source, _)| source)
                        .unwrap();
                    serde_json::json!({
                        "segment_id": segment_id,
                        "translation": format!("[English] {source}"),
                    })
                    .to_string()
                };
            let response_body = serde_json::json!({
                "choices": [{
                    "message": {"content": completion},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1}
            })
            .to_string();
            let response_headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(response_headers.as_bytes()).await.unwrap();
            stream.write_all(response_body.as_bytes()).await.unwrap();
            let _ = stream.shutdown().await;
        }
    });

    (format!("http://{address}"), server)
}

fn progress_event_kinds(events: &[bookforge_core::ProgressEvent]) -> Vec<String> {
    events
        .iter()
        .map(|event| {
            serde_json::to_value(event)
                .unwrap()
                .as_object()
                .and_then(|object| object.keys().next())
                .expect("progress event should serialize as a tagged object")
                .clone()
        })
        .collect()
}

#[test]
fn provider_config_sets_provider_max_attempts() {
    use bookforge_core::RetryAfterPolicy;
    let cfg = provider_config(
        "openrouter",
        None,
        None,
        None,
        120,
        2,
        false,
        RetryAfterPolicy::JitteredExponential,
        30,
        32,
        bookforge_core::JsonMode::Auto,
    )
    .expect("provider_config should build");
    assert_eq!(cfg.provider_max_attempts, 2);

    let cfg = provider_config(
        "openrouter",
        None,
        None,
        None,
        120,
        0,
        false,
        RetryAfterPolicy::JitteredExponential,
        30,
        32,
        bookforge_core::JsonMode::Auto,
    )
    .expect("provider_config should build");
    assert_eq!(cfg.provider_max_attempts, 1);
}

#[test]
fn provider_config_uses_resolved_json_mode() {
    let cfg = provider_config(
        "openrouter",
        None,
        None,
        None,
        120,
        1,
        true,
        bookforge_core::RetryAfterPolicy::JitteredExponential,
        15,
        64,
        bookforge_core::JsonMode::PromptOnly,
    )
    .expect("provider_config should build");

    assert_eq!(cfg.json_mode, bookforge_core::JsonMode::PromptOnly);
}

#[test]
fn provider_config_accepts_openai_compatible_with_model_and_base_url() {
    let cfg = provider_config(
        "openai-compatible",
        Some("provider/model"),
        Some("https://api.example.com/v1"),
        Some("OPENAI_API_KEY"),
        120,
        1,
        false,
        bookforge_core::RetryAfterPolicy::JitteredExponential,
        15,
        64,
        bookforge_core::JsonMode::Auto,
    )
    .expect("openai-compatible should build with model and base URL");

    assert_eq!(cfg.base_url, "https://api.example.com/v1");
    assert_eq!(cfg.model, "provider/model");
    assert_eq!(cfg.api_key_env, "OPENAI_API_KEY");
}

#[test]
fn provider_config_requires_openai_compatible_base_url_and_model() {
    let missing_base_url = provider_config(
        "openai-compatible",
        Some("provider/model"),
        None,
        None,
        120,
        1,
        false,
        bookforge_core::RetryAfterPolicy::JitteredExponential,
        15,
        64,
        bookforge_core::JsonMode::Auto,
    )
    .expect_err("missing base URL should fail");
    assert!(
        missing_base_url
            .to_string()
            .contains("--base-url is required")
    );

    let missing_model = provider_config(
        "openai-compatible",
        None,
        Some("https://api.example.com/v1"),
        None,
        120,
        1,
        false,
        bookforge_core::RetryAfterPolicy::JitteredExponential,
        15,
        64,
        bookforge_core::JsonMode::Auto,
    )
    .expect_err("missing model should fail");
    assert!(missing_model.to_string().contains("--model is required"));
}

#[test]
fn retry_amplification_warning_emitted_for_high_attempt_product() {
    let mut settings = TranslationProfile::Safe.resolve();
    settings.scheduler.max_attempts = 3;
    settings.provider.provider_max_attempts = 2;
    settings.provider.validation_max_attempts = 1;

    let warning = retry_amplification_warning(&settings).expect("warning expected");
    assert!(warning.contains("scheduler attempts 3 x provider attempts 2"));
    assert!(warning.contains("up to 6 calls"));
}

fn translate_args_with_preset(
    provider_preset: Option<bookforge_core::ProviderPreset>,
) -> TranslateArgs {
    TranslateArgs {
        input: temp_path("input.epub"),
        plan: false,
        language: LanguageArgs {
            source: Some("English".to_string()),
            target: "Italian".to_string(),
        },
        provider: CliProviderArgs {
            provider: "deepseek".to_string(),
            model: None,
            base_url: None,
            api_key_env: None,
            timeout_seconds: None,
        },
        profile: TranslationProfile::V1Fast,
        max_segment_tokens: None,
        context_tokens: None,
        batch_target_tokens: None,
        batch_max_items: None,
        compact_prompts: None,
        retry_failed_only: None,
        adaptive_concurrency: None,
        turbo_text_only: false,
        concurrency: None,
        max_attempts: None,
        provider_max_attempts: None,
        validation_max_attempts: None,
        out: None,
        creator: None,
        mode: bookforge_core::BilingualMode::Replace,
        bilingual_css: None,
        bilingual_style: bookforge_core::BilingualStyle::Minimal,
        bilingual_separator: " / ".to_string(),
        validate_output: false,
        strict_epubcheck: false,
        book_id: None,
        series_id: None,
        glossary: Vec::new(),
        glossary_budget_tokens: 800,
        glossary_format: GlossaryFormat::Json,
        prompt_extra: None,
        context_window: 0,
        context_budget_tokens: 1200,
        context_scope: bookforge_core::config::ContextScope::Chapter,
        context_strict: false,
        style: Vec::new(),
        entities: Vec::new(),
        qa: QaMode::Off,
        qa_concurrency: 8,
        qa_batch_target_tokens: None,
        qa_max_output_tokens: 8_192,
        qa_model: None,
        qa_provider: None,
        qa_base_url: None,
        qa_api_key_env: None,
        double_check: DoubleCheckMode::Off,
        double_check_model: None,
        double_check_provider: None,
        double_check_base_url: None,
        double_check_api_key_env: None,
        double_check_concurrency: 4,
        double_check_batch_target_tokens: None,
        auto_correct: false,
        correction_rounds: 1,
        fallback_provider: None,
        fallback_model: None,
        fallback_base_url: None,
        fallback_api_key_env: None,
        fallback_only: FallbackScope::Failed,
        no_thinking: false,
        model_context_tokens: None,
        max_output_tokens: None,
        batch_max_output_tokens: None,
        json_mode: bookforge_core::JsonMode::Auto,
        ui: crate::progress::UiMode::Quiet,
        progress_jsonl: None,
        provider_preset,
    }
}

/// A book-shaped EPUB in one of two scripts, for the preflight tests.
///
/// Both scripts get the same paragraph count and the same character count per
/// paragraph, so the only thing that differs between them is the script itself.
/// That is the variable the plan routes on, and holding the rest equal is what
/// makes a difference in the resulting settings attributable to it.
///
/// The returned `TempDir` owns the file and must stay alive while it is read.
fn scripted_book_fixture(caseless: bool) -> (tempfile::TempDir, PathBuf) {
    // Long enough that the plan sees a distribution rather than a single point,
    // and that the per-paragraph token estimate lands in the range where the
    // batch bounds actually move.
    const PARAGRAPHS: usize = 60;
    const CHARACTERS_PER_PARAGRAPH: usize = 600;

    let (language, sentence) = if caseless {
        ("zh", "矛盾的普遍性和特殊性的关系是矛盾问题的精髓。")
    } else {
        ("en", "The universal and the particular in contradiction. ")
    };
    let paragraphs = (0..PARAGRAPHS)
        .map(|_| {
            sentence
                .chars()
                .cycle()
                .take(CHARACTERS_PER_PARAGRAPH)
                .collect::<String>()
        })
        .collect::<Vec<_>>();

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let path = temp.path().join(if caseless {
        "caseless.epub"
    } else {
        "cased.epub"
    });
    build_epub_fixture(&path, language, &paragraphs);
    (temp, path)
}

#[test]
fn explicit_batch_flag_beats_plan_while_unset_fields_are_filled() {
    let (_fixture, input) = scripted_book_fixture(true);
    let mut args = translate_args_with_preset(None);
    args.input = input;
    args.plan = true;
    args.provider.provider = "mock".to_string();
    args.provider.model = Some("mock-prefix-target".to_string());
    args.batch_max_items = Some(64);

    let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);
    let (settings, application) =
        orchestration::resolve_settings_and_plan(&args, &effective_provider)
            .expect("planned settings should resolve");
    let applied = application.expect("--plan should produce an application");

    assert_eq!(settings.batch.max_items, 64, "explicit flag must win");
    assert!(
        settings.batch.target_tokens < TranslationProfile::V1Fast.resolve().batch.target_tokens,
        "the unset CJK token target should be filled by the plan"
    );
    assert!(
        applied
            .applied_plan
            .decisions
            .iter()
            .any(|decision| decision.setting == "batch_target_tokens")
    );
    assert!(
        applied
            .applied_plan
            .decisions
            .iter()
            .all(|decision| decision.setting != "batch_max_items"),
        "an explicit field must not be attributed to the plan"
    );
}

#[test]
fn translate_without_plan_preserves_resolved_settings_bytes() {
    let args = translate_args_with_preset(None);
    let expected =
        bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&resolve_settings(&args));
    let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);
    let (actual, application) =
        orchestration::resolve_settings_and_plan(&args, &effective_provider)
            .expect("ordinary settings should resolve");
    let actual = bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&actual);

    assert!(application.is_none());
    assert_eq!(
        serde_json::to_vec(&actual).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&bookforge_core::FinalizeCheckpointSnapshot::default()).unwrap(),
        br#"{"double_check_complete":false}"#,
        "an absent plan must not change legacy snapshot bytes"
    );
}

#[test]
fn caseless_and_cased_translate_preflights_apply_different_settings() {
    let planned_settings = |input: &std::path::Path| {
        let mut args = translate_args_with_preset(None);
        args.input = input.to_path_buf();
        args.plan = true;
        args.provider.provider = "mock".to_string();
        args.provider.model = Some("mock-prefix-target".to_string());
        let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);
        let (settings, application) =
            orchestration::resolve_settings_and_plan(&args, &effective_provider)
                .expect("EPUB preflight should succeed");
        assert!(application.is_some());
        settings
    };

    // `plan.rs` pins the rule itself against in-memory books. This asserts the
    // same divergence survives the real path: EPUB parse, segmentation, then
    // settings resolution.
    let (_caseless_fixture, caseless_input) = scripted_book_fixture(true);
    let (_cased_fixture, cased_input) = scripted_book_fixture(false);
    let caseless = planned_settings(&caseless_input);
    let cased = planned_settings(&cased_input);

    assert!(caseless.batch.target_tokens < cased.batch.target_tokens);
    assert!(caseless.batch.max_items < cased.batch.max_items);
}

#[tokio::test]
async fn applied_plan_decisions_and_reasons_are_persisted_in_run_snapshot() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input = temp.path().join("input.epub");
    let output = temp.path().join("output.epub");
    let store_path = temp.path().join("jobs.sqlite");
    build_translation_fixture(&input);

    let mut args = translate_args_with_preset(None);
    args.input = input.clone();
    args.out = Some(output.clone());
    args.plan = true;
    args.provider.provider = "mock".to_string();
    args.provider.model = Some("mock-prefix-target".to_string());
    let effective_provider = apply_provider_preset(&args.provider, args.provider_preset);
    let (settings, application) =
        orchestration::resolve_settings_and_plan(&args, &effective_provider)
            .expect("planned settings should resolve");
    let config = TranslationConfig {
        source_language: args.language.source.clone(),
        target_language: args.language.target.clone(),
        provider: "mock".to_string(),
        model: Some("mock-prefix-target".to_string()),
        concurrency: settings.scheduler.concurrency,
        max_attempts: settings.scheduler.max_attempts,
        output,
    };
    let progress = Arc::new(RecordingProgressSink::default());
    orchestration::run_mock_translation_with_store(
        &input,
        &config,
        &effective_provider,
        &args,
        &settings,
        application,
        &tokio_util::sync::CancellationToken::new(),
        progress.clone(),
        Some(JobStore::open(&store_path).unwrap()),
    )
    .await
    .expect("mock translation should finish");

    let job_id = progress
        .events
        .lock()
        .unwrap()
        .iter()
        .find_map(|event| match event {
            bookforge_core::ProgressEvent::JobCreated { job_id, .. } => Some(job_id.clone()),
            _ => None,
        })
        .expect("job creation should be reported");
    let snapshot = JobStore::open(&store_path)
        .unwrap()
        .load_job_config_snapshot(&job_id)
        .unwrap()
        .expect("run snapshot should exist");
    let applied = snapshot
        .finalize
        .applied_plan
        .expect("run snapshot should record the applied plan");

    assert_eq!(
        applied.schema_version,
        crate::commands::plan::PLAN_SCHEMA_VERSION
    );
    assert!(!applied.decisions.is_empty());
    assert!(
        applied
            .decisions
            .iter()
            .all(|decision| !decision.reason.trim().is_empty())
    );
}

#[test]
fn explicit_cli_concurrency_overrides_provider_preset() {
    let mut args =
        translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterPaidFast));
    args.concurrency = Some(8);
    let settings = resolve_settings(&args);
    assert_eq!(settings.scheduler.concurrency, 8);
}

#[test]
fn explicit_cli_provider_max_attempts_overrides_provider_preset() {
    let mut args =
        translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterPaidFast));
    args.provider_max_attempts = Some(3);
    let settings = resolve_settings(&args);
    assert_eq!(settings.provider.provider_max_attempts, 3);
}

#[test]
fn provider_preset_runtime_is_reflected_in_resolved_settings() {
    let args = translate_args_with_preset(Some(bookforge_core::ProviderPreset::OpenRouterFree));
    let settings = resolve_settings(&args);
    assert_eq!(settings.scheduler.concurrency, 2);
    assert_eq!(
        settings.provider.retry_after_policy,
        bookforge_core::RetryAfterPolicy::RespectHeader
    );
    assert_eq!(settings.provider.max_idle_per_host, 8);
}

#[test]
fn toki_pona_style_applies_expansion_aware_sizing_before_first_request() {
    let mut args = translate_args_with_preset(None);
    args.language.target = "Toki Pona".to_string();

    let settings = resolve_settings(&args);
    assert_eq!(settings.segmentation.max_segment_tokens, 200);
    assert_eq!(settings.batch.target_tokens, 200);
    assert_eq!(settings.batch.max_items, 1);
    assert!(!settings.batch.adaptive_sizing);
}

#[test]
fn explicit_toki_pona_sizing_overrides_built_in_style_policy() {
    let mut args = translate_args_with_preset(None);
    args.language.target = "Toki Pona".to_string();
    args.max_segment_tokens = Some(2_000);
    args.batch_target_tokens = Some(2_500);
    args.batch_max_items = Some(24);

    let settings = resolve_settings(&args);
    assert_eq!(settings.segmentation.max_segment_tokens, 2_000);
    assert_eq!(settings.batch.target_tokens, 2_500);
    assert_eq!(settings.batch.max_items, 24);
}
