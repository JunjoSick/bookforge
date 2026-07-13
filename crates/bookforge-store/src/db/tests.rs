use super::*;
use bookforge_core::{
    ir::{BlockId, SectionId},
    segment::{
        Segment, SegmentBlock, SegmentConstraints, SegmentContext, SegmentId, SegmentMetadata,
        SegmentSource, SegmentTextRun,
    },
};

#[test]
fn store_reuses_connection_across_job_operations() {
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
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Tradotto",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(11),
            input_cached_tokens: Some(0),
            output_tokens: Some(7),
            tokens_estimated: false,
        })
        .expect("translation should save");
    store
        .mark_segment_failed(&job.id, "seg_b", "provider unavailable")
        .expect("segment should be marked failed");

    let summary = store
        .summary(&job.id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.total_segments, 2);
    assert_eq!(summary.succeeded, 1);
    assert_eq!(summary.failed, 1);
    assert_eq!(summary.input_tokens, 11);
    assert_eq!(summary.output_tokens, 7);
    let blocks = store
        .load_block_translations(&job.id)
        .expect("block translations should load");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].text, "Tradotto");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn job_status_allows_pause_resume_and_stop() {
    let db_path = temp_path("pause_status.sqlite");
    let input_path = temp_path("pause_input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("pause_output.epub"),
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

    store.mark_job_running(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");
    store.mark_job_paused(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "paused");
    let segments = vec![segment("seg_pause", 0)];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .unwrap();
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_pause",
            translated_text: "Tradotto",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            tokens_estimated: false,
        })
        .unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "paused");
    store.mark_job_running(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");
    store.mark_job_stopped(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "stopped");
    store.mark_job_paused(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "stopped");
    store.mark_job_running(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "stopped");
    store.mark_job_complete(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "stopped");
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_pause",
            translated_text: "Tradotto ancora",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto ancora".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            tokens_estimated: false,
        })
        .unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "stopped");
    store.mark_job_running_for_resume(&job.id).unwrap();
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
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
    std::env::temp_dir().join(format!(
        "bookforge-store-test-{}-{}-{name}",
        std::process::id(),
        unix_timestamp_nanos()
    ))
}

#[test]
fn global_style_sheet_upsert_updates_null_scope_row() {
    let db_path = temp_path("style_global_upsert.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let first = NewStyleSheet {
        scope_kind: bookforge_core::GlossaryScopeKind::Global,
        scope_id: None,
        target_language: "Italian",
        content_toml: "first",
        fingerprint: "fp1",
    };
    let second = NewStyleSheet {
        content_toml: "second",
        fingerprint: "fp2",
        ..first
    };

    let first_id = store
        .upsert_style_sheet(&first)
        .expect("first style upsert");
    let second_id = store
        .upsert_style_sheet(&second)
        .expect("second style upsert");

    assert_eq!(first_id, second_id);
    let rows = store
        .list_style_sheets(
            Some("Italian"),
            Some(bookforge_core::GlossaryScopeKind::Global),
            None,
        )
        .expect("style rows list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].content_toml, "second");
    assert_eq!(rows[0].fingerprint, "fp2");

    let _ = fs::remove_file(db_path);
}

#[test]
fn global_entity_upsert_updates_null_scope_row() {
    let db_path = temp_path("entity_global_upsert.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let first = NewEntity {
        scope_kind: bookforge_core::GlossaryScopeKind::Global,
        scope_id: None,
        source_name: "Ivan",
        target_name: "Ivan",
        gender_target: Some(bookforge_core::entity::EntityGender::Masculine),
        role: Some("first"),
        notes: Some("old"),
        source_language: "English",
        target_language: "Italian",
    };
    let second = NewEntity {
        target_name: "Giovanni",
        role: Some("second"),
        notes: Some("new"),
        ..first
    };

    assert_eq!(store.upsert_entities(&[first]).expect("first entity"), 1);
    assert_eq!(store.upsert_entities(&[second]).expect("second entity"), 1);

    let rows = store
        .list_entities(
            Some("English"),
            Some("Italian"),
            Some(bookforge_core::GlossaryScopeKind::Global),
            None,
        )
        .expect("entity rows list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source_name, "Ivan");
    assert_eq!(rows[0].target_name, "Giovanni");
    assert_eq!(rows[0].role.as_deref(), Some("second"));
    assert_eq!(rows[0].notes.as_deref(), Some("new"));

    let _ = fs::remove_file(db_path);
}

fn build_seeded_store_with_translation(
    db_path: &PathBuf,
    cache_namespace: &str,
    block_ids: &[&str],
) -> (JobStore, JobRecord, Segment) {
    let input_path = temp_path("input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");

    let store = JobStore::open(db_path).expect("store should open");
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

    let mut seg = segment("seg_a", 0);
    let blocks: Vec<BlockTranslation> = block_ids
        .iter()
        .map(|id| BlockTranslation {
            block_id: BlockId(id.to_string()),
            text: format!("Tradotto {id}"),
        })
        .collect();
    seg.block_ids = block_ids.iter().map(|id| BlockId(id.to_string())).collect();

    store
        .insert_segments(
            &job.id,
            std::slice::from_ref(&seg),
            "v1",
            "mock",
            "mock-prefix",
            cache_namespace,
        )
        .expect("segments should insert");
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Tradotto",
            blocks: &blocks,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(11),
            input_cached_tokens: Some(0),
            output_tokens: Some(7),
            tokens_estimated: false,
        })
        .expect("translation should save");

    let _ = fs::remove_file(input_path);
    (store, job, seg)
}

