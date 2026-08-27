use super::translations::MODEL_TRANSLATION_UPSERT;
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
    // A per-call counter guarantees a unique path even when two parallel tests
    // hit the same timestamp — the OS clock is coarse on some platforms
    // (notably Windows), so pid + nanos alone can collide and let one test's
    // cleanup race another's setup.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "bookforge-store-test-{}-{}-{}-{name}",
        std::process::id(),
        unix_timestamp_nanos(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
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
        creator: None,
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
        creator: None,
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
fn migrate_pre_v2_4_database_adds_correction_audit_fields_without_data_loss() {
    let db_path = temp_path("pre_v2_4_human_corrections.sqlite");
    {
        let conn = Connection::open(&db_path).expect("legacy db opens");
        for migration in [
            include_str!("../../migrations/0001_initial.sql"),
            include_str!("../../migrations/0002_v1_0_1_input_snapshot.sql"),
            include_str!("../../migrations/0003_v1_1_token_usage_and_flags.sql"),
            include_str!("../../migrations/0004_v1_2_glossary_terms.sql"),
            include_str!("../../migrations/0005_v1_2_1_nullable_glossary_candidate_targets.sql"),
            include_str!("../../migrations/0006_v1_3_context_styles_entities.sql"),
        ] {
            conn.execute_batch(migration)
                .expect("pre-v2.4 migration should apply");
        }
        conn.execute_batch(
            "
            INSERT INTO _migrations (version, name, applied_at) VALUES
              (1, 'initial', 'legacy'),
              (2, 'v1_0_1_input_snapshot', 'legacy'),
              (3, 'v1_1_segment_flags', 'legacy'),
              (4, 'v1_2_glossary_terms', 'legacy'),
              (5, 'v1_2_1_nullable_glossary_candidate_targets', 'legacy'),
              (6, 'v1_3_context_styles_entities', 'legacy');
            INSERT INTO jobs
              (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
            VALUES
              ('legacy_job', 'legacy_hash', 'Italian', 'mock', 'mock-prefix',
               'succeeded', 'created', 'updated');
            INSERT INTO segments
              (id, job_id, section_id, ordinal, source_hash, prompt_version,
               provider, model, status)
            VALUES
              ('legacy_segment', 'legacy_job', 'section_0', 0, 'source_hash', 'v2',
               'mock', 'mock-prefix', 'succeeded');
            INSERT INTO translations
              (segment_id, job_id, translated_text, provider, model, prompt_version, created_at)
            VALUES
              ('legacy_segment', 'legacy_job', 'Traduzione esistente',
               'mock', 'mock-prefix', 'v2', 'created');
            INSERT INTO translation_blocks
              (segment_id, job_id, block_id, translated_text)
            VALUES
              ('legacy_segment', 'legacy_job', 'b_000000', 'Traduzione esistente');
            ",
        )
        .expect("legacy data should initialize");
    }

    let store = JobStore::open(&db_path).expect("pre-v2.4 store opens and migrates");
    {
        let conn = store.conn.borrow();
        let row = conn
            .query_row(
                "SELECT translated_text, origin, human_corrected, corrected_at
                 FROM translations
                 WHERE job_id = 'legacy_job' AND segment_id = 'legacy_segment'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, Option<String>>(3)?,
                    ))
                },
            )
            .expect("legacy translation should survive migration");
        assert_eq!(row.0, "Traduzione esistente");
        assert_eq!(row.1, "model");
        assert!(!row.2);
        assert_eq!(row.3, None);

        let block_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM translation_blocks
                 WHERE job_id = 'legacy_job' AND segment_id = 'legacy_segment'",
                [],
                |row| row.get(0),
            )
            .expect("legacy blocks should survive migration");
        assert_eq!(block_count, 1);

        let version: i64 = conn
            .query_row(
                "SELECT version FROM _migrations WHERE name = 'v2_4_human_corrections'",
                [],
                |row| row.get(0),
            )
            .expect("v2.4 migration should be recorded");
        assert_eq!(version, 7);
    }
    drop(store);

    let reopened = JobStore::open(&db_path).expect("v2.4 migration should be idempotent");
    assert!(
        !reopened
            .translation_is_human_corrected("legacy_job", "legacy_segment")
            .expect("legacy correction state should load")
    );
    drop(reopened);
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

const MULTI_FINDING_ERROR: &str = "translation is unchanged from the source-language prose; batch translation block mismatch: missing=[\"b_000853\"], extra=[], duplicate=[]";

fn setup_findings_store(
    name: &str,
    segment_count: usize,
) -> (PathBuf, PathBuf, JobStore, JobRecord) {
    let db_path = temp_path(name);
    let input_path = temp_path("findings_input.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture should be writable");
    let store = JobStore::open(&db_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path("findings_output.epub"),
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
    let segments = (0..segment_count)
        .map(|ordinal| segment(&format!("seg_{ordinal}"), ordinal))
        .collect::<Vec<_>>();
    store
        .insert_segments(
            &job.id,
            &segments,
            "v1",
            "mock",
            "mock-prefix",
            "findings_ns",
        )
        .expect("segments should insert");
    (db_path, input_path, store, job)
}

fn save_two_findings(store: &JobStore, job_id: &str) {
    store
        .save_needs_review(SaveNeedsReview {
            job_id,
            segment_id: "seg_0",
            preserved_text: "Source 0",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Source 0".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            error: MULTI_FINDING_ERROR,
            input_tokens: Some(5),
            input_cached_tokens: Some(0),
            output_tokens: Some(5),
            tokens_estimated: false,
        })
        .expect("review translation should save");
}

#[test]
fn classifies_real_qa_failure_strings_into_every_taxonomy_kind() {
    let cases = [
        (
            "protected span missing from segment 'seg_0007': 4th",
            QaFindingKind::ProtectedSpanMissing,
            QaFindingSeverity::Error,
            "protected_span_missing",
        ),
        (
            "inline marker missing from segment 'seg_0012': m1",
            QaFindingKind::InlineMarkerMissing,
            QaFindingSeverity::Error,
            "inline_marker_missing",
        ),
        (
            "inline marker duplicated: m3",
            QaFindingKind::InlineMarkerDuplicated,
            QaFindingSeverity::Error,
            "inline_marker_duplicated",
        ),
        (
            "unknown inline marker: m9",
            QaFindingKind::InlineMarkerUnknown,
            QaFindingSeverity::Error,
            "inline_marker_unknown",
        ),
        (
            "inline marker <i> is missing closing tag </i>",
            QaFindingKind::MarkerStructure,
            QaFindingSeverity::Error,
            "marker_structure",
        ),
        (
            "segment 'seg_0003' expected 4 block translations, got 3",
            QaFindingKind::BatchBlockMismatch,
            QaFindingSeverity::Error,
            "batch_block_mismatch",
        ),
        (
            "translation is unchanged from the source-language prose",
            QaFindingKind::SourceCopyUnchanged,
            QaFindingSeverity::Warning,
            "source_copy_unchanged",
        ),
        (
            "unapproved lowercase word in strict Toki Pona output: kalama",
            QaFindingKind::TargetLanguageGate,
            QaFindingSeverity::Warning,
            "target_language_gate",
        ),
        (
            "translation checkpoint failure: provider error: HTTP status 503: upstream unavailable",
            QaFindingKind::ProviderError,
            QaFindingSeverity::Error,
            "provider_error",
        ),
        (
            "batch translation checkpoint failure: provider error: interrupted by user",
            QaFindingKind::Interrupted,
            QaFindingSeverity::Warning,
            "interrupted",
        ),
        (
            "something nobody has seen before",
            QaFindingKind::Other,
            QaFindingSeverity::Error,
            "other",
        ),
    ];

    for (error, expected_kind, expected_severity, expected_wire_kind) in cases {
        let findings = classify_segment_error(error);
        assert_eq!(findings.len(), 1, "{error}");
        assert_eq!(findings[0].kind, expected_kind, "{error}");
        assert_eq!(findings[0].severity, expected_severity, "{error}");
        assert_eq!(findings[0].kind.as_str(), expected_wire_kind, "{error}");
        assert_eq!(
            findings[0].severity.as_str(),
            expected_severity.as_str(),
            "{error}"
        );
        assert_eq!(findings[0].message, error, "{error}");
    }
}

#[test]
fn classifies_concatenated_failures_as_distinct_findings() {
    let findings = classify_segment_error(MULTI_FINDING_ERROR);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].kind, QaFindingKind::SourceCopyUnchanged);
    assert_eq!(
        findings[0].message,
        "translation is unchanged from the source-language prose"
    );
    assert_eq!(findings[1].kind, QaFindingKind::BatchBlockMismatch);
    assert_eq!(
        findings[1].message,
        "batch translation block mismatch: missing=[\"b_000853\"], extra=[], duplicate=[]"
    );

    let breakdown = aggregate_findings(findings);
    assert_eq!(breakdown.len(), 2);
    assert_eq!(breakdown[0].share_percent(2), 50.0);
    assert_eq!(breakdown[0].share_percent(0), 0.0);
}