#[test]
fn job_config_snapshot_round_trips_through_store() {
    let db_path = temp_path("snapshot.sqlite");
    let input_path = temp_path("input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "openrouter",
            model: "model",
            base_url: Some("https://example.test/v1"),
            api_key_env: Some("OPENROUTER_API_KEY"),
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");
    let settings = bookforge_core::TranslationProfile::Balanced.resolve();
    let snapshot = RunConfigSnapshot {
        input_path: input_path.clone(),
        input_snapshot_path: Some(temp_path("input-snapshot.epub")),
        input_sha256: Some("abc123".to_string()),
        output_path: temp_path("translated.epub"),
        events_path: Some(temp_path("events.jsonl")),
        report_json_path: Some(temp_path("report.json")),
        report_markdown_path: Some(temp_path("report.md")),
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        provider: "openrouter".to_string(),
        model: "model".to_string(),
        base_url: Some("https://example.test/v1".to_string()),
        api_key_env: Some("OPENROUTER_API_KEY".to_string()),
        profile: settings.profile,
        provider_preset: None,
        prompt_version: "batch_v1".to_string(),
        cache_namespace: "cache_ns".to_string(),
        book_id: None,
        series_id: None,
        glossary_budget_tokens: 800,
        glossary_format: bookforge_core::GlossaryFormat::Json,
        prompt_extra: None,
        glossary_fingerprint: String::new(),
        glossary_terms: Vec::new(),
        context_window: 0,
        context_budget_tokens: 1200,
        context_scope: bookforge_core::config::ContextScope::Chapter,
        style_fingerprint: String::new(),
        style_rendered_block: String::new(),
        entities_fingerprint: String::new(),
        entities_rendered_block: String::new(),
        bilingual_mode: bookforge_core::BilingualMode::Replace,
        bilingual_separator: " / ".to_string(),
        bilingual_style: bookforge_core::BilingualStyle::Minimal,
        bilingual_css: None,
        fallback: None,
        finalize: bookforge_core::run_snapshot::FinalizeCheckpointSnapshot::default(),
        qa_mode: "off".to_string(),
        validate_output: false,
        settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
    };

    store
        .update_job_config_snapshot(&job.id, &snapshot)
        .expect("snapshot should persist");
    let loaded = store
        .load_job_config_snapshot(&job.id)
        .expect("snapshot should load")
        .expect("snapshot should exist");
    assert_eq!(loaded, snapshot);

    let reloaded_job = store
        .get_job(&job.id)
        .expect("job should load")
        .expect("job should exist");
    assert_eq!(reloaded_job.events_path, snapshot.events_path);
    assert_eq!(reloaded_job.report_json_path, snapshot.report_json_path);
    assert_eq!(
        reloaded_job.report_markdown_path,
        snapshot.report_markdown_path
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn job_config_snapshot_does_not_store_api_key_value() {
    let db_path = temp_path("snapshot_secret.sqlite");
    let input_path = temp_path("input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
    let api_key_env = "BOOKFORGE_TEST_API_KEY_VALUE_NOT_STORED";
    let api_key_value = "sk-live-secret-that-must-not-be-persisted";
    // This test uses a unique process-local env var and verifies snapshot
    // serialization never reads the value.
    unsafe {
        std::env::set_var(api_key_env, api_key_value);
    }

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "openrouter",
            model: "model",
            base_url: Some("https://example.test/v1"),
            api_key_env: Some(api_key_env),
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");
    let settings = bookforge_core::TranslationProfile::Balanced.resolve();
    let snapshot = RunConfigSnapshot {
        input_path: input_path.clone(),
        input_snapshot_path: None,
        input_sha256: None,
        output_path: temp_path("translated.epub"),
        events_path: Some(temp_path("events.jsonl")),
        report_json_path: Some(temp_path("report.json")),
        report_markdown_path: Some(temp_path("report.md")),
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        provider: "openrouter".to_string(),
        model: "model".to_string(),
        base_url: Some("https://example.test/v1".to_string()),
        api_key_env: Some(api_key_env.to_string()),
        profile: settings.profile,
        provider_preset: None,
        prompt_version: "batch_v1".to_string(),
        cache_namespace: "cache_ns".to_string(),
        book_id: None,
        series_id: None,
        glossary_budget_tokens: 800,
        glossary_format: bookforge_core::GlossaryFormat::Json,
        prompt_extra: None,
        glossary_fingerprint: String::new(),
        glossary_terms: Vec::new(),
        context_window: 0,
        context_budget_tokens: 1200,
        context_scope: bookforge_core::config::ContextScope::Chapter,
        style_fingerprint: String::new(),
        style_rendered_block: String::new(),
        entities_fingerprint: String::new(),
        entities_rendered_block: String::new(),
        bilingual_mode: bookforge_core::BilingualMode::Replace,
        bilingual_separator: " / ".to_string(),
        bilingual_style: bookforge_core::BilingualStyle::Minimal,
        bilingual_css: None,
        fallback: None,
        finalize: bookforge_core::run_snapshot::FinalizeCheckpointSnapshot::default(),
        qa_mode: "off".to_string(),
        validate_output: false,
        settings: bookforge_core::ResolvedRunSettingsSnapshot::from_settings(&settings),
    };

    store
        .update_job_config_snapshot(&job.id, &snapshot)
        .expect("snapshot should persist");
    let raw_json = {
        let conn = store.conn.borrow();
        conn.query_row(
            "SELECT config_json FROM jobs WHERE id = ?1",
            params![job.id],
            |row| row.get::<_, String>(0),
        )
        .expect("raw snapshot JSON should load")
    };

    assert!(raw_json.contains(api_key_env));
    assert!(!raw_json.contains(api_key_value));

    unsafe {
        std::env::remove_var(api_key_env);
    }
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn terminal_loading_and_resumable_ids_preserve_lifecycle_boundaries() {
    let db_path = temp_path("terminal_resume.sqlite");
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
    let segments = vec![
        segment("seg_done", 0),
        segment("seg_cached", 1),
        segment("seg_review", 2),
        segment("seg_failed", 3),
        segment("seg_queued", 4),
    ];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "ns")
        .expect("segments should insert");
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_done",
            translated_text: "Done",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Done".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("done should save");
    store
        .save_cached_translation(SaveCachedTranslation {
            job_id: &job.id,
            segment_id: "seg_cached",
            translated_text: "Cached",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000001".to_string()),
                text: "Cached".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("cached should save");
    store
        .save_needs_review(SaveNeedsReview {
            job_id: &job.id,
            segment_id: "seg_review",
            preserved_text: "Review",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000002".to_string()),
                text: "Review".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            error: "needs eyes",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("review should save");
    store
        .mark_segment_failed(&job.id, "seg_failed", "failed")
        .expect("failed should mark");

    let terminal = store
        .load_terminal_segment_translations(&job.id)
        .expect("terminal records should load");
    let ids = terminal
        .iter()
        .map(|record| record.segment_id.as_str())
        .collect::<Vec<_>>();
    assert_eq!(ids, vec!["seg_done", "seg_cached", "seg_review"]);
    assert_eq!(terminal[0].blocks[0].block_id.0, "b_000000");
    assert_eq!(terminal[2].status, "needs_review");

    let resumable = store
        .resumable_segment_ids(&job.id)
        .expect("resumable ids should load");
    assert_eq!(resumable, vec!["seg_failed", "seg_queued"]);

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn mark_unfinished_segments_failed_preserves_terminal_segments() {
    let db_path = temp_path("unfinished_preserve.sqlite");
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
    let segments = vec![
        segment("seg_succeeded", 0),
        segment("seg_cached", 1),
        segment("seg_review", 2),
        segment("seg_queued", 3),
        segment("seg_retry", 4),
    ];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_succeeded",
            translated_text: "Done",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Done".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("succeeded segment should save");
    store
        .save_cached_translation(SaveCachedTranslation {
            job_id: &job.id,
            segment_id: "seg_cached",
            translated_text: "Cached",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000001".to_string()),
                text: "Cached".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("cached segment should save");
    store
        .save_needs_review(SaveNeedsReview {
            job_id: &job.id,
            segment_id: "seg_review",
            preserved_text: "Needs review",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000002".to_string()),
                text: "Needs review".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            error: "qa issue",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("needs-review segment should save");
    store
        .retry_segments(&job.id, RetryScope::Failed)
        .expect("retry with no failed segments should be harmless");
    {
        let conn = store.conn.borrow();
        conn.execute(
            "UPDATE segments SET status = 'retry_pending' WHERE job_id = ?1 AND id = 'seg_retry'",
            params![job.id],
        )
        .expect("test status update should work");
    }

    let candidate_ids = segments
        .iter()
        .map(|segment| segment.id.0.clone())
        .collect::<Vec<_>>();
    let changed = store
        .mark_unfinished_segments_failed(&job.id, &candidate_ids, "run failed")
        .expect("unfinished segments should be marked failed");
    assert_eq!(changed, 2);

    let records = store.segment_records(&job.id).expect("records should load");
    let statuses = records
        .into_iter()
        .map(|record| (record.id, record.status))
        .collect::<HashMap<_, _>>();
    assert_eq!(statuses["seg_succeeded"], "succeeded");
    assert_eq!(statuses["seg_cached"], "skipped_cached");
    assert_eq!(statuses["seg_review"], "needs_review");
    assert_eq!(statuses["seg_queued"], "failed");
    assert_eq!(statuses["seg_retry"], "failed");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn mark_unfinished_segments_failed_marks_only_resumable_segments() {
    let db_path = temp_path("unfinished_resumable_only.sqlite");
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
    let segments = vec![
        segment("seg_succeeded", 0),
        segment("seg_cached", 1),
        segment("seg_review", 2),
        segment("seg_failed", 3),
        segment("seg_retry", 4),
        segment("seg_queued", 5),
    ];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    {
        let conn = store.conn.borrow();
        for (id, status) in [
            ("seg_succeeded", "succeeded"),
            ("seg_cached", "skipped_cached"),
            ("seg_review", "needs_review"),
            ("seg_failed", "failed"),
            ("seg_retry", "retry_pending"),
            ("seg_queued", "queued"),
        ] {
            conn.execute(
                "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                params![status, job.id, id],
            )
            .expect("status should update");
        }
    }

    let candidate_ids = segments
        .iter()
        .map(|segment| segment.id.0.clone())
        .collect::<Vec<_>>();
    let changed = store
        .mark_unfinished_segments_failed(&job.id, &candidate_ids, "run failed")
        .expect("unfinished segments should be marked failed");

    assert_eq!(changed, 3);
    let records = store.segment_records(&job.id).expect("records should load");
    let statuses = records
        .into_iter()
        .map(|record| (record.id, record.status))
        .collect::<HashMap<_, _>>();
    assert_eq!(statuses["seg_succeeded"], "succeeded");
    assert_eq!(statuses["seg_cached"], "skipped_cached");
    assert_eq!(statuses["seg_review"], "needs_review");
    assert_eq!(statuses["seg_failed"], "failed");
    assert_eq!(statuses["seg_retry"], "failed");
    assert_eq!(statuses["seg_queued"], "failed");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn resumable_segment_ids_excludes_succeeded_cached_and_needs_review() {
    let db_path = temp_path("resumable_excludes.sqlite");
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
    let segments = vec![
        segment("seg_succeeded", 0),
        segment("seg_cached", 1),
        segment("seg_review", 2),
    ];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    {
        let conn = store.conn.borrow();
        for (id, status) in [
            ("seg_succeeded", "succeeded"),
            ("seg_cached", "skipped_cached"),
            ("seg_review", "needs_review"),
        ] {
            conn.execute(
                "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                params![status, job.id, id],
            )
            .expect("status should update");
        }
    }

    let ids = store
        .resumable_segment_ids(&job.id)
        .expect("resumable ids should load");

    assert!(ids.is_empty());
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn resumable_segment_ids_includes_failed_retry_pending_and_pending() {
    let db_path = temp_path("resumable_includes.sqlite");
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
    let segments = vec![
        segment("seg_failed", 0),
        segment("seg_retry", 1),
        segment("seg_queued", 2),
    ];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "test_ns")
        .expect("segments should insert");
    {
        let conn = store.conn.borrow();
        for (id, status) in [
            ("seg_failed", "failed"),
            ("seg_retry", "retry_pending"),
            ("seg_queued", "queued"),
        ] {
            conn.execute(
                "UPDATE segments SET status = ?1 WHERE job_id = ?2 AND id = ?3",
                params![status, job.id, id],
            )
            .expect("status should update");
        }
    }

    let ids = store
        .resumable_segment_ids(&job.id)
        .expect("resumable ids should load");

    assert_eq!(ids, vec!["seg_failed", "seg_retry", "seg_queued"]);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn cached_translation_requires_matching_cache_namespace() {
    let db_path = temp_path("ns_match.sqlite");
    let (store, _job, seg) = build_seeded_store_with_translation(&db_path, "ns_one", &["b_000000"]);

    let hit = store
        .find_cached_translation(
            &seg,
            "v1",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "ns_one",
        )
        .expect("query ok");
    assert!(hit.is_some(), "matching namespace should hit");

    let miss = store
        .find_cached_translation(
            &seg,
            "v1",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "ns_two",
        )
        .expect("query ok");
    assert!(miss.is_none(), "different namespace must not hit");

    let _ = fs::remove_file(db_path);
}

#[test]
fn cached_translation_rejects_mismatched_prompt_version() {
    // Regression test for the batch prompt v2 -> v3 bump (retry_guidance
    // field added to translate_batch_plain/marker_safe/run_preserving and
    // their compact variants). segments.prompt_version is the
    // cross-job translation cache key (see find_cached_translation and
    // find_cached_translations_batch below); a translation cached under
    // the old "v2" tag must not be served back out when the current
    // binary queries under the bumped "v3" tag, since the v2-era row was
    // produced from prompt text that never mentioned retry_guidance.
    let db_path = temp_path("prompt_version_bump.sqlite");
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

    let mut seg = segment("seg_a", 0);
    seg.block_ids = vec![BlockId("b_000000".to_string())];

    store
        .insert_segments(
            &job.id,
            std::slice::from_ref(&seg),
            "v2",
            "mock",
            "mock-prefix",
            "ns_bump",
        )
        .expect("segments should insert");
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Tradotto v2",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto v2".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v2",
            input_tokens: Some(11),
            input_cached_tokens: Some(0),
            output_tokens: Some(7),
            tokens_estimated: false,
        })
        .expect("translation should save");

    let hit_v2 = store
        .find_cached_translation(
            &seg,
            "v2",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "ns_bump",
        )
        .expect("query ok");
    assert!(
        hit_v2.is_some(),
        "row stored under v2 must still be visible to a v2 query"
    );

    let miss_v3 = store
        .find_cached_translation(
            &seg,
            "v3",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "ns_bump",
        )
        .expect("query ok");
    assert!(
        miss_v3.is_none(),
        "row stored under v2 must not be served back for a v3 (retry_guidance) query"
    );

    let batch_request_v3 = CacheLookupRequest {
        prompt_version: "v3",
        provider: "mock",
        model: "mock-prefix",
        source_lang: Some("English"),
        target_lang: "Italian",
        cache_namespace: "ns_bump",
    };
    let batch_miss = store
        .find_cached_translations_batch(std::slice::from_ref(&seg), batch_request_v3)
        .expect("batch query ok");
    assert!(
        batch_miss.is_empty(),
        "batch lookup must not return v2-era rows for a v3 query"
    );

    let batch_request_v2 = CacheLookupRequest {
        prompt_version: "v2",
        ..batch_request_v3
    };
    let batch_hit = store
        .find_cached_translations_batch(std::slice::from_ref(&seg), batch_request_v2)
        .expect("batch query ok");
    assert!(
        batch_hit.contains_key(&seg.id.0),
        "batch lookup must still return the row for a matching v2 query"
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn cached_translation_rejects_mismatched_block_ids() {
    let db_path = temp_path("blockid_match.sqlite");
    let (store, _job, mut seg) =
        build_seeded_store_with_translation(&db_path, "ns_x", &["b_000000"]);

    // Caller's segment now expects different block IDs than what was stored.
    seg.block_ids = vec![BlockId("b_999999".to_string())];

    let miss = store
        .find_cached_translation(
            &seg,
            "v1",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "ns_x",
        )
        .expect("query ok");
    assert!(
        miss.is_none(),
        "mismatched block_ids must reject the cached row"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn cached_translation_prefers_repaired_succeeded_rows_over_cached_clones() {
    let db_path = temp_path("cache_prefers_repaired.sqlite");
    let (store, _stale_job, seg) =
        build_seeded_store_with_translation(&db_path, "cache_ns", &["b_000000"]);
    let cached_input = temp_path("cached-input.epub");
    let repaired_input = temp_path("repaired-input.epub");
    fs::write(&cached_input, b"cached input").expect("cached input should be writable");
    fs::write(&repaired_input, b"repaired input").expect("repaired input should be writable");

    let cached_job = store
        .create_job(CreateJob {
            input: &cached_input,
            output: &temp_path("cached-output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("cached job should be created");
    store
        .insert_segments(
            &cached_job.id,
            std::slice::from_ref(&seg),
            "v1",
            "mock",
            "mock-prefix",
            "cache_ns",
        )
        .expect("cached job segment should insert");
    store
        .save_cached_translation(SaveCachedTranslation {
            job_id: &cached_job.id,
            segment_id: "seg_a",
            translated_text: "stale cached clone",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "stale cached clone".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("cached clone should save");

    let repaired_job = store
        .create_job(CreateJob {
            input: &repaired_input,
            output: &temp_path("repaired-output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("repaired job should be created");
    store
        .insert_segments(
            &repaired_job.id,
            std::slice::from_ref(&seg),
            "v1",
            "mock",
            "mock-prefix",
            "cache_ns",
        )
        .expect("repaired job segment should insert");
    store
        .save_translation(SaveTranslation {
            job_id: &repaired_job.id,
            segment_id: "seg_a",
            translated_text: "repaired translation",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "repaired translation".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(12),
            input_cached_tokens: Some(0),
            output_tokens: Some(8),
            tokens_estimated: false,
        })
        .expect("repaired translation should save");

    let request = CacheLookupRequest {
        prompt_version: "v1",
        provider: "mock",
        model: "mock-prefix",
        source_lang: Some("English"),
        target_lang: "Italian",
        cache_namespace: "cache_ns",
    };
    let single_hit = store
        .find_cached_translation(
            &seg,
            request.prompt_version,
            request.provider,
            request.model,
            request.source_lang,
            request.target_lang,
            request.cache_namespace,
        )
        .expect("single lookup should succeed")
        .expect("single lookup should hit");
    let batch_hit = store
        .find_cached_translations_batch(std::slice::from_ref(&seg), request)
        .expect("batch lookup should succeed")
        .remove(&seg.id.0)
        .expect("batch lookup should hit");

    assert_eq!(single_hit.translated_text, "repaired translation");
    assert_eq!(single_hit.blocks[0].text, "repaired translation");
    assert_eq!(batch_hit.translated_text, "repaired translation");
    assert_eq!(batch_hit.blocks[0].text, "repaired translation");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(cached_input);
    let _ = fs::remove_file(repaired_input);
}

#[test]
fn old_empty_cache_namespace_rows_do_not_match_new_runs() {
    let db_path = temp_path("legacy_ns.sqlite");
    // Simulate a row migrated from an older schema with the default empty namespace.
    let (store, _job, seg) = build_seeded_store_with_translation(&db_path, "", &["b_000000"]);

    let miss = store
        .find_cached_translation(
            &seg,
            "v1",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "real_ns",
        )
        .expect("query ok");
    assert!(
        miss.is_none(),
        "legacy empty-namespace row must not satisfy a real namespace lookup"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn job_store_enables_wal_and_busy_timeout() {
    let db_path = temp_path("wal_busy.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");

    let conn = store.conn.borrow();
    let journal_mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("pragma journal_mode should succeed");
    assert_eq!(
        journal_mode.to_lowercase(),
        "wal",
        "WAL journal mode must be enabled"
    );

    let busy_timeout: i64 = conn
        .pragma_query_value(None, "busy_timeout", |row| row.get(0))
        .expect("pragma busy_timeout should succeed");
    assert!(
        busy_timeout >= 5000,
        "busy_timeout should be at least 5000ms, got {busy_timeout}"
    );

    let wal_path = db_path.with_extension("sqlite-wal");
    let shm_path = db_path.with_extension("sqlite-shm");
    // WAL/shm may or may not exist depending on transactions, but the
    // journal_mode query confirms WAL is active.

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(shm_path);
}

#[test]
fn job_store_enables_foreign_keys_on_every_connection() {
    let db_path = temp_path("fk.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");

    let conn = store.conn.borrow();
    let fk_enabled: i64 = conn
        .pragma_query_value(None, "foreign_keys", |row| row.get(0))
        .expect("pragma foreign_keys should succeed");
    assert_eq!(
        fk_enabled, 1,
        "foreign_keys pragma must be ON on every connection"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn doctor_reports_wal_sidecars_as_normal_when_integrity_check_passes() {
    let db_path = temp_path("doctor.sqlite");
    let store = JobStore::open(&db_path).expect("store should open");

    // Perform a write to trigger WAL sidecar creation.
    let input_path = temp_path("input_doctor.epub");
    fs::write(&input_path, b"epub bytes").expect("test epub");
    let _job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("out_doctor.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job created");
    drop(store);

    let doctor = run_doctor(Some(db_path.clone())).expect("doctor should run");
    assert!(doctor.database_exists, "database should exist");
    assert_eq!(
        doctor.journal_mode.to_lowercase(),
        "wal",
        "journal mode should be wal"
    );
    assert_eq!(doctor.integrity_check, "ok", "integrity check should pass");
    assert!(
        doctor.wal_sidecars_normal,
        "wal sidecars should be reported as normal"
    );

    if doctor.wal_present || doctor.shm_present {
        assert!(
            !doctor.note.is_empty(),
            "doctor must explain WAL sidecars when they are present"
        );
    }

    let wal_path = db_path.with_extension("sqlite-wal");
    let shm_path = db_path.with_extension("sqlite-shm");
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(input_path);
    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(shm_path);
}

#[test]
fn checkpoint_writer_and_reader_do_not_immediately_busy_fail() {
    let db_path = temp_path("concurrent.sqlite");
    let input_path = temp_path("input_conc.epub");
    fs::write(&input_path, b"epub bytes").expect("test epub");

    // Open writer store first and create a job.
    let store_w = JobStore::open(&db_path).expect("store_w open");
    let job = store_w
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("out_conc.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job created");
    store_w
        .insert_segments(
            &job.id,
            &[segment("seg_conc", 0)],
            "v1",
            "mock",
            "mock-prefix",
            "ns",
        )
        .expect("segments inserted");

    // Open a second reader store while the first is still active.
    let store_r = JobStore::open(&db_path).expect("store_r open");
    let summary = store_r
        .summary(&job.id)
        .expect("summary should load")
        .expect("job should exist");
    assert_eq!(summary.total_segments, 1);

    let wal_path = db_path.with_extension("sqlite-wal");
    let shm_path = db_path.with_extension("sqlite-shm");
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(input_path);
    let _ = fs::remove_file(wal_path);
    let _ = fs::remove_file(shm_path);
}

#[test]
fn migrate_creates_glossary_terms_table() {
    let db_path = temp_path("glossary_migrate.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let conn = store.conn.borrow();
    let table: String = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'glossary_terms'",
            [],
            |row| row.get(0),
        )
        .expect("glossary_terms table exists");
    assert_eq!(table, "glossary_terms");
    let index: String = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_glossary_lookup'",
            [],
            |row| row.get(0),
        )
        .expect("idx_glossary_lookup exists");
    assert_eq!(index, "idx_glossary_lookup");
    let version: i64 = conn
        .query_row(
            "SELECT version FROM _migrations WHERE name = 'v1_2_glossary_terms'",
            [],
            |row| row.get(0),
        )
        .expect("v1_2 migration recorded");
    assert_eq!(version, 4);
    drop(conn);
    let _ = fs::remove_file(&db_path);
}

#[test]
fn migrate_is_idempotent_v1_2() {
    let db_path = temp_path("glossary_idem.sqlite");
    // Open twice; second open re-runs migrate() and must not error.
    {
        let _store = JobStore::open(&db_path).expect("first open");
    }
    {
        let _store = JobStore::open(&db_path).expect("second open");
    }
    let _ = fs::remove_file(&db_path);
}

#[test]
fn migrate_rebuilds_glossary_terms_with_nullable_target_text() {
    let db_path = temp_path("glossary_nullable_target.sqlite");
    {
        let conn = Connection::open(&db_path).expect("legacy db opens");
        conn.execute_batch(
            "
                CREATE TABLE _migrations (
                  version INTEGER PRIMARY KEY,
                  name TEXT NOT NULL,
                  applied_at TEXT NOT NULL
                );
                CREATE TABLE glossary_terms (
                  id INTEGER PRIMARY KEY,
                  scope_kind TEXT NOT NULL CHECK(scope_kind IN ('global', 'series', 'book')),
                  scope_id TEXT,
                  source_text TEXT NOT NULL,
                  target_text TEXT NOT NULL,
                  category TEXT NOT NULL CHECK(category IN
                    ('person', 'place', 'object', 'invented', 'style', 'phrase', 'other')),
                  notes TEXT,
                  case_sensitive INTEGER NOT NULL DEFAULT 0,
                  always_active INTEGER NOT NULL DEFAULT 0,
                  status TEXT NOT NULL CHECK(status IN
                    ('user_seeded', 'auto_candidate', 'accepted', 'rejected'))
                    DEFAULT 'user_seeded',
                  source_language TEXT NOT NULL,
                  target_language TEXT NOT NULL,
                  source_count INTEGER DEFAULT 0,
                  created_at TEXT NOT NULL,
                  updated_at TEXT NOT NULL,
                  UNIQUE(scope_kind, scope_id, source_text, source_language, target_language)
                );
                CREATE INDEX idx_glossary_lookup
                  ON glossary_terms(source_language, target_language, scope_kind, scope_id, status);
                INSERT INTO glossary_terms
                  (id, scope_kind, scope_id, source_text, target_text, category, notes,
                   case_sensitive, always_active, status, source_language, target_language,
                   source_count, created_at, updated_at)
                VALUES
                  (42, 'book', 'ivan', 'Ivan Ilych', 'Ivan Il''ich', 'person', 'legacy',
                   1, 0, 'user_seeded', 'English', 'Italian', 9, 'created', 'updated');
                ",
        )
        .expect("legacy schema should initialize");
    }

    let store = JobStore::open(&db_path).expect("store opens and migrates");
    let conn = store.conn.borrow();
    assert!(
        !table_column_is_not_null(&conn, "glossary_terms", "target_text")
            .expect("table info should load")
    );
    let row = conn
        .query_row(
            "SELECT id, target_text, created_at, updated_at FROM glossary_terms WHERE id = 42",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .expect("legacy glossary row should survive");
    assert_eq!(row.0, 42);
    assert_eq!(row.1, "Ivan Il'ich");
    assert_eq!(row.2, "created");
    assert_eq!(row.3, "updated");
    let version: i64 = conn
            .query_row(
                "SELECT version FROM _migrations WHERE name = 'v1_2_1_nullable_glossary_candidate_targets'",
                [],
                |row| row.get(0),
            )
            .expect("v1.2.1 migration recorded");
    assert_eq!(version, 5);
    let index: String = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_glossary_lookup'",
            [],
            |row| row.get(0),
        )
        .expect("idx_glossary_lookup should be recreated");
    assert_eq!(index, "idx_glossary_lookup");
    let duplicate = conn.execute(
        "INSERT INTO glossary_terms
              (scope_kind, scope_id, source_text, target_text, category, notes,
               case_sensitive, always_active, status, source_language, target_language,
               source_count, created_at, updated_at)
             VALUES
              ('book', 'ivan', 'Ivan Ilych', 'duplicate', 'person', NULL,
               1, 0, 'user_seeded', 'English', 'Italian', 1, 'now', 'now')",
        [],
    );
    assert!(
        duplicate.is_err(),
        "unique constraint should survive the rebuild"
    );
    drop(conn);
    let _ = fs::remove_file(&db_path);
}

#[test]
fn glossary_candidate_upsert_updates_auto_and_skips_rejected() {
    let db_path = temp_path("glossary_candidates.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let first = store
        .upsert_glossary_candidates(
            "ivan",
            "English",
            "Italian",
            &[NewGlossaryCandidate {
                source_text: "Ivan Ilych",
                category: GlossaryCategory::Other,
                source_count: 4,
            }],
        )
        .expect("candidate inserts");
    assert_eq!(first.inserted, 1);

    let candidates = store
        .list_glossary_candidates("ivan", "English", "Italian")
        .expect("candidates should list");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].target_text, None);
    assert_eq!(candidates[0].source_count, 4);

    let second = store
        .upsert_glossary_candidates(
            "ivan",
            "English",
            "Italian",
            &[NewGlossaryCandidate {
                source_text: "Ivan Ilych",
                category: GlossaryCategory::Other,
                source_count: 7,
            }],
        )
        .expect("candidate updates");
    assert_eq!(second.updated, 1);
    assert_eq!(
        store
            .list_glossary_candidates("ivan", "English", "Italian")
            .expect("candidates should list")[0]
            .source_count,
        7
    );

    assert!(
        store
            .reject_glossary_candidate(candidates[0].id)
            .expect("candidate rejects")
    );
    let third = store
        .upsert_glossary_candidates(
            "ivan",
            "English",
            "Italian",
            &[NewGlossaryCandidate {
                source_text: "Ivan Ilych",
                category: GlossaryCategory::Other,
                source_count: 9,
            }],
        )
        .expect("rejected candidate is skipped");
    assert_eq!(third.skipped, 1);
    assert!(
        store
            .list_glossary_candidates("ivan", "English", "Italian")
            .expect("rejected candidates are not pending")
            .is_empty()
    );

    let all = store
        .list_glossary_terms(GlossaryFilter {
            scope_kind: Some(GlossaryScopeKind::Book),
            scope_id: Some("ivan"),
            source_language: Some("English"),
            target_language: Some("Italian"),
            active_only: false,
        })
        .expect("terms should list");
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].status, GlossaryStatus::Rejected);
    assert_eq!(all[0].source_count, 7);

    let seeded = glossary_term(
        bookforge_core::GlossaryScopeKind::Book,
        Some("ivan"),
        "Aragorn",
        "Aragorn",
    );
    let mut accepted = glossary_term(
        bookforge_core::GlossaryScopeKind::Book,
        Some("ivan"),
        "Mount Doom",
        "Monte Fato",
    );
    accepted.status = GlossaryStatus::Accepted;
    store
        .upsert_glossary_terms(&[seeded, accepted])
        .expect("active terms should insert");
    let fourth = store
        .upsert_glossary_candidates(
            "ivan",
            "English",
            "Italian",
            &[
                NewGlossaryCandidate {
                    source_text: "Aragorn",
                    category: GlossaryCategory::Other,
                    source_count: 12,
                },
                NewGlossaryCandidate {
                    source_text: "Mount Doom",
                    category: GlossaryCategory::Other,
                    source_count: 11,
                },
            ],
        )
        .expect("active terms are skipped");
    assert_eq!(fourth.skipped, 2);

    let _ = fs::remove_file(&db_path);
}

#[test]
fn glossary_terms_upsert_list_and_active_lookup() {
    let db_path = temp_path("glossary_terms.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let mut global = glossary_term(
        bookforge_core::GlossaryScopeKind::Global,
        None,
        "Aragorn",
        "Aragorn",
    );
    let book = glossary_term(
        bookforge_core::GlossaryScopeKind::Book,
        Some("fellowship"),
        "Aragorn",
        "Granpasso",
    );

    assert_eq!(
        store
            .upsert_glossary_terms(&[global.clone(), book.clone()])
            .expect("terms upsert"),
        2
    );
    global.target_text = "Aragorn II".to_string();
    store
        .upsert_glossary_terms(&[global.clone()])
        .expect("global term updates instead of duplicating");

    let all = store
        .list_glossary_terms(GlossaryFilter::default())
        .expect("terms list");
    assert_eq!(all.len(), 2);

    let active = store
        .load_active_glossary_terms("English", "Italian", Some("fellowship"), Some("lotr"))
        .expect("active terms");
    assert_eq!(active.len(), 2);
    assert!(active.iter().any(|term| term.target_text == "Granpasso"));
    let active_by_target = store
        .load_active_glossary_terms_for_target("Italian", Some("fellowship"), Some("lotr"))
        .expect("target-only active terms");
    assert_eq!(active_by_target.len(), 2);
    assert!(
        active_by_target
            .iter()
            .any(|term| term.source_language == "English" && term.target_text == "Granpasso")
    );

    let removed = store
        .clear_glossary_scope(bookforge_core::GlossaryScopeKind::Global, None)
        .expect("global clear");
    assert_eq!(removed, 1);

    let _ = fs::remove_file(&db_path);
}

#[test]
fn create_job_persists_book_and_series_ids() {
    let db_path = temp_path("glossary_jobids.sqlite");
    let input_path = temp_path("input_jobids.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture");
    let store = JobStore::open(&db_path).expect("store opens");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("out_jobids.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock",
            base_url: None,
            api_key_env: None,
            book_id: Some("fellowship"),
            series_id: Some("lord-of-the-rings"),
        })
        .expect("job created");
    assert_eq!(job.book_id.as_deref(), Some("fellowship"));
    assert_eq!(job.series_id.as_deref(), Some("lord-of-the-rings"));
    let loaded = store
        .get_job(&job.id)
        .expect("get_job ok")
        .expect("job present");
    assert_eq!(loaded.book_id.as_deref(), Some("fellowship"));
    assert_eq!(loaded.series_id.as_deref(), Some("lord-of-the-rings"));
    let _ = fs::remove_file(&db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn manual_correction_is_auditable_frozen_and_not_cacheable() {
    let db_path = temp_path("manual_correction.sqlite");
    let (store, job, segment) =
        build_seeded_store_with_translation(&db_path, "manual_ns", &["b_000000"]);
    store
        .mark_job_needs_review(&job.id)
        .expect("job should become reviewable");

    let manual_blocks = [BlockTranslation {
        block_id: BlockId("b_000000".to_string()),
        text: "Correzione umana".to_string(),
    }];
    store
        .save_manual_correction(SaveManualCorrection {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Correzione umana",
            blocks: &manual_blocks,
        })
        .expect("manual correction should save");

    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "MODEL OVERWRITE",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "MODEL OVERWRITE".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            tokens_estimated: false,
        })
        .expect("model write should be ignored rather than fail");

    let translations = store
        .load_terminal_segment_translations(&job.id)
        .expect("translation should load");
    assert_eq!(translations.len(), 1);
    assert_eq!(translations[0].translated_text, "Correzione umana");
    assert_eq!(translations[0].provider, "manual");
    assert_eq!(translations[0].model, "manual");
    assert!(translations[0].human_corrected);
    assert!(translations[0].corrected_at.is_some());
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "succeeded");

    let cached = store
        .find_cached_translation(
            &segment,
            "v1",
            "mock",
            "mock-prefix",
            Some("English"),
            "Italian",
            "manual_ns",
        )
        .expect("cache lookup should succeed");
    assert!(cached.is_none(), "manual corrections must remain job-local");

    let _ = fs::remove_file(db_path);
}

#[test]
fn manual_correction_rejects_active_jobs() {
    let db_path = temp_path("manual_correction_running.sqlite");
    let (store, job, _segment) =
        build_seeded_store_with_translation(&db_path, "manual_running_ns", &["b_000000"]);
    let result = store.save_manual_correction(SaveManualCorrection {
        job_id: &job.id,
        segment_id: "seg_a",
        translated_text: "Correzione",
        blocks: &[BlockTranslation {
            block_id: BlockId("b_000000".to_string()),
            text: "Correzione".to_string(),
        }],
    });
    assert!(matches!(result, Err(StoreError::InvalidCorrection(_))));

    let _ = fs::remove_file(db_path);
}

#[test]
fn dashboard_segment_flag_set_and_clear_is_job_scoped() {
    let db_path = temp_path("dashboard_flag.sqlite");
    let input_a = temp_path("flag_input_a.epub");
    let input_b = temp_path("flag_input_b.epub");
    fs::write(&input_a, b"epub bytes flag a").expect("input a fixture should be writable");
    fs::write(&input_b, b"epub bytes flag b").expect("input b fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job_a = store
        .create_job(CreateJob {
            input: &input_a,
            output: &temp_path("flag_output_a.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job a should be created");
    let job_b = store
        .create_job(CreateJob {
            input: &input_b,
            output: &temp_path("flag_output_b.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job b should be created");

    let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
    store
        .insert_segments(&job_a.id, &segments, "v1", "mock", "mock-prefix", "flag_ns")
        .expect("job a segments should insert");
    store
        .insert_segments(&job_b.id, &segments, "v1", "mock", "mock-prefix", "flag_ns")
        .expect("job b segments should insert");

    assert!(
        store
            .dashboard_flagged_segment_ids(&job_a.id)
            .unwrap()
            .is_empty()
    );

    store
        .set_dashboard_segment_flag(&job_a.id, "seg_a", true)
        .expect("flag should set");
    assert_eq!(
        store.dashboard_flagged_segment_ids(&job_a.id).unwrap(),
        vec!["seg_a".to_string()]
    );
    // Flags are job-scoped: the same segment id in a different job is unaffected.
    assert!(
        store
            .dashboard_flagged_segment_ids(&job_b.id)
            .unwrap()
            .is_empty()
    );

    // Clearing the flag removes it from the read path.
    store
        .set_dashboard_segment_flag(&job_a.id, "seg_a", false)
        .expect("flag should clear");
    assert!(
        store
            .dashboard_flagged_segment_ids(&job_a.id)
            .unwrap()
            .is_empty()
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_a);
    let _ = fs::remove_file(input_b);
}

#[test]
fn request_segment_retry_stores_guidance_and_transitions_state() {
    let db_path = temp_path("retry_guidance.sqlite");
    let (store, job, _segment) =
        build_seeded_store_with_translation(&db_path, "retry_ns", &["b_000000"]);

    // A retry request is only accepted once the job is out of "running"/
    // "paused" (see `request_segment_retry_rejects_running_and_paused_jobs`),
    // so move it to "needs_review" first, as a real dashboard-driven retry
    // would only be offered once the job has stopped actively translating.
    store
        .mark_job_needs_review(&job.id)
        .expect("job should become reviewable");
    assert_eq!(
        store.get_job(&job.id).unwrap().unwrap().status,
        "needs_review"
    );

    store
        .request_segment_retry(&job.id, "seg_a", Some("check the idiom in paragraph 2"))
        .expect("retry request should succeed");

    let guidance = store
        .load_retry_guidance(&job.id)
        .expect("guidance should load");
    assert_eq!(
        guidance.get("seg_a").map(String::as_str),
        Some("check the idiom in paragraph 2")
    );

    let records = store
        .segment_records(&job.id)
        .expect("segment records should load");
    let seg_a = records
        .iter()
        .find(|record| record.id == "seg_a")
        .expect("segment should be present");
    assert_eq!(seg_a.status, "retry_pending");
    assert!(seg_a.error.is_none());

    assert_eq!(
        store.get_job(&job.id).unwrap().unwrap().status,
        "retry_pending"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn request_segment_retry_rejects_human_corrected_segment() {
    let db_path = temp_path("retry_frozen.sqlite");
    let (store, job, _segment) =
        build_seeded_store_with_translation(&db_path, "retry_frozen_ns", &["b_000000"]);
    store
        .mark_job_needs_review(&job.id)
        .expect("job should become reviewable");
    store
        .save_manual_correction(SaveManualCorrection {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Correzione umana",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Correzione umana".to_string(),
            }],
        })
        .expect("manual correction should save");

    let result = store.request_segment_retry(&job.id, "seg_a", Some("try again"));
    assert!(matches!(result, Err(StoreError::InvalidCorrection(_))));

    // The rejected retry must not have recorded guidance or disturbed the frozen segment.
    let guidance = store
        .load_retry_guidance(&job.id)
        .expect("guidance should load");
    assert!(!guidance.contains_key("seg_a"));
    let records = store
        .segment_records(&job.id)
        .expect("segment records should load");
    let seg_a = records
        .iter()
        .find(|record| record.id == "seg_a")
        .expect("segment should be present");
    assert_eq!(seg_a.status, "succeeded");

    let _ = fs::remove_file(db_path);
}

#[test]
fn request_segment_retry_rejects_running_and_paused_jobs() {
    // Like save_manual_correction, request_segment_retry must reject retry
    // requests while the job is "running" or "paused": accepting one would
    // force-transition an in-flight job to "retry_pending" out from under
    // whatever is currently driving it. Neither guidance nor segment/job
    // state may change when the request is rejected.
    let db_path = temp_path("retry_no_job_guard.sqlite");
    let (store, job, _segment) =
        build_seeded_store_with_translation(&db_path, "retry_no_guard_ns", &["b_000000"]);

    store
        .mark_job_running(&job.id)
        .expect("job should be running");
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");
    let result = store.request_segment_retry(&job.id, "seg_a", Some("try again"));
    assert!(
        matches!(result, Err(StoreError::InvalidCorrection(_))),
        "retry must be rejected while the job is running, got: {result:?}"
    );
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "running");
    let guidance = store
        .load_retry_guidance(&job.id)
        .expect("guidance should load");
    assert!(
        !guidance.contains_key("seg_a"),
        "a rejected retry must not record guidance"
    );
    let records = store
        .segment_records(&job.id)
        .expect("segment records should load");
    let seg_a = records
        .iter()
        .find(|record| record.id == "seg_a")
        .expect("segment should be present");
    assert_eq!(
        seg_a.status, "succeeded",
        "a rejected retry must not move the segment to retry_pending"
    );

    store
        .mark_job_paused(&job.id)
        .expect("job should be paused");
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "paused");
    let result = store.request_segment_retry(&job.id, "seg_a", Some("try again"));
    assert!(
        matches!(result, Err(StoreError::InvalidCorrection(_))),
        "retry must be rejected while the job is paused, got: {result:?}"
    );
    assert_eq!(store.get_job(&job.id).unwrap().unwrap().status, "paused");
    let guidance = store
        .load_retry_guidance(&job.id)
        .expect("guidance should load");
    assert!(
        !guidance.contains_key("seg_a"),
        "a rejected retry must not record guidance"
    );
    let records = store
        .segment_records(&job.id)
        .expect("segment records should load");
    let seg_a = records
        .iter()
        .find(|record| record.id == "seg_a")
        .expect("segment should be present");
    assert_eq!(
        seg_a.status, "succeeded",
        "a rejected retry must not move the segment to retry_pending"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn terminal_save_translation_consumes_only_its_segment_guidance() {
    let db_path = temp_path("retry_consume.sqlite");
    let input_path = temp_path("consume_input.epub");
    fs::write(&input_path, b"epub bytes consume").expect("input fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("consume_output.epub"),
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
        .insert_segments(
            &job.id,
            &segments,
            "v1",
            "mock",
            "mock-prefix",
            "consume_ns",
        )
        .expect("segments should insert");
    // Jobs are created "running"; request_segment_retry now rejects retries
    // while running/paused, so move it to "needs_review" first.
    store
        .mark_job_needs_review(&job.id)
        .expect("job should become reviewable");

    store
        .request_segment_retry(&job.id, "seg_a", Some("guidance for a"))
        .expect("retry a should succeed");
    store
        .request_segment_retry(&job.id, "seg_b", Some("guidance for b"))
        .expect("retry b should succeed");

    let guidance = store.load_retry_guidance(&job.id).unwrap();
    assert_eq!(guidance.len(), 2);

    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "Tradotto A",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto A".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(1),
            input_cached_tokens: Some(0),
            output_tokens: Some(1),
            tokens_estimated: false,
        })
        .expect("translation should save");

    let guidance_after = store.load_retry_guidance(&job.id).unwrap();
    assert!(
        !guidance_after.contains_key("seg_a"),
        "the consumed segment's guidance should be gone"
    );
    assert_eq!(
        guidance_after.get("seg_b").map(String::as_str),
        Some("guidance for b"),
        "another segment's guidance should survive"
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn load_retry_guidance_is_job_scoped() {
    let db_path = temp_path("retry_job_scope.sqlite");
    let input_a = temp_path("scope_input_a.epub");
    let input_b = temp_path("scope_input_b.epub");
    fs::write(&input_a, b"epub bytes scope a").expect("input a fixture should be writable");
    fs::write(&input_b, b"epub bytes scope b").expect("input b fixture should be writable");

    let store = JobStore::open(&db_path).expect("store should open");
    let job_a = store
        .create_job(CreateJob {
            input: &input_a,
            output: &temp_path("scope_output_a.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job a should be created");
    let job_b = store
        .create_job(CreateJob {
            input: &input_b,
            output: &temp_path("scope_output_b.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job b should be created");

    let segments = vec![segment("seg_a", 0)];
    store
        .insert_segments(
            &job_a.id,
            &segments,
            "v1",
            "mock",
            "mock-prefix",
            "scope_ns",
        )
        .expect("job a segments should insert");
    store
        .insert_segments(
            &job_b.id,
            &segments,
            "v1",
            "mock",
            "mock-prefix",
            "scope_ns",
        )
        .expect("job b segments should insert");
    // Jobs are created "running"; request_segment_retry now rejects retries
    // while running/paused, so move job a to "needs_review" first.
    store
        .mark_job_needs_review(&job_a.id)
        .expect("job a should become reviewable");

    store
        .request_segment_retry(&job_a.id, "seg_a", Some("only for job a"))
        .expect("retry should succeed");

    let guidance_a = store.load_retry_guidance(&job_a.id).unwrap();
    assert_eq!(
        guidance_a.get("seg_a").map(String::as_str),
        Some("only for job a")
    );

    let guidance_b = store.load_retry_guidance(&job_b.id).unwrap();
    assert!(
        guidance_b.is_empty(),
        "job b should not see job a's retry guidance"
    );

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_a);
    let _ = fs::remove_file(input_b);
}

fn glossary_term(
    scope_kind: bookforge_core::GlossaryScopeKind,
    scope_id: Option<&str>,
    source: &str,
    target: &str,
) -> bookforge_core::GlossaryTerm {
    bookforge_core::GlossaryTerm {
        id: None,
        scope_kind,
        scope_id: scope_id.map(ToOwned::to_owned),
        source_text: source.to_string(),
        target_text: target.to_string(),
        category: bookforge_core::GlossaryCategory::Person,
        notes: None,
        case_sensitive: true,
        always_active: false,
        status: bookforge_core::GlossaryStatus::UserSeeded,
        source_language: "English".to_string(),
        target_language: "Italian".to_string(),
        source_count: 0,
    }
}