#[test]
fn embedded_separator_in_validator_context_stays_in_one_finding() {
    let error = "pi must group at least two following words; offending context: jan pi mute";
    let findings = classify_segment_error(error);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].kind, QaFindingKind::TargetLanguageGate);
    assert_eq!(findings[0].message, error);
}

#[test]
fn empty_segment_errors_produce_no_findings() {
    assert!(classify_segment_error("").is_empty());
    assert!(classify_segment_error(" \t\r\n ").is_empty());
}

#[test]
fn save_needs_review_records_each_distinct_failure() {
    let (db_path, input_path, store, job) =
        setup_findings_store("save_needs_review_findings.sqlite", 1);
    save_two_findings(&store, &job.id);

    let findings = store
        .segment_qa_findings(&job.id)
        .expect("findings should load");
    assert_eq!(findings.len(), 2);
    assert!(findings.iter().all(|finding| finding.segment_id == "seg_0"));
    let breakdown = store
        .qa_finding_breakdown(&job.id)
        .expect("breakdown should load");
    assert_eq!(breakdown.len(), 2);
    assert_eq!(breakdown[0].kind, "batch_block_mismatch");
    assert_eq!(breakdown[1].kind, "source_copy_unchanged");

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn repeated_needs_review_save_replaces_findings_without_duplication() {
    let (db_path, input_path, store, job) =
        setup_findings_store("replace_needs_review_findings.sqlite", 1);
    save_two_findings(&store, &job.id);
    let first_ids = store
        .segment_qa_findings(&job.id)
        .expect("first findings should load")
        .into_iter()
        .map(|finding| finding.id)
        .collect::<Vec<_>>();

    save_two_findings(&store, &job.id);
    let second_ids = store
        .segment_qa_findings(&job.id)
        .expect("replacement findings should load")
        .into_iter()
        .map(|finding| finding.id)
        .collect::<Vec<_>>();
    assert_eq!(second_ids.len(), 2);
    assert_eq!(second_ids, first_ids);

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn successful_translation_clears_previous_findings() {
    let (db_path, input_path, store, job) =
        setup_findings_store("clear_translation_findings.sqlite", 1);
    save_two_findings(&store, &job.id);
    assert_eq!(store.segment_qa_findings(&job.id).unwrap().len(), 2);

    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_0",
            translated_text: "Tradotto",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000000".to_string()),
                text: "Tradotto".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(4),
            input_cached_tokens: Some(0),
            output_tokens: Some(3),
            tokens_estimated: false,
        })
        .expect("successful translation should save");
    assert!(store.segment_qa_findings(&job.id).unwrap().is_empty());

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn migration_eight_backfills_existing_segment_errors_once() {
    let (db_path, input_path, store, job) = setup_findings_store("backfill_qa_findings.sqlite", 1);
    {
        let conn = store.conn.borrow();
        conn.execute(
            "UPDATE segments
             SET status = 'needs_review', error = ?1
             WHERE job_id = ?2 AND id = 'seg_0'",
            params![MULTI_FINDING_ERROR, job.id],
        )
        .expect("legacy segment should update");
        conn.execute("DELETE FROM qa_findings", [])
            .expect("findings should clear");
        conn.execute("DELETE FROM _migrations WHERE version = 8", [])
            .expect("migration marker should clear");
    }
    drop(store);

    let reopened = JobStore::open(&db_path).expect("legacy store should reopen");
    let findings = reopened
        .segment_qa_findings(&job.id)
        .expect("backfilled findings should load");
    assert_eq!(findings.len(), 2);
    assert!(
        findings
            .iter()
            .any(|finding| finding.kind == "source_copy_unchanged")
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.kind == "batch_block_mismatch")
    );
    let migration_count = reopened
        .conn
        .borrow()
        .query_row(
            "SELECT COUNT(*) FROM _migrations WHERE version = 8",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("migration marker should query");
    assert_eq!(migration_count, 1);

    drop(reopened);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn qa_finding_breakdown_orders_counts_descending() {
    let (db_path, input_path, store, job) =
        setup_findings_store("qa_finding_breakdown_order.sqlite", 6);
    let errors = [
        "batch translation block mismatch: missing=[], extra=[], duplicate=[]",
        "missing block translations: b_1",
        "segment 'seg_2' expected 2 block translations, got 1",
        "provider error: upstream unavailable",
        "HTTP status 503: upstream unavailable",
        "translation is unchanged from the source-language prose",
    ];
    for (ordinal, error) in errors.into_iter().enumerate() {
        assert_eq!(
            store
                .record_segment_findings(&job.id, &format!("seg_{ordinal}"), error)
                .expect("finding should record"),
            1
        );
    }

    let breakdown = store
        .qa_finding_breakdown(&job.id)
        .expect("breakdown should load");
    assert_eq!(
        breakdown
            .iter()
            .map(|entry| (entry.kind.as_str(), entry.count))
            .collect::<Vec<_>>(),
        vec![
            ("batch_block_mismatch", 3),
            ("provider_error", 2),
            ("source_copy_unchanged", 1),
        ]
    );

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn model_write_paths_do_not_clobber_preexisting_human_correction() {
    // Regression for H-1/STORE-1 (check-then-write TOCTOU). The freeze check
    // and the write used to be separate autocommit steps, so a dashboard
    // process committing a manual correction between them was clobbered by
    // `INSERT OR REPLACE`. Here the frozen row already exists when each
    // model-write checkpoint runs — exactly the interleaving a losing race
    // produced — and every path must yield without disturbing it.
    let db_path = temp_path("toctou_frozen.sqlite");
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
    let segments = vec![segment("seg_a", 0)];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "toctou_ns")
        .expect("segments should insert");

    // The other process lands a human correction first.
    {
        let conn = store.conn.borrow();
        conn.execute(
            "INSERT INTO translations
             (segment_id, job_id, translated_text, provider, model, prompt_version,
              created_at, origin, human_corrected, corrected_at)
             VALUES ('seg_a', ?1, 'Correzione umana', 'manual', 'manual', 'v1',
                     '1000', 'manual', 1, '1000')",
            params![job.id],
        )
        .expect("frozen translation should insert");
        conn.execute(
            "INSERT INTO translation_blocks (segment_id, job_id, block_id, translated_text)
             VALUES ('seg_a', ?1, 'b_000000', 'Correzione umana')",
            params![job.id],
        )
        .expect("frozen block should insert");
        conn.execute(
            "UPDATE segments SET status = 'succeeded', attempts = 1
             WHERE job_id = ?1 AND id = 'seg_a'",
            params![job.id],
        )
        .expect("frozen segment state should update");
    }

    let model_blocks = [BlockTranslation {
        block_id: BlockId("b_000000".to_string()),
        text: "MODEL OVERWRITE".to_string(),
    }];
    store
        .save_translation(SaveTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "MODEL OVERWRITE",
            blocks: &model_blocks,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            input_tokens: Some(9),
            input_cached_tokens: Some(0),
            output_tokens: Some(9),
            tokens_estimated: false,
        })
        .expect("model save should be ignored rather than fail");
    store
        .save_needs_review(SaveNeedsReview {
            job_id: &job.id,
            segment_id: "seg_a",
            preserved_text: "MODEL REVIEW OVERWRITE",
            blocks: &model_blocks,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            error: "qa issue",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("needs-review save should be ignored rather than fail");
    store
        .save_cached_translation(SaveCachedTranslation {
            job_id: &job.id,
            segment_id: "seg_a",
            translated_text: "MODEL CACHE OVERWRITE",
            blocks: &model_blocks,
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
        })
        .expect("cached save should be ignored rather than fail");

    let conn = store.conn.borrow();
    let row = conn
        .query_row(
            "SELECT translated_text, provider, model, origin, human_corrected, corrected_at
             FROM translations WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![job.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .expect("frozen translation should survive all three model-write paths");
    assert_eq!(row.0, "Correzione umana");
    assert_eq!(row.1, "manual");
    assert_eq!(row.2, "manual");
    assert_eq!(row.3, "manual");
    assert_eq!(row.4, 1);
    assert_eq!(row.5.as_deref(), Some("1000"));

    let block_text: String = conn
        .query_row(
            "SELECT translated_text FROM translation_blocks
             WHERE job_id = ?1 AND segment_id = 'seg_a' AND block_id = 'b_000000'",
            params![job.id],
            |row| row.get(0),
        )
        .expect("corrected block should survive");
    assert_eq!(block_text, "Correzione umana");

    let segment_state = conn
        .query_row(
            "SELECT status, attempts FROM segments WHERE job_id = ?1 AND id = 'seg_a'",
            params![job.id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("segment should load");
    assert_eq!(segment_state.0, "succeeded");
    assert_eq!(
        segment_state.1, 1,
        "attempts must not advance on a frozen segment"
    );
    drop(conn);

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn guarded_model_upsert_leaves_frozen_rows_untouched_at_sql_level() {
    // Even if a future caller bypasses the application-level freeze check,
    // the single-statement upsert itself must refuse to overwrite a row with
    // `human_corrected = 1` (and must not delete-and-reinsert it the way
    // `INSERT OR REPLACE` did).
    let db_path = temp_path("guarded_upsert.sqlite");
    let (store, job, _seg) =
        build_seeded_store_with_translation(&db_path, "guard_ns", &["b_000000"]);
    {
        let conn = store.conn.borrow();
        conn.execute(
            "UPDATE translations SET human_corrected = 1, origin = 'manual',
                    corrected_at = '1000'
             WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![job.id],
        )
        .expect("freeze flag should update");
    }

    let overwrite_before = {
        let conn = store.conn.borrow();
        let before: (String, String, i64) = conn
            .query_row(
                "SELECT translated_text, created_at, human_corrected FROM translations
                 WHERE job_id = ?1 AND segment_id = 'seg_a'",
                params![job.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("frozen row should load");
        assert_eq!(before.0, "Tradotto");
        assert_eq!(before.2, 1);
        let changed = conn
            .execute(
                MODEL_TRANSLATION_UPSERT,
                params![
                    "seg_a",
                    job.id,
                    "SQL LEVEL OVERWRITE",
                    "mock",
                    "mock-prefix",
                    "v1",
                    "2000"
                ],
            )
            .expect("guarded upsert should execute");
        (changed, before.1)
    };
    assert_eq!(overwrite_before.0, 0, "a frozen row must not be updated");

    let conn = store.conn.borrow();
    let frozen = conn
        .query_row(
            "SELECT translated_text, created_at, origin, human_corrected FROM translations
             WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![job.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .expect("frozen row should load");
    assert_eq!(frozen.0, "Tradotto", "frozen text must not change");
    assert_eq!(frozen.1, overwrite_before.1, "created_at must not change");
    assert_eq!(frozen.2, "manual", "origin must survive the guarded upsert");
    assert_eq!(frozen.3, 1);
    drop(conn);

    // Unfreezing lets the same statement through.
    {
        let conn = store.conn.borrow();
        conn.execute(
            "UPDATE translations SET human_corrected = 0 WHERE job_id = ?1",
            params![job.id],
        )
        .expect("unfreeze should work");
    }
    let overwrite = {
        let conn = store.conn.borrow();
        conn.execute(
            MODEL_TRANSLATION_UPSERT,
            params![
                "seg_a",
                job.id,
                "SQL LEVEL OVERWRITE",
                "mock",
                "mock-prefix",
                "v1",
                "2000"
            ],
        )
        .expect("guarded upsert should execute")
    };
    assert_eq!(overwrite, 1, "an unfrozen row must be updated");
    let text: String = {
        let conn = store.conn.borrow();
        conn.query_row(
            "SELECT translated_text FROM translations WHERE job_id = ?1 AND segment_id = 'seg_a'",
            params![job.id],
            |row| row.get(0),
        )
        .expect("row should load")
    };
    assert_eq!(text, "SQL LEVEL OVERWRITE");

    let _ = fs::remove_file(db_path);
}

#[test]
fn resume_reinsert_refreshes_segment_cache_identity_columns() {
    // STORE-11: resume re-runs insert_segments against rows that already
    // exist. `INSERT OR IGNORE` left stale provider/model/prompt_version/
    // source_hash/cache_namespace values behind after a config change, so
    // later cache lookups misattributed hits.
    let db_path = temp_path("resume_identity.sqlite");
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
            model: "old-model",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");

    let mut seg = segment("seg_a", 0);
    seg.checksum = "checksum_old".to_string();
    store
        .insert_segments(
            &job.id,
            std::slice::from_ref(&seg),
            "v1",
            "mock",
            "old-model",
            "ns_old",
        )
        .expect("initial insert should work");
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
            model: "old-model",
            prompt_version: "v1",
            input_tokens: Some(3),
            input_cached_tokens: Some(0),
            output_tokens: Some(3),
            tokens_estimated: false,
        })
        .expect("translation should save");

    // Resume after a config change: new provider/model/prompt/source hash.
    seg.checksum = "checksum_new".to_string();
    store
        .insert_segments(
            &job.id,
            std::slice::from_ref(&seg),
            "v2",
            "openrouter",
            "new-model",
            "ns_new",
        )
        .expect("resume re-insert should work");

    let conn = store.conn.borrow();
    let row = conn
        .query_row(
            "SELECT provider, model, prompt_version, source_hash, cache_namespace,
                    status, attempts
             FROM segments WHERE job_id = ?1 AND id = 'seg_a'",
            params![job.id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )
        .expect("segment row should load");
    drop(conn);

    assert_eq!(row.0, "openrouter");
    assert_eq!(row.1, "new-model");
    assert_eq!(row.2, "v2");
    assert_eq!(row.3, "checksum_new");
    assert_eq!(row.4, "ns_new");
    assert_eq!(row.5, "succeeded", "resume re-insert must not reset status");
    assert_eq!(row.6, 1, "resume re-insert must not reset attempts");

    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn global_scope_unique_indexes_reject_duplicate_globals_across_connections() {
    // STORE-13: NULL scope_id made every global row distinct to the table
    // UNIQUE constraints, so concurrent first-inserts on two connections
    // duplicated global terms/styles/entities. The partial unique indexes
    // close that hole; scoped rows keep working via the table constraints.
    let db_path = temp_path("global_unique.sqlite");
    let store_a = JobStore::open(&db_path).expect("store a opens");
    let store_b = JobStore::open(&db_path).expect("store b opens (second connection)");

    store_a
        .upsert_glossary_terms(&[glossary_term(
            bookforge_core::GlossaryScopeKind::Global,
            None,
            "Aragorn",
            "Aragorn",
        )])
        .expect("global term inserts");

    // A second writer inserting the same global identity must now fail.
    let duplicate = {
        let conn = store_b.conn.borrow();
        conn.execute(
            "INSERT INTO glossary_terms
             (scope_kind, scope_id, source_text, target_text, category, notes,
              case_sensitive, always_active, status, source_language, target_language,
              source_count, created_at, updated_at)
             VALUES ('global', NULL, 'Aragorn', 'other target', 'person', NULL,
                     1, 0, 'user_seeded', 'English', 'Italian', 0, 't1', 't1')",
            [],
        )
    };
    assert!(
        duplicate.is_err(),
        "duplicate global glossary term must violate the partial unique index"
    );

    // Scoped rows are unaffected by the partial index.
    let scoped = {
        let conn = store_b.conn.borrow();
        conn.execute(
            "INSERT INTO glossary_terms
             (scope_kind, scope_id, source_text, target_text, category, notes,
              case_sensitive, always_active, status, source_language, target_language,
              source_count, created_at, updated_at)
             VALUES ('book', 'lotr', 'Aragorn', 'Granpasso', 'person', NULL,
                     1, 0, 'user_seeded', 'English', 'Italian', 0, 't1', 't1')",
            [],
        )
    };
    assert!(scoped.is_ok(), "scoped rows must remain insertable");

    // Same protection for global style sheets and entities.
    let style_first = {
        let conn = store_a.conn.borrow();
        conn.execute(
            "INSERT INTO style_sheets
             (scope_kind, scope_id, target_language, content_toml, fingerprint,
              created_at, updated_at)
             VALUES ('global', NULL, 'Italian', 'toml-a', 'fp-a', 't1', 't1')",
            [],
        )
    };
    assert!(style_first.is_ok());
    let style_dup = {
        let conn = store_b.conn.borrow();
        conn.execute(
            "INSERT INTO style_sheets
             (scope_kind, scope_id, target_language, content_toml, fingerprint,
              created_at, updated_at)
             VALUES ('global', NULL, 'Italian', 'toml-b', 'fp-b', 't2', 't2')",
            [],
        )
    };
    assert!(
        style_dup.is_err(),
        "duplicate global style sheet must violate the partial unique index"
    );

    let entity_first = {
        let conn = store_a.conn.borrow();
        conn.execute(
            "INSERT INTO entities
             (scope_kind, scope_id, source_name, target_name, gender_target,
              role, notes, source_language, target_language, created_at, updated_at)
             VALUES ('global', NULL, 'Ivan', 'Ivan', NULL, NULL, NULL,
                     'English', 'Italian', 't1', 't1')",
            [],
        )
    };
    assert!(entity_first.is_ok());
    let entity_dup = {
        let conn = store_b.conn.borrow();
        conn.execute(
            "INSERT INTO entities
             (scope_kind, scope_id, source_name, target_name, gender_target,
              role, notes, source_language, target_language, created_at, updated_at)
             VALUES ('global', NULL, 'Ivan', 'Ivan II', NULL, NULL, NULL,
                     'English', 'Italian', 't2', 't2')",
            [],
        )
    };
    assert!(
        entity_dup.is_err(),
        "duplicate global entity must violate the partial unique index"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn migration_nine_deduplicates_legacy_global_rows_and_recreates_indexes() {
    let db_path = temp_path("migration_nine_dedupe.sqlite");
    {
        let store = JobStore::open(&db_path).expect("store opens");
        {
            let conn = store.conn.borrow();
            conn.execute_batch(
                "DROP INDEX IF EXISTS ux_glossary_terms_global_identity;
                 DELETE FROM _migrations WHERE version = 9;
                 INSERT INTO glossary_terms
                   (scope_kind, scope_id, source_text, target_text, category, notes,
                    case_sensitive, always_active, status, source_language, target_language,
                    source_count, created_at, updated_at)
                 VALUES
                   ('global', NULL, 'Ivan', 'vecchio', 'person', NULL,
                    1, 0, 'user_seeded', 'English', 'Italian', 0, '10', '10'),
                   ('global', NULL, 'Ivan', 'recente', 'person', NULL,
                    1, 0, 'user_seeded', 'English', 'Italian', 0, '20', '20');",
            )
            .expect("pre-migration legacy state should initialize");
        }
    }

    let reopened = JobStore::open(&db_path).expect("reopen runs migration 9");
    let survivors = reopened
        .list_glossary_terms(GlossaryFilter {
            scope_kind: Some(GlossaryScopeKind::Global),
            scope_id: None,
            source_language: Some("English"),
            target_language: Some("Italian"),
            active_only: false,
        })
        .expect("terms should list");
    assert_eq!(survivors.len(), 1, "duplicates must collapse to one row");
    assert_eq!(
        survivors[0].target_text, "recente",
        "the most recently updated duplicate must win"
    );

    {
        let conn = reopened.conn.borrow();
        let index: String = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'index'
                 AND name = 'ux_glossary_terms_global_identity'",
                [],
                |row| row.get(0),
            )
            .expect("unique index should be recreated");
        assert_eq!(index, "ux_glossary_terms_global_identity");
        let applied: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _migrations WHERE version = 9",
                [],
                |row| row.get(0),
            )
            .expect("migration marker should query");
        assert_eq!(applied, 1);
    }

    let _ = fs::remove_file(db_path);
}

#[test]
fn migrate_creates_jobs_created_at_index() {
    // STORE-16: the dashboard/watch job lists sort by created_at on every
    // refresh; without this index each refresh sorted the whole table.
    let db_path = temp_path("jobs_created_at_index.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let conn = store.conn.borrow();
    let index: String = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'index'
             AND name = 'idx_jobs_created_at'",
            [],
            |row| row.get(0),
        )
        .expect("idx_jobs_created_at exists");
    assert_eq!(index, "idx_jobs_created_at");
    drop(conn);
    let _ = fs::remove_file(db_path);
}

#[test]
fn add_glossary_term_returns_stable_row_id_and_updates_in_place() {
    // STORE-15: the id used to come from a re-select after a separate
    // transaction, so a concurrent writer could make the returned id point at
    // a different row. The upsert now returns its own id atomically.
    let db_path = temp_path("add_glossary_term_id.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let mut term = glossary_term(
        bookforge_core::GlossaryScopeKind::Global,
        None,
        "Aragorn",
        "Aragorn",
    );
    let first = store.add_glossary_term(&term).expect("first insert");
    term.target_text = "Granpasso".to_string();
    let second = store.add_glossary_term(&term).expect("update in place");
    assert_eq!(first, second, "upsert must return the existing row's id");

    let rows = store
        .list_glossary_terms(GlossaryFilter::default())
        .expect("terms list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, Some(second));
    assert_eq!(rows[0].target_text, "Granpasso");

    let _ = fs::remove_file(db_path);
}

#[test]
fn not_found_and_policy_rejections_use_distinct_error_variants() {
    // STORE-18: InvalidCorrection doubled as a generic rejection, so callers
    // could not distinguish "this does not exist" from "policy says no".
    let db_path = temp_path("not_found_vs_policy.sqlite");
    let (store, job, _segment) =
        build_seeded_store_with_translation(&db_path, "variant_ns", &["b_000000"]);

    // Unknown segment on an inactive job -> NotFound.
    store
        .mark_job_needs_review(&job.id)
        .expect("job should become reviewable");
    let unknown_segment = store.save_manual_correction(SaveManualCorrection {
        job_id: &job.id,
        segment_id: "missing_segment",
        translated_text: "Correzione",
        blocks: &[BlockTranslation {
            block_id: BlockId("b_000000".to_string()),
            text: "Correzione".to_string(),
        }],
    });
    assert!(
        matches!(unknown_segment, Err(StoreError::NotFound(_))),
        "unknown segment must be NotFound, got: {unknown_segment:?}"
    );
    let unknown_retry = store.request_segment_retry(&job.id, "missing_segment", None);
    assert!(
        matches!(unknown_retry, Err(StoreError::NotFound(_))),
        "unknown retry target must be NotFound, got: {unknown_retry:?}"
    );
    let unknown_flag = store.set_dashboard_segment_flag(&job.id, "missing_segment", true);
    assert!(
        matches!(unknown_flag, Err(StoreError::NotFound(_))),
        "unknown flag target must be NotFound, got: {unknown_flag:?}"
    );
    let unknown_job = store.save_manual_correction(SaveManualCorrection {
        job_id: "missing_job",
        segment_id: "seg_a",
        translated_text: "Correzione",
        blocks: &[BlockTranslation {
            block_id: BlockId("b_000000".to_string()),
            text: "Correzione".to_string(),
        }],
    });
    assert!(
        matches!(unknown_job, Err(StoreError::NotFound(_))),
        "unknown job must be NotFound, got: {unknown_job:?}"
    );

    // Policy rejection on an active job stays InvalidCorrection.
    store
        .mark_job_running_for_resume(&job.id)
        .expect("job should run again");
    let policy = store.save_manual_correction(SaveManualCorrection {
        job_id: &job.id,
        segment_id: "seg_a",
        translated_text: "Correzione",
        blocks: &[BlockTranslation {
            block_id: BlockId("b_000000".to_string()),
            text: "Correzione".to_string(),
        }],
    });
    assert!(
        matches!(policy, Err(StoreError::InvalidCorrection(_))),
        "running-job rejection is policy, got: {policy:?}"
    );

    let _ = fs::remove_file(db_path);
}

#[test]
fn migrate_legacy_rename_cascade_completes_and_preserves_data() {
    // STORE-4: the pre-v1_0_1 rename cascade used to run as five separate
    // autocommit statements; a crash mid-cascade orphaned data and the next
    // open silently recreated empty tables. The cascade now commits once, so
    // an open either leaves zero orphans plus fresh tables, or nothing at all.
    // The fixture hand-builds the true pre-v1_0_1 shape (translations keyed
    // by segment_id alone, no job_id column — the trigger the migrate pass
    // detects).
    let db_path = temp_path("legacy_rename_cascade.sqlite");
    {
        let conn = Connection::open(&db_path).expect("legacy db opens");
        conn.execute_batch(
            "
            CREATE TABLE _migrations (
              version INTEGER PRIMARY KEY,
              name TEXT NOT NULL,
              applied_at TEXT NOT NULL
            );
            CREATE TABLE jobs (
              id TEXT PRIMARY KEY,
              input_hash TEXT NOT NULL,
              target_lang TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              status TEXT NOT NULL,
              created_at TEXT NOT NULL,
              updated_at TEXT NOT NULL
            );
            CREATE TABLE segments (
              id TEXT NOT NULL,
              job_id TEXT NOT NULL,
              section_id TEXT NOT NULL,
              ordinal INTEGER NOT NULL,
              source_hash TEXT NOT NULL,
              prompt_version TEXT NOT NULL,
              provider TEXT NOT NULL,
              model TEXT NOT NULL,
              status TEXT NOT NULL,
              attempts INTEGER NOT NULL DEFAULT 0,
              error TEXT,
              PRIMARY KEY (job_id, id)
            );
            CREATE TABLE translations (
              segment_id TEXT NOT NULL PRIMARY KEY,
              translated_text TEXT NOT NULL,
              created_at TEXT NOT NULL
            );
            CREATE TABLE translation_blocks (
              segment_id TEXT NOT NULL,
              block_id TEXT NOT NULL,
              translated_text TEXT NOT NULL,
              PRIMARY KEY (segment_id, block_id)
            );
            CREATE TABLE qa_findings (
              id TEXT PRIMARY KEY,
              segment_id TEXT NOT NULL
            );
            INSERT INTO _migrations VALUES (1, 'initial', 'legacy');
            INSERT INTO jobs
              (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
            VALUES
              ('legacy_job', 'legacy_hash', 'Italian', 'mock', 'mock-prefix',
               'succeeded', 'created', 'updated');
            INSERT INTO segments
              (id, job_id, section_id, ordinal, source_hash, prompt_version,
               provider, model, status)
            VALUES
              ('legacy_segment', 'legacy_job', 'section_0', 0, 'hash', 'v1',
               'mock', 'mock-prefix', 'succeeded');
            INSERT INTO translations (segment_id, translated_text, created_at)
            VALUES ('legacy_segment', 'Traduzione', 'c');
            ",
        )
        .expect("legacy data should initialize");
    }

    let store = JobStore::open(&db_path).expect("store opens and migrates");
    let conn = store.conn.borrow();
    let legacy_tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE '%_legacy_%'",
            )
            .expect("legacy scan prepares");
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("legacy scan queries");
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .expect("legacy scan collects")
    };
    assert_eq!(
        legacy_tables.len(),
        5,
        "exactly the five renamed tables may remain: {legacy_tables:?}"
    );

    let preserved: i64 = legacy_tables
        .iter()
        .filter(|table| table.starts_with("translations_legacy_"))
        .map(|table| {
            conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("legacy row count")
        })
        .sum();
    assert_eq!(
        preserved, 1,
        "legacy translation data must survive the cascade"
    );

    let fresh_jobs: i64 = conn
        .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get(0))
        .expect("fresh jobs count");
    assert_eq!(
        fresh_jobs, 0,
        "fresh tables start empty; data lives in the copies"
    );
    drop(conn);

    let _ = fs::remove_file(db_path);
}

// ---------------------------------------------------------------------------
// STORE-12: typed statuses + storage-level CHECK enforcement
// ---------------------------------------------------------------------------

#[test]
fn status_check_constraints_reject_non_canonical_writes() {
    let db_path = temp_path("status_checks.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    assert!(
        store.take_diagnostics().is_empty(),
        "fresh stores carry no diagnostics"
    );

    {
        let conn = store.conn.borrow();
        for text in JobStatus::KNOWN_DB_TEXTS {
            let result = conn.execute(
                "INSERT INTO jobs
                 (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
                 VALUES (?1, 'h', 'Italian', 'mock', 'mock', ?2, 't', 't')",
                params![format!("job_ok_{text}"), text],
            );
            assert!(result.is_ok(), "canonical job status '{text}' must insert");
        }
        for text in SegmentStatus::KNOWN_DB_TEXTS {
            let result = conn.execute(
                "INSERT INTO segments
                 (id, job_id, section_id, ordinal, source_hash, prompt_version,
                  provider, model, status)
                 VALUES (?1, 'job_ok_running', 'sec', 0, 'sh', 'v', 'mock', 'mock', ?2)",
                params![format!("seg_ok_{text}"), text],
            );
            assert!(
                result.is_ok(),
                "canonical segment status '{text}' must insert"
            );
        }

        // CHECK constraints make every foreign vocabulary write fail at the
        // SQL boundary instead of silently poisoning downstream matches.
        let bad_job = conn.execute(
            "INSERT INTO jobs
             (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
             VALUES ('job_bad', 'h', 'Italian', 'mock', 'mock', 'mysteriously_broken', 't', 't')",
            [],
        );
        assert!(
            bad_job.is_err(),
            "non-canonical job status must be rejected"
        );
        let bad_segment = conn.execute(
            "INSERT INTO segments
             (id, job_id, section_id, ordinal, source_hash, prompt_version,
              provider, model, status)
             VALUES ('seg_bad', 'job_ok_running', 'sec', 1, 'sh', 'v',
                     'mock', 'mock', 'vendor_patched_state')",
            [],
        );
        assert!(
            bad_segment.is_err(),
            "non-canonical segment status must be rejected"
        );
    }

    drop(store);
    let _ = fs::remove_file(db_path);
}

#[test]
fn legacy_unknown_status_degrades_to_warn_on_open_without_data_loss() {
    let db_path = temp_path("legacy_unknown_status.sqlite");
    // Hand-edited style database: a status value BookForge never wrote sits in
    // a pre-CHECK table built straight from the v1 baseline.
    {
        let conn = Connection::open(&db_path).expect("legacy db opens");
        conn.execute_batch(include_str!("../../migrations/0001_initial.sql"))
            .expect("v1 baseline applies");
        conn.execute_batch(
            "
            INSERT INTO _migrations (version, name, applied_at) VALUES
              (1, 'initial', 'legacy'), (2, 'v1_0_1_input_snapshot', 'legacy'),
              (3, 'v1_1_segment_flags', 'legacy'), (4, 'v1_2_glossary_terms', 'legacy'),
              (5, 'v1_2_1_nullable_glossary_candidate_targets', 'legacy'),
              (6, 'v1_3_context_styles_entities', 'legacy'),
              (7, 'v2_4_human_corrections', 'legacy'), (8, 'v2_7_qa_findings', 'legacy');
            INSERT INTO jobs
              (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
            VALUES
              ('legacy_job', 'hash', 'Italian', 'mock', 'mock',
               'out_of_band', '1000', '1000');
            ",
        )
        .expect("legacy fixture initializes");
    }

    let store = JobStore::open(&db_path).expect("open must tolerate unknown rows");
    let diagnostics = store.take_diagnostics();
    assert!(
        diagnostics.iter().any(|note| note.contains("out_of_band")),
        "unknown value must warn on open: {diagnostics:?}"
    );

    // Serialized format is unchanged externally; only the decoder differs.
    let record = store
        .get_job("legacy_job")
        .expect("job reads")
        .expect("row exists");
    assert_eq!(
        record.status, "out_of_band",
        "raw text is preserved verbatim"
    );
    assert_eq!(
        record.job_status(),
        JobStatus::Unknown("out_of_band".to_string()),
        "unknown decodes defensively instead of panicking"
    );

    drop(store);

    // Repairing the data lets the next open finally apply the constraints.
    {
        let conn = Connection::open(&db_path).expect("repair reopens");
        conn.execute(
            "UPDATE jobs SET status = 'failed' WHERE id = 'legacy_job'",
            [],
        )
        .expect("repair applies");
    }
    let hardened = JobStore::open(&db_path).expect("repaired store opens");
    assert!(
        hardened.take_diagnostics().is_empty(),
        "canonical data no longer warns"
    );
    {
        let conn = hardened.conn.borrow();
        let applied: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM _migrations WHERE version = 10",
                [],
                |row| row.get(0),
            )
            .expect("migration ledger readable");
        assert_eq!(applied, 1, "hardening recorded once data conforms");
        let bogus = conn.execute(
            "INSERT INTO jobs
             (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
             VALUES ('x', 'h', 'l', 'p', 'm', 'still_bogus', 't', 't')",
            [],
        );
        assert!(bogus.is_err(), "constraints active after repair");
    }

    drop(hardened);
    let _ = fs::remove_file(db_path);
}

#[test]
fn unknown_segment_status_counts_toward_totals_but_no_bucket() {
    let db_path = temp_path("legacy_unknown_segment_status.sqlite");
    {
        let conn = Connection::open(&db_path).expect("legacy db opens");
        conn.execute_batch(include_str!("../../migrations/0001_initial.sql"))
            .expect("v1 baseline applies");
        conn.execute_batch(
            "
            INSERT INTO _migrations (version, name, applied_at) VALUES
              (1, 'initial', 'legacy'), (2, 'v1_0_1_input_snapshot', 'legacy'),
              (3, 'v1_1_segment_flags', 'legacy'), (4, 'v1_2_glossary_terms', 'legacy'),
              (5, 'v1_2_1_nullable_glossary_candidate_targets', 'legacy'),
              (6, 'v1_3_context_styles_entities', 'legacy'),
              (7, 'v2_4_human_corrections', 'legacy'), (8, 'v2_7_qa_findings', 'legacy');
            INSERT INTO jobs
              (id, input_hash, target_lang, provider, model, status, created_at, updated_at)
            VALUES
              ('j', 'h', 'Italian', 'mock', 'mock', 'stopped', '1', '2');
            INSERT INTO segments
              (id, job_id, section_id, ordinal,
               source_hash, prompt_version, provider, model, status)
            VALUES
              ('s1', 'j', 'sec', 0, 'c1', 'v', 'mock', 'mock', 'succeeded'),
              ('s2', 'j', 'sec', 1, 'c2', 'v', 'mock', 'mock', 'fancy_custom');
            ",
        )
        .expect("legacy fixture initializes");
    }

    let store = JobStore::open(&db_path).expect("store opens with warning");
    assert!(!store.take_diagnostics().is_empty());

    let summary = store.summary("j").expect("summary").expect("job exists");
    assert_eq!(summary.total_segments, 2, "unknown still counts");
    assert_eq!(summary.succeeded, 1);
    assert_eq!(summary.failed, 0, "unbucketed values stay unbucketed");

    let records = store.segment_records("j").expect("records");
    assert_eq!(
        records[1].segment_status(),
        SegmentStatus::Unknown("fancy_custom".to_string())
    );

    drop(store);
    let _ = fs::remove_file(db_path);
}

// ---------------------------------------------------------------------------
// STORE-17 part A: prune_jobs retention path
// ---------------------------------------------------------------------------

fn prune_fixture_job(store: &JobStore, label: &str) -> JobRecord {
    let input_path = temp_path(&format!("{label}-input.epub"));
    fs::write(&input_path, format!("{label} epub bytes")).expect("fixture input writes");
    store
        .create_job(CreateJob {
            input: &input_path,
            output: &temp_path(&format!("{label}-output.epub")),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("fixture job creates")
}

fn populate_full_job_tree(store: &JobStore, job: &JobRecord, artifacts_dir: &Path) {
    fs::create_dir_all(artifacts_dir).expect("artifacts dir exists");
    let events_path = artifacts_dir.join(format!("{}-events.jsonl", job.id));
    let report_json_path = artifacts_dir.join(format!("{}-report.json", job.id));
    let report_markdown_path = artifacts_dir.join(format!("{}-report.md", job.id));
    fs::write(&events_path, b"event").expect("events artifact writes");
    fs::write(&report_json_path, b"{}").expect("json report artifact writes");
    fs::write(&report_markdown_path, b"# Report").expect("md report artifact writes");

    store
        .update_job_event_path(&job.id, &events_path)
        .expect("events path set");
    store
        .update_job_report_paths(&job.id, &report_json_path, &report_markdown_path)
        .expect("report paths set");

    let segments = vec![segment("seg_a", 0), segment("seg_b", 1)];
    store
        .insert_segments(&job.id, &segments, "v1", "mock", "mock-prefix", "ns")
        .expect("segments inserted");
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
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("translation saved");
    store
        .save_needs_review(SaveNeedsReview {
            job_id: &job.id,
            segment_id: "seg_b",
            preserved_text: "Da rivedere",
            blocks: &[BlockTranslation {
                block_id: BlockId("b_000001".to_string()),
                text: "Da rivedere".to_string(),
            }],
            provider: "mock",
            model: "mock-prefix",
            prompt_version: "v1",
            error: "needs eyes on it",
            input_tokens: None,
            input_cached_tokens: None,
            output_tokens: None,
            tokens_estimated: false,
        })
        .expect("needs review saved");
    store
        .set_dashboard_segment_flag(&job.id, "seg_b", true)
        .expect("dashboard flag set");
    store
        .insert_segment_flags(&[NewSegmentFlag {
            job_id: &job.id,
            segment_id: "seg_a",
            kind: "dashboard_retry",
            note: Some("please retry"),
            suggested_source: None,
            suggested_target: None,
            consumed: true,
        }])
        .expect("flags inserted");
}

fn count_rows(store: &JobStore, table: &str, job_id: &str) -> i64 {
    store
        .conn
        .borrow()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE job_id = ?1"),
            params![job_id],
            |row| row.get(0),
        )
        .expect("count query")
}

#[test]
fn prune_jobs_deletes_whole_tree_atomically_and_protects_running() {
    let db_path = temp_path("prune_tree.sqlite");
    let artifacts_dir = temp_path("prune-artifacts-root");
    let store = JobStore::open(&db_path).expect("store opens");

    let finished = prune_fixture_job(&store, "finished");
    populate_full_job_tree(&store, &finished, &artifacts_dir);
    store.mark_job_stopped(&finished.id).expect("job stopped");

    // This one stays `running` from creation and is never touched.
    let running = prune_fixture_job(&store, "running");
    store
        .insert_segments(
            &running.id,
            &[segment("seg_live", 0)],
            "v1",
            "mock",
            "mock-prefix",
            "ns",
        )
        .expect("running segments inserted");

    let events_file_artifact = artifacts_dir.join(format!("{}-events.jsonl", finished.id));
    let report_json_artifact = artifacts_dir.join(format!("{}-report.json", finished.id));

    let report = store
        .prune_jobs(PruneJobsOptions::default())
        .expect("prunes");
    assert_eq!(
        report.protected_running_jobs, 1,
        "the running job is guarded"
    );
    assert_eq!(report.candidate_count, 1);
    assert_eq!(report.pruned_job_count(), 1);
    let deletion = &report.deletions[0];
    assert_eq!(deletion.job_id, finished.id);
    assert_eq!(deletion.segments, 2);
    assert!(deletion.translations >= 1);
    assert!(deletion.translation_blocks >= 2);
    assert!(
        deletion.qa_findings >= 1,
        "needs-review classification kept"
    );
    assert!(deletion.segment_flags >= 1);
    assert!(deletion.artifacts_removed.contains(&events_file_artifact));
    assert!(deletion.artifacts_removed.contains(&report_json_artifact));
    assert!(!events_file_artifact.exists(), "artifact file removed");
    assert!(!report_json_artifact.exists(), "artifact file removed");

    // FK-cascade correctness across ALL child tables: zero orphans remain.
    assert_eq!(count_rows(&store, "translations", &finished.id), 0);
    assert_eq!(count_rows(&store, "translation_blocks", &finished.id), 0);
    assert_eq!(count_rows(&store, "qa_findings", &finished.id), 0);
    assert_eq!(count_rows(&store, "segment_flags", &finished.id), 0);
    assert_eq!(count_rows(&store, "segments", &finished.id), 0);
    assert!(store.get_job(&finished.id).expect("lookup").is_none());
    assert!(
        store.get_job(&running.id).expect("lookup").is_some(),
        "running survives"
    );
    assert_eq!(count_rows(&store, "segments", &running.id), 1);

    // Second pass finds nothing more to do.
    let second = store
        .prune_jobs(PruneJobsOptions::default())
        .expect("idempotent");
    assert_eq!(second.candidate_count, 0);
    assert_eq!(second.pruned_job_count(), 0);

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn prune_jobs_dry_run_reports_without_modifying_anything() {
    let db_path = temp_path("prune_dry_run.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");
    let job = prune_fixture_job(&store, "dry");
    let artifacts_dir = temp_path("prune-dry-artifacts");
    populate_full_job_tree(&store, &job, &artifacts_dir);
    store.mark_job_stopped(&job.id).expect("stopped");
    let events_artifact = artifacts_dir.join(format!("{}-events.jsonl", job.id));

    let report = store
        .prune_jobs(PruneJobsOptions {
            dry_run: true,
            ..PruneJobsOptions::default()
        })
        .expect("dry run");
    assert!(report.dry_run);
    assert_eq!(report.candidate_count, 1);
    let deletion = &report.deletions[0];
    assert_eq!(deletion.segments, 2);
    assert!(deletion.segment_flags >= 1);

    // Nothing actually changed.
    assert!(store.get_job(&job.id).expect("lookup").is_some());
    assert_eq!(count_rows(&store, "segments", &job.id), 2);
    assert!(events_artifact.exists(), "dry run never touches artifacts");

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn prune_jobs_respects_older_than_and_keep_last_n() {
    use std::time::{SystemTime, UNIX_EPOCH};

    let db_path = temp_path("prune_filters.sqlite");
    let store = JobStore::open(&db_path).expect("store opens");

    let old_job = prune_fixture_job(&store, "old");
    let mid_job = prune_fixture_job(&store, "mid");
    let new_job = prune_fixture_job(&store, "new");
    for (id, stamp) in [
        (&old_job.id, "900"),
        (&mid_job.id, "1500"),
        (&new_job.id, "2000"),
    ] {
        {
            let conn = store.conn.borrow();
            conn.execute(
                "UPDATE jobs SET created_at = ?1, updated_at = ?1 WHERE id = ?2",
                params![stamp, id],
            )
            .expect("stamp set");
        }
        store.mark_job_stopped(id).expect("stopped");
    }

    // dry-ish age filter that excludes everything future.
    let none_match = store
        .prune_jobs(PruneJobsOptions {
            older_than: Some(SystemTime::UNIX_EPOCH),
            ..PruneJobsOptions::default()
        })
        .expect("future-only cutoff");
    assert_eq!(none_match.candidate_count, 0);

    // Age cutoff keeps only `mid`/`new` eligible; keep_last_n=1 then spares
    // the newest survivor (`new`) — deleting exactly `mid`.
    let cutoff_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 10;
    let cutoff = UNIX_EPOCH
        .checked_add(std::time::Duration::from_secs(cutoff_secs))
        .expect("cutoff");
    let report = store
        .prune_jobs(PruneJobsOptions {
            older_than: Some(cutoff),
            keep_last_n: Some(1),
            ..PruneJobsOptions::default()
        })
        .expect("prunes");
    assert_eq!(report.candidate_count, 3, "age floor is in the far future");
    assert_eq!(report.retained_by_keep_last_n, 1);
    assert_eq!(report.pruned_job_count(), 2, "mid+old deleted, newest kept");
    let deleted_ids: Vec<&str> = report.deletions.iter().map(|d| d.job_id.as_str()).collect();
    assert!(deleted_ids.contains(&old_job.id.as_str()));
    assert!(deleted_ids.contains(&mid_job.id.as_str()));
    assert!(store.get_job(&new_job.id).expect("lookup").is_some());
    assert!(store.get_job(&old_job.id).expect("lookup").is_none());

    // keep_last_n alone: rebuild order sensitivity by keeping 0 → all go.
    let everything = store
        .prune_jobs(PruneJobsOptions {
            older_than: Some(cutoff),
            keep_last_n: Some(0),
            ..PruneJobsOptions::default()
        })
        .expect("prunes rest");
    assert_eq!(everything.pruned_job_count(), 1, "only the newest remains");

    drop(store);
    let _ = fs::remove_file(db_path);
}

// ---------------------------------------------------------------------------
// STORE-17 part B: bounded/streaming input hashing
// ---------------------------------------------------------------------------

#[test]
fn file_hash_streams_chunks_matching_the_single_shot_digest() {
    use sha2::{Digest, Sha256};

    let db_path = temp_path("streaming_hash.sqlite");
    let big_input = temp_path("big-input.epub");
    // > 2x FILE_HASH_CHUNK_BYTES so at least three streaming chunks are read,
    // with an awkward remainder size to exercise partial-buffer handling.
    let total_bytes = 64 * 1024 * 2 + 7;
    let payload: Vec<u8> = (0..total_bytes).map(|i| (i % 251) as u8).collect();
    fs::write(&big_input, &payload).expect("large fixture writes");

    let store = JobStore::open(&db_path).expect("store opens");
    let job = store
        .create_job(CreateJob {
            input: &big_input,
            output: &temp_path("big-output.epub"),
            source_lang: Some("English"),
            target_lang: "Italian",
            provider: "mock",
            model: "mock-prefix",
            base_url: None,
            api_key_env: None,
            book_id: None,
            series_id: None,
        })
        .expect("job creates");

    let mut expected_hasher = Sha256::new();
    expected_hasher.update(&payload);
    let expected: String = expected_hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(job.input_hash, expected);

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(big_input);
}

// ---------------------------------------------------------------------------
// Audit remediation: S-1 / S-2 / S-3 regressions + file_hash NIT
// ---------------------------------------------------------------------------

#[test]
fn harden_failure_path_restores_foreign_key_enforcement() {
    use super::restore_safe_status_harden;

    let failing_step = |message: &'static str| {
        move |_conn: &mut Connection| -> rusqlite::Result<()> {
            Err(rusqlite::Error::InvalidParameterName(message.to_string()))
        }
    };

    // Case 1: the inner rebuild fails immediately (no transaction open).
    {
        let db_path = temp_path("harden_fk_restore_instant.sqlite");
        let mut conn = Connection::open(&db_path).expect("test db opens");
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE parent (id TEXT PRIMARY KEY);
             CREATE TABLE child (
               id TEXT PRIMARY KEY,
               parent_id TEXT NOT NULL REFERENCES parent(id)
             );",
        )
        .expect("schema builds");
        assert!(conn.is_autocommit(), "clean start is in autocommit");

        let outcome = restore_safe_status_harden(&mut conn, failing_step("boom-instant"));
        assert!(outcome.is_err(), "inner failure must propagate");

        let enforcement: bool = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys pragma readable");
        assert!(
            enforcement,
            "S-1: the failure path must switch foreign_keys back on"
        );
        let orphaned = conn.execute(
            "INSERT INTO child (id, parent_id) VALUES ('orphan', 'ghost')",
            [],
        );
        assert!(
            orphaned.is_err(),
            "enforcement must be active, not merely reporting ON"
        );
        drop(conn);
        let _ = fs::remove_file(db_path);
    }

    // Case 2: the inner step fails while a raw transaction it opened via SQL
    // is still open — exactly what a failed rollback leaves behind. PRAGMA
    // inside a transaction is a silent no-op, so the guard must roll that
    // stray transaction back before restoring.
    {
        let db_path = temp_path("harden_fk_restore_stray_txn.sqlite");
        let mut conn = Connection::open(&db_path).expect("test db opens");
        conn.execute_batch(
            "CREATE TABLE parent (id TEXT PRIMARY KEY);
             CREATE TABLE child (
               id TEXT PRIMARY KEY,
               parent_id TEXT NOT NULL REFERENCES parent(id)
             );",
        )
        .expect("schema builds");
        let stray_txn_step = |conn: &mut Connection| -> rusqlite::Result<()> {
            conn.execute_batch("BEGIN IMMEDIATE")?;
            Err(rusqlite::Error::InvalidParameterName(
                "boom-stray".to_string(),
            ))
        };

        let outcome = restore_safe_status_harden(&mut conn, stray_txn_step);
        assert!(outcome.is_err(), "stray-txn failure must propagate");

        assert!(
            conn.is_autocommit(),
            "the stranded transaction must be rolled back first"
        );
        let enforcement: bool = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys pragma readable");
        assert!(
            enforcement,
            "S-1: enforcement restored even when the txn was left open"
        );
        drop(conn);
        let _ = fs::remove_file(db_path);
    }

    // Control: success still records enforcement and passes through.
    {
        let db_path = temp_path("harden_fk_restore_success.sqlite");
        let mut conn = Connection::open(&db_path).expect("test db opens");
        conn.execute_batch("CREATE TABLE marker (x INTEGER);")
            .expect("schema builds");
        let outcome = restore_safe_status_harden(&mut conn, |_| Ok(()));
        assert!(outcome.is_ok(), "happy path stays happy");
        let enforcement: bool = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .expect("foreign_keys pragma readable");
        assert!(enforcement, "success path also leaves FK enforced");
        drop(conn);
        let _ = fs::remove_file(db_path);
    }
}

#[test]
fn prune_skips_job_flipped_to_running_after_selection() {
    // Regression for S-2: the running-guard used to be checked only during
    // selection; each per-job deletion transaction never re-read status, so a
    // job that turned `running` between the two phases could be deleted
    // underneath its live checkpointing process. Emulate the interleaving
    // with two connections: selection commits first, connection B flips the
    // job to `running`, then A drives its pruning path.
    let db_path = temp_path("prune_toctou_running.sqlite");
    let artifacts_dir = temp_path("prune-toctou-artifacts");
    let store = JobStore::open(&db_path).expect("store opens (conn A)");

    let victim = prune_fixture_job(&store, "victim");
    populate_full_job_tree(&store, &victim, &artifacts_dir);
    store.mark_job_stopped(&victim.id).expect("job stopped");

    // Phase 1 — select candidates on connection A. The job is stopped here.
    let selection = store
        .select_prune_candidates(None, None)
        .expect("selection runs");
    assert_eq!(selection.candidate_count, 1);
    assert_eq!(selection.to_delete, vec![victim.id.clone()]);
    assert_eq!(selection.protected_running_jobs, 0);

    // Phase 2 — connection B flips the selected job to `running` after A's
    // selection transaction has already committed.
    {
        let conn_b = Connection::open(&db_path).expect("second connection");
        let flipped = conn_b
            .execute(
                "UPDATE jobs SET status = 'running' WHERE id = ?1",
                params![victim.id],
            )
            .expect("concurrent flip applies");
        assert_eq!(flipped, 1);
    }

    // Phase 3 — drive A's deletion path for the stale candidate list.
    let options = PruneJobsOptions::default();
    let mut report = PruneJobsReport {
        dry_run: false,
        candidate_count: selection.candidate_count,
        protected_running_jobs: selection.protected_running_jobs,
        retained_by_keep_last_n: selection.retained_by_keep_last_n,
        deletions: Vec::new(),
    };
    store
        .execute_prune_selection(&selection.to_delete, &options, &mut report)
        .expect("execution runs");

    assert!(
        report.deletions.is_empty(),
        "the re-check inside the deletion txn must skip this job entirely"
    );
    assert_eq!(
        report.protected_running_jobs, 1,
        "the late flip counts as protected"
    );
    assert_eq!(
        report.candidate_count, 0,
        "a running job is no longer an honest prune candidate"
    );

    // The whole tree plus artifacts survive untouched.
    assert!(
        store.get_job(&victim.id).expect("lookup").is_some(),
        "flipped-to-running job row survives"
    );
    assert_eq!(count_rows(&store, "segments", &victim.id), 2);
    assert!(
        count_rows(&store, "translations", &victim.id) >= 1,
        "model translation rows survive the skip"
    );
    assert!(
        artifacts_dir
            .join(format!("{}-events.jsonl", victim.id))
            .exists(),
        "artifact files are not unlinked for protected jobs"
    );

    // Sanity: once stopped again, pruning proceeds normally.
    store.mark_job_stopped(&victim.id).expect("stopped again");
    let second = store.prune_jobs(options).expect("normal prune resumes");
    assert_eq!(second.pruned_job_count(), 1);

    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_dir_all(artifacts_dir);
}

#[test]
fn request_segment_retry_freeze_check_rides_inside_the_write_txn() {
    // Regression for S-3 (H-1 TOCTOU family): the human-correction freeze
    // check used to run BEFORE opening the IMMEDIATE write transaction. Hold
    // the writer lock with a second connection so the retry blocks at the
    // transaction door, land a frozen correction meanwhile, then release:
    // the retry's in-transaction freeze check must now see it and refuse.
    // The pre-transaction placement would instead pass the window and flip
    // the segment to retry_pending over the correction.
    let db_path = temp_path("retry_freeze_inside_txn.sqlite");
    let input_path = temp_path("input.sqlite-retry.epub");
    fs::write(&input_path, b"epub bytes").expect("input fixture writes");

    let store = JobStore::open(&db_path).expect("store opens");
    let job_id = {
        let job = store
            .create_job(CreateJob {
                input: &input_path,
                output: &temp_path("output-retry.epub"),
                source_lang: Some("English"),
                target_lang: "Italian",
                provider: "mock",
                model: "mock-prefix",
                base_url: None,
                api_key_env: None,
                book_id: None,
                series_id: None,
            })
            .expect("job creates");
        store
            .insert_segments(
                &job.id,
                &[segment("seg_a", 0)],
                "v1",
                "mock",
                "mock-prefix",
                "freeze_ns",
            )
            .expect("segments insert");
        store.mark_job_stopped(&job.id).expect("stopped for retry");
        job.id
    };

    {
        // Second process grabs the IMMEDIATE writer lock FIRST so the
        // retry's transaction below deterministically starts only after the
        // correction is committed.
        let conn_b = Connection::open(&db_path).expect("second connection");
        conn_b
            .execute_batch("BEGIN IMMEDIATE")
            .expect("writer lock acquired before the retry can begin");
        let job_id_for_thread = job_id.clone();
        let db_path_for_thread = db_path.clone();
        let handle = std::thread::spawn(move || {
            // All migrations are already recorded, so this open takes no
            // write lock and proceeds happily while B holds the writer.
            let retry_store = JobStore::open(&db_path_for_thread).expect("retry-side store opens");
            retry_store.request_segment_retry(&job_id_for_thread, "seg_a", Some("late guidance"))
        });
        std::thread::sleep(std::time::Duration::from_millis(100));

        conn_b
            .execute(
                "INSERT INTO translations
                 (segment_id, job_id, translated_text, provider, model, prompt_version,
                  created_at, origin, human_corrected, corrected_at)
                 VALUES ('seg_a', ?1, 'Correzione umana', 'manual', 'manual', 'v1',
                         '2000', 'manual', 1, '2000')",
                params![job_id],
            )
            .expect("frozen translation inserts under the lock");
        conn_b
            .execute(
                "INSERT INTO translation_blocks
                 (segment_id, job_id, block_id, translated_text)
                 VALUES ('seg_a', ?1, 'b_000000', 'Correzione umana')",
                params![job_id],
            )
            .expect("frozen block inserts under the lock");
        conn_b
            .execute_batch("COMMIT")
            .expect("correction committed");

        let result = handle.join().expect("retry thread joins without panic");
        match result {
            Err(StoreError::InvalidCorrection(message)) => {
                assert!(
                    message.contains("frozen human correction"),
                    "the in-txn freeze check must reject, got: {message}"
                );
            }
            other => panic!("expected InvalidCorrection from the freeze check, got {other:?}"),
        }
    }

    // The segment was NOT flipped and the correction survives verbatim.
    {
        let conn = store.conn.borrow();
        let status: String = conn
            .query_row(
                "SELECT status FROM segments WHERE job_id = ?1 AND id = 'seg_a'",
                params![job_id],
                |row| row.get(0),
            )
            .expect("segment readable");
        assert_eq!(
            status, "queued",
            "a frozen segment must never transition to retry_pending"
        );
        let (origin, corrected): (String, i64) = conn
            .query_row(
                "SELECT origin, human_corrected FROM translations WHERE job_id = ?1 AND segment_id = 'seg_a'",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("translation readable");
        assert_eq!(origin, "manual");
        assert_eq!(corrected, 1);
    }
    drop(store);
    let _ = fs::remove_file(db_path);
    let _ = fs::remove_file(input_path);
}

#[test]
fn file_hash_of_empty_input_is_the_canonical_sha256_digest() {
    let empty_input = temp_path("empty-input.epub");
    fs::write(&empty_input, b"").expect("empty fixture writes");

    let hash = file_hash(&empty_input).expect("hashing succeeds");
    assert_eq!(
        hash, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        "streaming zero chunks must equal SHA-256 of the empty string"
    );

    let _ = fs::remove_file(empty_input);
}
