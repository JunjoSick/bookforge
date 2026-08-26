use super::*;
use super::{assets::*, audio::*, glossary::*, jobs::*, options::*, security::*, translation::*};
use axum::http::HeaderValue;

const TEST_HOST: &str = "127.0.0.1:8765";
const TEST_DEADLOCK_TIMEOUT: Duration = Duration::from_secs(30);

fn test_state(token: &str) -> AppState {
    AppState {
        refresh: Duration::from_millis(250),
        csrf_token: token.to_string(),
        auth_enabled: true,
        host_port: 8765,
        upload_dir: PathBuf::from(UPLOAD_DIR),
        keys: Arc::new(Mutex::new(HashMap::new())),
        elevenlabs_voices: Arc::new(Mutex::new(None)),
        audio_cancels: Arc::new(Mutex::new(HashMap::new())),
        store_path: default_store_path(),
        runtime_lease_stale_after: crate::control::RUNTIME_LEASE_STALE_AFTER,
        correction_locks: Arc::new(Mutex::new(HashMap::new())),
        launch_slots: Arc::new(Mutex::new(0)),
        resume_launches: None,
        resume_child_environments: None,
        audio_restart_cancels: None,
    }
}

/// Like [`test_state`], but pointed at an isolated store path instead of
/// the process-relative default — lets tests exercise the store-backed
/// mutation endpoints against a temp-dir database without chdir'ing the
/// (shared, per-process) current directory, which would race across
/// parallel test threads.
fn test_state_with_store(token: &str, store_path: PathBuf) -> AppState {
    AppState {
        store_path,
        ..test_state(token)
    }
}

fn test_state_with_upload_dir(token: &str, upload_dir: PathBuf) -> AppState {
    AppState {
        upload_dir,
        ..test_state(token)
    }
}

#[test]
fn bind_must_be_loopback() {
    let local: SocketAddr = "127.0.0.1:8765".parse().unwrap();
    let remote: SocketAddr = "0.0.0.0:8765".parse().unwrap();

    assert!(require_loopback_bind(local).is_ok());
    assert!(require_loopback_bind(remote).is_err());
}

#[test]
fn dashboard_host_header_allows_only_loopback_names_on_bound_port() {
    for host in [
        "127.0.0.1:8765",
        "localhost:8765",
        "LOCALHOST:8765",
        "[::1]:8765",
    ] {
        assert!(
            dashboard_host_value_allowed(host, 8765),
            "{host} should be allowed"
        );
    }

    for host in [
        "",
        "127.0.0.1",
        "127.0.0.1:8766",
        " 127.0.0.1:8765",
        "127.0.0.1:8765 ",
        "127.0.0.1.evil.test:8765",
        "localhost.evil.test:8765",
        "evil.test:8765",
        "127.0.0.1:8765.evil.test",
        "127.0.0.1:08765",
        "[::1]:8766",
        "[::ffff:127.0.0.1]:8765",
    ] {
        assert!(
            !dashboard_host_value_allowed(host, 8765),
            "{host} should be rejected"
        );
    }
}

#[test]
fn provider_key_envs_cover_supported_cloud_providers() {
    assert_eq!(provider_key_env("deepseek"), Some("DEEPSEEK_API_KEY"));
    assert_eq!(provider_key_env("openrouter"), Some("OPENROUTER_API_KEY"));
    assert_eq!(
        provider_key_env("openai-compatible"),
        Some("OPENAI_API_KEY")
    );
    assert!(provider_key_env("mock").is_none());
}

#[test]
fn dashboard_child_environment_omits_unneeded_provider_secrets() {
    let mut command = tokio::process::Command::new("bookforge");
    let fake_parent_environment = [
        (
            OsString::from("DEEPSEEK_API_KEY"),
            OsString::from("fake-parent-secret"),
        ),
        (OsString::from("PATH"), OsString::from("fake-path")),
    ];
    configure_dashboard_child_environment_from(
        &mut command,
        fake_parent_environment,
        Some(("ELEVENLABS_API_KEY", "required-child-key")),
    );

    let child_environment = command
        .as_std()
        .get_envs()
        .map(|(name, value)| (name.to_os_string(), value.map(ToOwned::to_owned)))
        .collect::<HashMap<_, _>>();
    assert!(!child_environment.contains_key(std::ffi::OsStr::new("DEEPSEEK_API_KEY")));
    assert_eq!(
        child_environment
            .get(std::ffi::OsStr::new("PATH"))
            .and_then(|value| value.as_deref()),
        Some(std::ffi::OsStr::new("fake-path"))
    );
    assert_eq!(
        child_environment
            .get(std::ffi::OsStr::new("ELEVENLABS_API_KEY"))
            .and_then(|value| value.as_deref()),
        Some(std::ffi::OsStr::new("required-child-key"))
    );
}

#[test]
fn dashboard_options_include_common_languages_and_models() {
    let options = dashboard_options_payload();
    assert!(options.languages.contains(&"Italian"));
    assert!(options.languages.contains(&"English"));
    assert!(options.languages.contains(&"Toki Pona"));

    let deepseek = options
        .providers
        .iter()
        .find(|provider| provider.id == "deepseek")
        .expect("deepseek provider option should exist");
    assert_eq!(deepseek.default_model, "deepseek-v4-flash");
    assert!(deepseek.models.contains(&"deepseek-v4-pro"));

    let openrouter = options
        .providers
        .iter()
        .find(|provider| provider.id == "openrouter")
        .expect("openrouter provider option should exist");
    assert!(openrouter.models.contains(&"google/gemini-2.5-flash"));

    let gemini_tts = options
        .audio_providers
        .iter()
        .find(|provider| provider.id == "gemini")
        .expect("Gemini TTS provider option should exist");
    assert_eq!(gemini_tts.default_voice, "Kore");
    assert_eq!(gemini_tts.formats, &["wav", "pcm"]);
    assert!(!gemini_tts.supports_speed);

    let elevenlabs = options
        .audio_providers
        .iter()
        .find(|provider| provider.id == "elevenlabs")
        .expect("ElevenLabs provider option should exist");
    assert!(elevenlabs.requires_voice);
    assert!(elevenlabs.supports_auto_model);
    assert!(!elevenlabs.supports_instructions);
    assert_eq!(elevenlabs.models.first(), Some(&"eleven_v3"));
    assert_eq!(elevenlabs.default_model, "");
    assert_eq!(elevenlabs.max_chars, 10_000);
    assert_eq!(audio_provider_max_chars("openai", "anything"), 4_096);
    assert_eq!(audio_provider_max_chars("elevenlabs", ""), 10_000);
    assert_eq!(audio_provider_max_chars("elevenlabs", "eleven_v3"), 5_000);
    assert_eq!(
        audio_provider_max_chars("elevenlabs", "eleven_flash_v2_5"),
        40_000
    );
}

/// The browser used to carry its own table of ElevenLabs per-model limits,
/// which is a copy that can drift from the one synthesis actually enforces.
/// The options payload now serves them, so guard both halves: the payload
/// agrees with the canonical function, and the JS no longer hardcodes them.
#[test]
fn audio_options_serve_per_model_limits_so_the_browser_keeps_no_copy() {
    let options = dashboard_options_payload();
    let elevenlabs = options
        .audio_providers
        .iter()
        .find(|provider| provider.id == "elevenlabs")
        .expect("elevenlabs provider should be offered");

    assert!(
        !elevenlabs.model_max_chars.is_empty(),
        "per-model limits must reach the browser"
    );
    for model in elevenlabs.models {
        assert_eq!(
            elevenlabs.model_max_chars.get(model).copied(),
            Some(bookforge_audio::elevenlabs_model_max_input_chars(model)),
            "served limit for {model} must match the synthesis path"
        );
    }

    assert!(
        DASHBOARD_JS.contains("provider.model_max_chars"),
        "the browser must read limits from the payload"
    );
    for stale in ["eleven_flash_v2_5:40000", "eleven_v3:5000"] {
        assert!(
            !DASHBOARD_JS.contains(stale),
            "dashboard.js must not reintroduce a hardcoded limit table ({stale})"
        );
    }
}

#[test]
fn dashboard_escapes_dynamic_html_fields() {
    assert!(DASHBOARD_HTML.contains("function esc(value)"));
    assert!(DASHBOARD_HTML.contains("${esc(d.id)}"));
    assert!(DASHBOARD_HTML.contains("${esc(body)}"));
    assert!(DASHBOARD_HTML.contains("${esc(a.id)}"));
    assert!(DASHBOARD_HTML.contains("${esc(w.sourcePath)}"));
    assert!(DASHBOARD_HTML.contains("${esc(audioWarningMessage(warning))}"));
}

#[test]
fn dashboard_openai_compatible_base_url_must_be_https() {
    assert!(dashboard_base_url_uses_https("https://api.example.com/v1"));
    assert!(!dashboard_base_url_uses_https("http://api.example.com/v1"));
    assert!(!dashboard_base_url_uses_https("https://"));
    assert!(!dashboard_base_url_uses_https("not a url"));
}

#[tokio::test]
async fn child_startup_check_reports_immediate_success_exit() -> Result<()> {
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let expected = tokio::time::timeout(TEST_DEADLOCK_TIMEOUT, child.wait())
        .await
        .expect("help child should exit before the deadlock guard")?;
    let status = child_exit_status_after(&mut child, Duration::ZERO)
        .await?
        .expect("completed help child should have an exit status");

    assert_eq!(status, expected);
    assert!(expected.success());
    Ok(())
}

#[test]
fn mutating_routes_require_dashboard_token() {
    let state = test_state("token-123");
    let headers = HeaderMap::new();
    assert!(reject_mutation(&headers, &state).is_some());

    let mut headers = HeaderMap::new();
    headers.insert(CSRF_HEADER, HeaderValue::from_static("token-123"));
    assert!(reject_mutation(&headers, &state).is_none());
}

#[test]
fn mutating_routes_reject_cross_site_browser_requests() {
    let state = test_state("token-123");
    let mut headers = HeaderMap::new();
    headers.insert(CSRF_HEADER, HeaderValue::from_static("token-123"));
    headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));

    assert!(reject_mutation(&headers, &state).is_some());
}

#[test]
fn csrf_token_compare_matches_only_exact_token() {
    assert!(constant_time_eq(b"token-123", b"token-123"));
    assert!(!constant_time_eq(b"token-123", b"token-124"));
    assert!(!constant_time_eq(b"token-123", b"token-1234"));
}

#[tokio::test]
async fn dashboard_rejects_untrusted_host_header_before_serving_html() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", "evil.test:8765")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn dashboard_serves_loopback_host_header() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", "localhost:8765")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("content-security-policy"),
        Some(&HeaderValue::from_static(DASHBOARD_CONTENT_SECURITY_POLICY))
    );
    for (name, value) in [
        ("x-frame-options", "DENY"),
        ("x-content-type-options", "nosniff"),
        ("referrer-policy", "no-referrer"),
        ("cache-control", "no-store"),
    ] {
        assert_eq!(
            response.headers().get(name),
            Some(&HeaderValue::from_static(value))
        );
    }
}

#[tokio::test]
async fn retry_endpoint_rejects_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs/not-real/retry")
                .header("host", TEST_HOST)
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn audiobook_endpoint_rejects_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let body = "--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B--\r\n";
    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/audiobook")
                .header("host", TEST_HOST)
                .header("content-type", "multipart/form-data; boundary=B")
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn audiobook_estimate_endpoint_rejects_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let body = "--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B--\r\n";
    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/audiobook/estimate")
                .header("host", TEST_HOST)
                .header("content-type", "multipart/form-data; boundary=B")
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn elevenlabs_voices_without_key_returns_conflict() {
    let response = get_route(
        &dashboard_router(test_state("token-123")),
        "/api/audio/voices?provider=elevenlabs",
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);

    // And without the session token it never reaches the handler at all.
    let rejected = get_route_with_token(
        &dashboard_router(test_state("token-123")),
        "/api/audio/voices?provider=elevenlabs",
        None,
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn resolved_model_uses_last_synthesis_id_colon() {
    assert_eq!(
        resolved_model_from_synthesis_id("elevenlabs:https://api.elevenlabs.io:443/v1:eleven_v3"),
        Some("eleven_v3")
    );
}

#[test]
fn audiobook_command_args_omit_auto_model_and_include_explicit_model() {
    let build = |model| {
        audiobook_command_args(
            Path::new("book.epub"),
            Path::new("audio-out"),
            "elevenlabs",
            model,
            "voice-id",
            "mp3",
            1.0,
            2_000,
            4,
            None,
            None,
            true,
            true,
            Some("ELEVENLABS_API_KEY"),
            &AudiobookCommandOptions::default(),
        )
    };

    let auto = build(None);
    assert!(!auto.iter().any(|arg| arg == "--model"));

    let explicit = build(Some("eleven_flash_v2_5"));
    let model_flag = explicit
        .iter()
        .position(|arg| arg == "--model")
        .expect("explicit model should add --model");
    assert_eq!(
        explicit.get(model_flag + 1),
        Some(&OsString::from("eleven_flash_v2_5"))
    );
}

#[test]
fn audiobook_command_args_include_only_advanced_flags_that_are_set() {
    let build = |advanced: &AudiobookCommandOptions| {
        audiobook_command_args(
            Path::new("book.epub"),
            Path::new("audio-out"),
            "elevenlabs",
            Some("eleven_flash_v2_5"),
            "voice-id",
            "mp3",
            1.0,
            2_000,
            4,
            None,
            None,
            true,
            true,
            Some("ELEVENLABS_API_KEY"),
            advanced,
        )
    };

    let defaults = build(&AudiobookCommandOptions::default());
    for flag in ["--single", "--loudnorm", "--seed", "--language"] {
        assert!(!defaults.iter().any(|arg| arg == flag), "included {flag}");
    }

    let configured = build(&AudiobookCommandOptions {
        single: true,
        loudnorm: true,
        seed: Some(u32::MAX),
        language: Some("pt-BR".to_string()),
        ..AudiobookCommandOptions::default()
    });
    for flag in ["--single", "--loudnorm", "--seed", "--language"] {
        assert!(configured.iter().any(|arg| arg == flag), "omitted {flag}");
    }
    assert!(configured.iter().any(|arg| arg == "4294967295"));
    assert!(configured.iter().any(|arg| arg == "pt-BR"));
}

#[test]
fn audiobook_language_validation_matches_dashboard_contract() {
    for valid in ["it", "en-US", "pt-BR"] {
        assert!(valid_audio_language(valid), "rejected {valid:?}");
    }
    for invalid in ["../etc", "toolongtoken", ""] {
        assert!(!valid_audio_language(invalid), "accepted {invalid:?}");
    }
}

#[test]
fn audiobook_gap_values_are_clamped_to_ten_seconds() {
    assert_eq!(clamp_audio_gap(0), 0);
    assert_eq!(clamp_audio_gap(600), 600);
    assert_eq!(clamp_audio_gap(99_999), 10_000);
}

// -----------------------------------------------------------------------
// AUDIO-6 / ASYM-1 + AUDIO-7: capability-gated launches and estimator
// preprocessing parity with the launcher.
// -----------------------------------------------------------------------

/// The dashboard must refuse seed-for-the-wrong-provider before writing any
/// upload or operation directory and before spawning a child that would only
/// fail on the same check later inside the CLI.
#[tokio::test]
async fn audiobook_launch_rejects_seed_before_spawning_a_doomed_child() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let upload_dir = tempfile::tempdir().expect("temp dir should be created");
    let epub_dir = tempfile::tempdir().expect("fixture dir should be created");
    let epub_path = epub_dir.path().join("fixture.epub");
    build_fixture_epub(&epub_path);
    let epub_bytes = std::fs::read(&epub_path).expect("fixture EPUB should read");
    let mut body = Vec::new();
    body.extend_from_slice(
        b"--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.epub\"\r\nContent-Type: application/epub+zip\r\n\r\n",
    );
    body.extend_from_slice(&epub_bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        b"--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nopenai\r\n--B\r\n",
    );
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"seed\"\r\n\r\n7\r\n--B--\r\n");

    let response = dashboard_router(test_state_with_upload_dir(
        "token-123",
        upload_dir.path().to_path_buf(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/audiobook")
            .header("host", TEST_HOST)
            .header(CSRF_HEADER, "token-123")
            .header("content-type", "multipart/form-data; boundary=B")
            .body(Body::from(body))
            .expect("request should build"),
    )
    .await
    .expect("route should respond");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let payload = response_json(response).await;
    let error = payload["error"].as_str().expect("error message");
    assert!(
        error.contains("--seed is supported only with --provider elevenlabs"),
        "{payload}"
    );
    assert_eq!(
        std::fs::read_dir(upload_dir.path())
            .expect("upload dir should be readable")
            .count(),
        0,
        "a capability-rejected launch must not leave uploads or operation dirs"
    );
}

/// The estimate endpoint plans through `read_narration_source` — the same
/// PDF-cleanup reflow plus page-grouping pipeline a real launch runs — so its
/// chapter/chunk/character numbers cannot drift from what synthesis builds.
#[tokio::test]
async fn audiobook_estimate_matches_the_shared_launcher_chunk_plan() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let epub_dir = tempfile::tempdir().expect("fixture dir should be created");
    let epub_path = epub_dir.path().join("fixture.epub");
    build_fixture_epub(&epub_path);
    let bytes = std::fs::read(&epub_path).expect("fixture EPUB should read");

    let scratch = tempfile::tempdir().expect("scratch dir should be created");
    let narration = bookforge_audio::read_narration_source(&epub_path, scratch.path())
        .expect("shared preprocessing should parse the fixture");
    let options = bookforge_audio::AudiobookOptions {
        max_chars: 2_000,
        ..bookforge_audio::AudiobookOptions::default()
    };
    let plan = bookforge_audio::plan_chunks(&narration.book, &options);
    let expected_chapters = plan
        .iter()
        .map(|chunk| chunk.chapter_index)
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let expected_characters: usize = plan.iter().map(|chunk| chunk.chars).sum();
    assert!(!plan.is_empty(), "fixture should yield narratable chunks");
    // The fixture is not PDF-derived, so both sides must agree it leaves
    // page grouping off — the boolean flows through the shared pipeline too.
    assert!(!narration.pdf_page_grouping);

    let boundary = "B";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.epub\"\r\nContent-Type: application/epub+zip\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let response = {
        let upload_dir = tempfile::tempdir().expect("temp dir should be created");
        dashboard_router(test_state_with_upload_dir(
            "token-123",
            upload_dir.path().to_path_buf(),
        ))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/audiobook/estimate")
                .header("host", TEST_HOST)
                .header(CSRF_HEADER, "token-123")
                .header("content-type", "multipart/form-data; boundary=B")
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("route should respond")
    };

    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    assert_eq!(payload["chapters"], json!(expected_chapters));
    assert_eq!(payload["chunks"], json!(plan.len()));
    assert_eq!(payload["characters"], json!(expected_characters));
}

#[tokio::test]
async fn audiobook_cancel_rejects_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/audiobooks/example/cancel")
                .header("host", TEST_HOST)
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

fn write_test_audiobook_operation(
    upload_dir: &Path,
    id: &str,
    manifest: serde_json::Value,
    process: serde_json::Value,
) -> PathBuf {
    let out_dir = upload_dir.join(format!("audiobook-{id}"));
    std::fs::create_dir_all(&out_dir).expect("operation directory should be created");
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec(&manifest).expect("manifest should serialize"),
    )
    .expect("manifest should be written");
    std::fs::write(
        out_dir.join("process.json"),
        serde_json::to_vec(&process).expect("process state should serialize"),
    )
    .expect("process state should be written");
    out_dir
}

#[tokio::test]
async fn audiobook_index_scans_durable_operations_newest_first() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    write_test_audiobook_operation(
        temp.path(),
        "older",
        json!({
            "title": "Older Book",
            "status": "running",
            "completed_chunks": 1,
            "chapters": 1,
            "chunks": [{"status": "synthesized"}],
            "updated_at_ms": 10,
        }),
        json!({"status": "running", "updated_at_ms": 11}),
    );
    write_test_audiobook_operation(
        temp.path(),
        "newer",
        json!({
            "title": "Newer Book",
            "status": "succeeded",
            "completed_chunks": 2,
            "chapters": 1,
            "chunks": [{"status": "synthesized"}, {"status": "cached"}],
            "updated_at_ms": 20,
        }),
        json!({
            "status": "succeeded",
            "warnings": [{"message": "chapter markers were unavailable"}],
            "updated_at_ms": 21,
        }),
    );
    std::fs::create_dir_all(temp.path().join("not-an-audiobook"))
        .expect("unrelated directory should be created");

    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));
    let response = get_route(&router, "/api/audiobooks").await;
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    let items = payload.as_array().expect("index should be an array");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "newer");
    assert_eq!(items[0]["title"], "Newer Book");
    assert_eq!(items[0]["total_chunks"], 2);
    assert_eq!(
        items[0]["warnings"][0]["message"],
        "chapter markers were unavailable"
    );
    assert_eq!(items[1]["id"], "older");
}

#[tokio::test]
async fn audiobook_cancel_uses_persisted_pid_after_server_restart() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out_dir = write_test_audiobook_operation(
        temp.path(),
        "restartable",
        json!({"status": "running", "chunks": []}),
        json!({
            "status": "running",
            // The recorded process must be verifiably ours before cancel will
            // signal it; the test binary itself is a real BookForge executable.
            "pid": std::process::id(),
            "auto_model": true,
            "warnings": ["stitch fallback"],
            "updated_at_ms": 10,
        }),
    );
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let mut state = test_state_with_upload_dir("token-123", temp.path().to_path_buf());
    state.audio_restart_cancels = Some(cancelled.clone());
    assert!(state.audio_cancels.lock().unwrap().is_empty());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        "/api/audiobooks/restartable/cancel",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(*cancelled.lock().unwrap(), vec![std::process::id()]);
    let process: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("process.json")).expect("process state should remain"),
    )
    .expect("process state should be JSON");
    assert_eq!(process["status"], "cancelled");
    assert_eq!(process["auto_model"], true);
    assert_eq!(process["warnings"][0], "stitch fallback");
}

#[tokio::test]
async fn audiobook_source_resolves_finished_translation_by_job_id() {
    let fixture = build_mutation_fixture();
    build_fixture_epub(&fixture.output_path);
    let state = test_state_with_store("token-123", fixture.store_path.clone());
    let fields = HashMap::from([("source_job_id".to_string(), fixture.job_id.clone())]);

    let source = resolve_audiobook_source(
        &state,
        &fields,
        None,
        "ignored-upload-name.epub".to_string(),
    )
    .await
    .expect("finished translation output should resolve");

    assert_eq!(source.file_name, "output.epub");
    assert!(source.bytes.starts_with(b"PK"));
}

#[tokio::test]
async fn audiobook_artifact_supports_ranges_and_rejects_unsatisfiable_ranges() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let out_dir = temp.path().join("audiobook-range-test");
    std::fs::create_dir_all(&out_dir).expect("operation directory should be created");
    std::fs::write(out_dir.join("audiobook.m4b"), b"0123456789")
        .expect("artifact should be written");
    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));

    let partial = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/audiobooks/range-test/artifact")
                .header("host", TEST_HOST)
                .header(CSRF_HEADER, "token-123")
                .header("range", "bytes=2-5")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        partial.headers().get("content-range"),
        Some(&HeaderValue::from_static("bytes 2-5/10"))
    );
    assert_eq!(
        partial.headers().get("accept-ranges"),
        Some(&HeaderValue::from_static("bytes"))
    );
    let body = axum::body::to_bytes(partial.into_body(), usize::MAX)
        .await
        .expect("partial body should read");
    assert_eq!(&body[..], b"2345");

    let unsatisfiable = router
        .oneshot(
            Request::builder()
                .uri("/api/audiobooks/range-test/artifact")
                .header("host", TEST_HOST)
                .header(CSRF_HEADER, "token-123")
                .header("range", "bytes=20-")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(unsatisfiable.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        unsatisfiable.headers().get("content-range"),
        Some(&HeaderValue::from_static("bytes */10"))
    );
}

#[test]
fn audiobook_operation_ids_cannot_escape_the_upload_directory() {
    assert!(valid_audiobook_id("1234-safe_id"));
    for invalid in ["", "../escape", "with/slash", "with.dot", "with space"] {
        assert!(!valid_audiobook_id(invalid), "accepted {invalid:?}");
    }
}

#[tokio::test]
async fn control_endpoints_reject_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let router = dashboard_router(test_state("token-123"));
    for command in ["pause", "resume", "stop"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/jobs/not-real/{command}"))
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{command}");
    }
}

#[tokio::test]
async fn estimate_endpoint_rejects_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    // A well-formed multipart body so the Multipart extractor succeeds and
    // the handler's own CSRF check is what rejects the request.
    let body = "--B\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n--B--\r\n";
    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/estimate")
                .header("host", TEST_HOST)
                .header("content-type", "multipart/form-data; boundary=B")
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn glossary_mutations_reject_missing_dashboard_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let add = dashboard_router(test_state("token-123"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/glossary")
                    .header("host", TEST_HOST)
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source":"a","target":"b","source_language":"English","target_language":"Italian"}"#,
                    ))
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
    assert_eq!(add.status(), StatusCode::UNAUTHORIZED);

    let remove = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/glossary/1")
                .header("host", TEST_HOST)
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(remove.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn translate_rejects_non_https_openai_compatible_base_url() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let body = concat!(
        "--B\r\n",
        "Content-Disposition: form-data; name=\"file\"; filename=\"book.epub\"\r\n",
        "Content-Type: application/epub+zip\r\n\r\n",
        "not-a-real-epub\r\n",
        "--B\r\n",
        "Content-Disposition: form-data; name=\"target\"\r\n\r\n",
        "Italian\r\n",
        "--B\r\n",
        "Content-Disposition: form-data; name=\"provider\"\r\n\r\n",
        "openai-compatible\r\n",
        "--B\r\n",
        "Content-Disposition: form-data; name=\"base_url\"\r\n\r\n",
        "http://api.example.test/v1\r\n",
        "--B\r\n",
        "Content-Disposition: form-data; name=\"model\"\r\n\r\n",
        "test-model\r\n",
        "--B\r\n",
        "Content-Disposition: form-data; name=\"api_key\"\r\n\r\n",
        "sk-test-key\r\n",
        "--B--\r\n",
    );

    let response = dashboard_router(test_state("token-123"))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/translate")
                .header("host", TEST_HOST)
                .header(CSRF_HEADER, "token-123")
                .header("content-type", "multipart/form-data; boundary=B")
                .body(Body::from(body))
                .expect("request should build"),
        )
        .await
        .expect("route should respond");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn glossary_scope_and_category_parse_with_sane_defaults() {
    assert_eq!(parse_glossary_scope("book"), GlossaryScopeKind::Book);
    assert_eq!(parse_glossary_scope("series"), GlossaryScopeKind::Series);
    assert_eq!(parse_glossary_scope("whatever"), GlossaryScopeKind::Global);
    assert_eq!(parse_glossary_category("place"), GlossaryCategory::Place);
    assert_eq!(parse_glossary_category("mystery"), GlossaryCategory::Other);
}

#[test]
fn dashboard_ships_all_screen_renderers() {
    for marker in [
        "function renderLibrary",
        "function renderWizard",
        "function renderAudiobook",
        "function renderProgress",
        "function drawReview",
        "function drawValidation",
        "function renderGlossary",
        "function bfAudioEstimate",
        "/api/audiobook/estimate",
        "Advanced",
    ] {
        assert!(DASHBOARD_HTML.contains(marker), "missing {marker}");
    }
    assert!(!DASHBOARD_HTML.contains("fonts.googleapis.com"));
    assert!(!DASHBOARD_HTML.contains("fonts.gstatic.com"));
}

#[test]
fn dashboard_assets_reassemble_byte_stably() {
    use sha2::{Digest, Sha256};

    assert_eq!(DASHBOARD_HTML.len(), 118_849);
    assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_CSS}}"));
    assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_JS}}"));
    let digest = Sha256::digest(DASHBOARD_HTML.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        digest_hex,
        "dfe1806d8d93a812893a330200fad6458dfe696977be5b8062ff53fa4cade73b"
    );

    let crlf = |asset: &str| asset.replace("\r\n", "\n").replace('\n', "\r\n");
    assert_eq!(
        assemble_dashboard_html(
            &crlf(DASHBOARD_HTML_TEMPLATE),
            &crlf(DASHBOARD_CSS),
            &crlf(DASHBOARD_JS),
        ),
        *DASHBOARD_HTML,
    );
}

#[test]
fn dashboard_ships_runtime_editor_and_inline_retry_guidance() {
    for marker in [
        "function drawRuntimeSettings",
        "function bfSaveRuntimeSettings",
        "RuntimeConfigChanged",
        "function bfReviewRetrySubmit",
        "Stop the job before queuing a retry",
        "function bfReviewStopForRetry",
        "function showResumeKeyEntry",
        "function bfResumeWithKey",
    ] {
        assert!(DASHBOARD_HTML.contains(marker), "missing {marker}");
    }
    assert!(
        !DASHBOARD_HTML.contains("window.prompt"),
        "retry guidance must use the inline editor"
    );
}

#[test]
fn dashboard_posts_include_csrf_header() {
    assert!(DASHBOARD_HTML.contains(CSRF_TOKEN_PLACEHOLDER));
    assert!(DASHBOARD_HTML.contains("headers: { [CSRF_HEADER]: CSRF_TOKEN }"));
}

#[test]
fn csrf_token_is_hex_encoded_random_bytes() {
    let token = generate_csrf_token().expect("token should generate");
    assert_eq!(token.len(), 32);
    assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
}

// -----------------------------------------------------------------------
// Segment mutation endpoints: CSRF rejection + isolated-store end-to-end.
//
// These build a real completed job (real EPUB parsed with `read_epub`,
// real `Segment`s from `build_segments`, real store rows) in a per-test
// temp directory so `save_manual_translation` / `set_segment_flag` /
// `retry_segment_with_guidance` exercise the same code paths production
// traffic does. Every test gets its own `tempfile::TempDir` and its own
// `AppState::store_path` (via `test_state_with_store`), so nothing here
// touches the process's current directory and tests are parallel-safe.
// -----------------------------------------------------------------------

use bookforge_core::segment::BlockTranslation;
use bookforge_store::{CreateJob, SaveTranslation};
use std::io::Write as _;

const FIXTURE_CONTAINER_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#;

const FIXTURE_OPF: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="uid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="uid">serve-fixture</dc:identifier>
    <dc:title>Serve Fixture</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ch1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="ch2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="ch1"/>
    <itemref idref="ch2"/>
  </spine>
</package>"#;

const FIXTURE_CHAPTER_ONE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Fixture Chapter One</title></head>
<body>
<p>The lantern flickered in the old library.</p>
</body>
</html>"#;

const FIXTURE_CHAPTER_TWO: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>Fixture Chapter Two</title></head>
<body>
<p>Rain tapped steadily against the windowpane.</p>
</body>
</html>"#;

fn build_fixture_epub(path: &std::path::Path) {
    use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

    let file = std::fs::File::create(path).expect("fixture EPUB should be creatable");
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored).unwrap();
    zip.write_all(b"application/epub+zip").unwrap();
    zip.start_file("META-INF/container.xml", deflated).unwrap();
    zip.write_all(FIXTURE_CONTAINER_XML.as_bytes()).unwrap();
    zip.start_file("content.opf", deflated).unwrap();
    zip.write_all(FIXTURE_OPF.as_bytes()).unwrap();
    zip.start_file("chapter1.xhtml", deflated).unwrap();
    zip.write_all(FIXTURE_CHAPTER_ONE.as_bytes()).unwrap();
    zip.start_file("chapter2.xhtml", deflated).unwrap();
    zip.write_all(FIXTURE_CHAPTER_TWO.as_bytes()).unwrap();
    zip.finish().unwrap();
}

/// A completed two-segment job (one segment per chapter, one block per
/// segment) backed by an isolated temp-dir store and a real rebuildable
/// output path — everything the mutation endpoints under test touch.
struct MutationFixture {
    // Held only to keep the temp directory alive for the fixture's
    // lifetime; never read directly.
    _temp: tempfile::TempDir,
    store_path: PathBuf,
    output_path: PathBuf,
    job_id: String,
    segment_a: String,
    segment_b: String,
    csrf: String,
}

fn build_mutation_fixture() -> MutationFixture {
    build_mutation_fixture_for_provider("mock", "mock-identity", None)
}

fn build_mutation_fixture_for_provider(
    provider: &str,
    model: &str,
    api_key_env: Option<&str>,
) -> MutationFixture {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let input_path = temp.path().join("input.epub");
    build_fixture_epub(&input_path);
    let output_path = temp.path().join("output.epub");
    let store_path = temp.path().join("jobs.sqlite");

    let store = JobStore::open(&store_path).expect("store should open");
    let job = store
        .create_job(CreateJob {
            input: &input_path,
            output: &output_path,
            source_lang: Some("English"),
            target_lang: "Italian",
            provider,
            model,
            base_url: None,
            api_key_env,
            book_id: None,
            series_id: None,
        })
        .expect("job should be created");

    let book = bookforge_epub::read_epub(&input_path).expect("fixture EPUB should parse");
    let settings = bookforge_core::TranslationProfile::Balanced.resolve();
    let segments = bookforge_core::segment::build_segments(&book, &settings.segmentation)
        .expect("segments should build");
    // `read_epub`/`build_segments` also synthesize a segment for the OPF
    // `dc:title` metadata (its own section, ahead of the spine chapters),
    // so a two-chapter book yields three segments total. Only the two
    // chapter segments are used as `segment_a`/`segment_b` below; the
    // metadata segment is still inserted and translated like any other
    // so the fixture matches what a real job actually persists.
    let chapter_segments = segments
        .iter()
        .filter(|segment| segment.section_id.0 != "sec_metadata_opf")
        .collect::<Vec<_>>();
    assert_eq!(
        chapter_segments.len(),
        2,
        "fixture EPUB (one paragraph per chapter) should yield exactly two chapter segments"
    );

    store
        .insert_segments(&job.id, &segments, "v1", provider, model, "test_ns")
        .expect("segments should insert");

    for segment in &segments {
        let blocks = segment
            .source
            .blocks
            .iter()
            .map(|block| BlockTranslation {
                block_id: block.block_id.clone(),
                text: format!("[IT] {}", block.text),
            })
            .collect::<Vec<_>>();
        let translated_text = blocks
            .iter()
            .map(|block| block.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        store
            .save_translation(SaveTranslation {
                job_id: &job.id,
                segment_id: &segment.id.0,
                translated_text: &translated_text,
                blocks: &blocks,
                input_tokens: Some(10),
                input_cached_tokens: Some(0),
                output_tokens: Some(12),
                tokens_estimated: false,
                provider,
                model,
                prompt_version: "v1",
            })
            .expect("translation should save");
    }
    store
        .mark_job_complete(&job.id)
        .expect("job should complete");

    let snapshot = bookforge_core::RunConfigSnapshot {
        input_path: input_path.clone(),
        input_snapshot_path: Some(input_path.clone()),
        input_sha256: Some("test-sha".to_string()),
        output_path: output_path.clone(),
        events_path: None,
        report_json_path: None,
        report_markdown_path: None,
        source_language: Some("English".to_string()),
        target_language: "Italian".to_string(),
        creator: None,
        provider: provider.to_string(),
        model: model.to_string(),
        base_url: None,
        api_key_env: api_key_env.map(str::to_string),
        profile: settings.profile,
        provider_preset: None,
        prompt_version: "v1".to_string(),
        cache_namespace: "test_ns".to_string(),
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

    MutationFixture {
        store_path,
        output_path,
        job_id: job.id,
        segment_a: chapter_segments[0].id.0.clone(),
        segment_b: chapter_segments[1].id.0.clone(),
        csrf: "fixture-csrf-token".to_string(),
        _temp: temp,
    }
}

/// Sends `body` to `uri` with the given CSRF header value (or none), and
/// returns the response.
async fn post_json(
    router: &Router,
    uri: &str,
    csrf: Option<&str>,
    body: serde_json::Value,
) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", TEST_HOST)
        .header("content-type", "application/json");
    if let Some(token) = csrf {
        builder = builder.header(CSRF_HEADER, token);
    }
    router
        .clone()
        .oneshot(
            builder
                .body(Body::from(body.to_string()))
                .expect("request should build"),
        )
        .await
        .expect("route should respond")
}

/// GET with the default test session token; pass a deliberately different
/// token to assert authentication behavior.
async fn get_route(router: &Router, uri: &str) -> Response {
    get_route_with_token(router, uri, Some("token-123")).await
}

async fn get_route_with_token(router: &Router, uri: &str, token: Option<&str>) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder().uri(uri).header("host", TEST_HOST);
    if let Some(token) = token {
        builder = builder.header(CSRF_HEADER, token);
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request should build"))
        .await
        .expect("route should respond")
}

async fn response_json(response: Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body should read");
    serde_json::from_slice(&bytes).expect("response should be JSON")
}

fn make_stopped_fixture_resumable(fixture: &MutationFixture) {
    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    store
        .request_segment_retry(&fixture.job_id, &fixture.segment_b, None)
        .expect("completed segment should become retry-pending");
    store
        .mark_job_stopped(&fixture.job_id)
        .expect("job should become stopped");
}

fn clean_runtime_files(job_id: &str) {
    let _ = std::fs::remove_dir_all(bookforge_core::run_dir_for_job(job_id));
}

#[tokio::test]
async fn dashboard_reconfigure_is_typed_revisioned_and_csrf_protected() {
    let fixture = build_mutation_fixture();
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));
    let uri = format!("/api/jobs/{}/reconfigure", fixture.job_id);

    let initial = get_route_with_token(&router, &uri, Some(&fixture.csrf)).await;
    assert_eq!(initial.status(), StatusCode::OK);
    let initial = response_json(initial).await;
    assert_eq!(initial["revision"], 0);
    assert_eq!(initial["applied_revision"], 0);
    assert_eq!(initial["application_state"], "resume_required");
    assert_eq!(initial["lease"]["state"], "missing");
    assert_eq!(initial["identity"]["provider"], "mock");
    assert_eq!(initial["identity"]["model"], "mock-identity");
    assert_eq!(initial["editable"], true);

    let body = json!({ "concurrency": 2 });
    // H-5 folds the old CSRF gate into the global session-token check, so
    // both a missing and a wrong token are bare 401 rejections.
    let missing = post_json(&router, &uri, None, body.clone()).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !crate::commands::reconfigure::overrides_path_for_job(&fixture.job_id).exists(),
        "rejected mutations must not create a sidecar"
    );

    let unknown = post_json(
        &router,
        &uri,
        Some(&fixture.csrf),
        json!({ "model": "immutable-model" }),
    )
    .await;
    assert!(unknown.status().is_client_error());
    assert!(
        !crate::commands::reconfigure::overrides_path_for_job(&fixture.job_id).exists(),
        "unknown or immutable fields must not create a sidecar"
    );

    let first = post_json(
        &router,
        &uri,
        Some(&fixture.csrf),
        json!({
            "batch_max_output_tokens": 12000,
            "batch_max_items": 2,
            "batch_target_tokens": 3000,
            "concurrency": 2,
            "qa": "all",
            "double_check": "Formatting",
            "validate_output": true,
            "provider_max_attempts": 5,
            "adaptive_concurrency": false,
            "adaptive_batch_sizing": false
        }),
    )
    .await;
    assert_eq!(first.status(), StatusCode::OK);
    let first = response_json(first).await;
    assert_eq!(first["revision"], 1);
    assert_eq!(first["effective"]["concurrency"], 2);
    assert_eq!(first["effective"]["qa"], "all");
    assert_eq!(first["effective"]["double_check"], "Formatting");
    assert_eq!(first["effective"]["validate_output"], true);
    assert_eq!(first["application_state"], "resume_required");
    let fields = first["changed_fields"]
        .as_array()
        .expect("changed fields should be an array");
    assert!(fields.iter().any(|field| field == "concurrency"));
    assert!(fields.iter().any(|field| field == "qa"));
    let boundaries = first["next_boundary"]
        .as_array()
        .expect("boundaries should be an array");
    for boundary in ["next_request", "next_batch", "next_stage"] {
        assert!(boundaries.iter().any(|value| value == boundary));
    }

    let second = post_json(
        &router,
        &uri,
        Some(&fixture.csrf),
        json!({ "concurrency": 3 }),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second = response_json(second).await;
    assert_eq!(second["revision"], 2);
    assert_eq!(second["effective"]["concurrency"], 3);
    assert_eq!(second["effective"]["batch_max_items"], 2);

    let replayed =
        response_json(get_route_with_token(&router, &uri, Some(&fixture.csrf)).await).await;
    assert_eq!(replayed["revision"], 2);
    assert_eq!(replayed["effective"]["concurrency"], 3);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_controls_require_a_fresh_lease_and_signal_one_when_present() {
    let fixture = build_mutation_fixture();
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.runtime_lease_stale_after = Duration::from_millis(u64::MAX);
    let router = dashboard_router(state);

    for command in ["pause", "stop"] {
        let response = post_json(
            &router,
            &format!("/api/jobs/{}/{}", fixture.job_id, command),
            Some(&fixture.csrf),
            json!({}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let payload = response_json(response).await;
        assert!(
            payload["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no live worker")
        );
    }

    let lease = crate::control::RuntimeLease {
        schema_version: 1,
        instance_id: "dashboard-test-worker".to_string(),
        pid: std::process::id(),
        process_started_at_ms: bookforge_core::now_ms(),
        heartbeat_at_ms: bookforge_core::now_ms(),
        last_loaded_revision: 4,
        last_applied_revision: 4,
    };
    let runtime_path = crate::control::runtime_path_for_job(&fixture.job_id);
    std::fs::create_dir_all(runtime_path.parent().expect("runtime parent"))
        .expect("runtime directory should exist");
    std::fs::write(
        &runtime_path,
        serde_json::to_vec_pretty(&lease).expect("lease should serialize"),
    )
    .expect("lease should write");

    let view = response_json(
        get_route_with_token(
            &router,
            &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            Some(&fixture.csrf),
        )
        .await,
    )
    .await;
    assert_eq!(view["lease"]["state"], "fresh");
    assert_eq!(view["live"], true);
    assert_eq!(view["applied_revision"], 4);

    let resume = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(resume.status(), StatusCode::OK);
    let resume = response_json(resume).await;
    assert_eq!(resume["mode"], "signaled");
    assert_eq!(
        bookforge_core::read_control_file(&bookforge_core::control_path_for_job(&fixture.job_id))
            .expect("control file should read"),
        bookforge_core::ControlCommand::Resume
    );

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_uses_remembered_key_in_scrubbed_child_environment() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY_ENV: &str = "BOOKFORGE_SERVE_TEST_DEEPSEEK_KEY";
    const SESSION_KEY: &str = "dashboard-session-key";

    let fixture = build_mutation_fixture_for_provider("deepseek", "deepseek-chat", Some(KEY_ENV));
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);

    let launches = Arc::new(AtomicUsize::new(0));
    let environments = Arc::new(Mutex::new(Vec::new()));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    lock_keys(&state).expect("key store should lock").extend([
        ("deepseek".to_string(), SESSION_KEY.to_string()),
        (
            "openrouter".to_string(),
            "unrelated-session-key".to_string(),
        ),
    ]);
    state.resume_launches = Some(launches.clone());
    state.resume_child_environments = Some(environments.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = response_json(response).await;
    assert_eq!(response["mode"], "spawned");
    assert!(!response.to_string().contains(SESSION_KEY));
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    let environments = environments.lock().expect("environments should lock");
    let environment = environments
        .first()
        .expect("one resume environment should be captured");
    assert_eq!(
        environment
            .get(std::ffi::OsStr::new(KEY_ENV))
            .and_then(|value| value.as_deref()),
        Some(std::ffi::OsStr::new(SESSION_KEY))
    );
    for (_, unrelated_env) in PROVIDER_KEY_ENVS {
        assert!(
            !environment.contains_key(std::ffi::OsStr::new(unrelated_env)),
            "{unrelated_env} must not leak into the resume child"
        );
    }

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_without_required_key_returns_actionable_error() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY_ENV: &str = "BOOKFORGE_SERVE_TEST_MISSING_OPENROUTER_KEY_9C71";
    let fixture =
        build_mutation_fixture_for_provider("openrouter", "openrouter/auto", Some(KEY_ENV));
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);

    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = response_json(response).await;
    assert_eq!(response["requires_api_key"], true);
    assert_eq!(response["provider"], "openrouter");
    assert_eq!(response["api_key_env"], KEY_ENV);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or_default()
            .contains("supply it to resume")
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_accepts_and_remembers_replacement_key() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    const KEY_ENV: &str = "BOOKFORGE_SERVE_TEST_RESUPPLIED_OPENROUTER_KEY";
    const REPLACEMENT_KEY: &str = "replacement-dashboard-key";

    let fixture =
        build_mutation_fixture_for_provider("openrouter", "openrouter/auto", Some(KEY_ENV));
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);

    let launches = Arc::new(AtomicUsize::new(0));
    let environments = Arc::new(Mutex::new(Vec::new()));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    let keys = state.keys.clone();
    state.resume_launches = Some(launches.clone());
    state.resume_child_environments = Some(environments.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({ "api_key": REPLACEMENT_KEY }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response_json(response).await["mode"], "spawned");
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(
        keys.lock()
            .expect("key store should lock")
            .get("openrouter")
            .map(String::as_str),
        Some(REPLACEMENT_KEY)
    );
    let environments = environments.lock().expect("environments should lock");
    let environment = environments
        .first()
        .expect("one resume environment should be captured");
    assert_eq!(
        environment
            .get(std::ffi::OsStr::new(KEY_ENV))
            .and_then(|value| value.as_deref()),
        Some(std::ffi::OsStr::new(REPLACEMENT_KEY))
    );
    assert!(!environment.contains_key(std::ffi::OsStr::new("DEEPSEEK_API_KEY")));

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_missing_worker_resume_launches_exactly_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = build_mutation_fixture();
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);
    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);
    let uri = format!("/api/jobs/{}/resume", fixture.job_id);

    let (first, second) = tokio::join!(
        post_json(&router, &uri, Some(&fixture.csrf), json!({})),
        post_json(&router, &uri, Some(&fixture.csrf), json!({}))
    );
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let first = response_json(first).await;
    let second = response_json(second).await;
    let modes = [
        first["mode"].as_str().unwrap_or_default(),
        second["mode"].as_str().unwrap_or_default(),
    ];
    assert!(modes.contains(&"spawned"));
    assert!(modes.contains(&"launching"));
    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "the atomic launch claim must deduplicate concurrent Resume clicks"
    );

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_recognizes_finalize_only_work() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = build_mutation_fixture();
    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    assert!(
        store
            .resumable_segment_ids(&fixture.job_id)
            .expect("segment lookup should succeed")
            .is_empty(),
        "the fixture should have no translation work left"
    );
    store
        .mark_job_stopped(&fixture.job_id)
        .expect("job should become stopped during finalization");
    clean_runtime_files(&fixture.job_id);

    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let view = response_json(
        get_route_with_token(
            &router,
            &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            Some(&fixture.csrf),
        )
        .await,
    )
    .await;
    assert_eq!(view["resumable_work"], true);
    assert_eq!(view["editable"], true);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = response_json(response).await;
    assert_eq!(response["mode"], "spawned");
    assert_eq!(response["forced"], false);
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_forces_relaunch_of_a_dead_paused_worker() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = build_mutation_fixture();
    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    store
        .mark_job_paused(&fixture.job_id)
        .expect("job should become paused");
    clean_runtime_files(&fixture.job_id);

    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);
    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let response = response_json(response).await;
    assert_eq!(response["mode"], "spawned");
    assert_eq!(response["forced"], true);
    assert_eq!(launches.load(Ordering::SeqCst), 1);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_resume_rejects_a_completed_job_without_work() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let fixture = build_mutation_fixture();
    clean_runtime_files(&fixture.job_id);
    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_store(&fixture.csrf, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let view = response_json(
        get_route_with_token(
            &router,
            &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            Some(&fixture.csrf),
        )
        .await,
    )
    .await;
    assert_eq!(view["resumable_work"], false);
    assert_eq!(view["editable"], false);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.csrf),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(response).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("no resumable work")
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn correction_locks_serialize_one_job_and_not_two() {
    let state = test_state("token-123");

    let a1 = job_correction_lock(&state, "job-a").expect("lock should resolve");
    let a2 = job_correction_lock(&state, "job-a").expect("lock should resolve");
    let b = job_correction_lock(&state, "job-b").expect("lock should resolve");
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "the same job must contend on one lock"
    );
    assert!(
        !Arc::ptr_eq(&a1, &b),
        "different books must not block each other"
    );

    // Two corrections to the same job must not overlap. Without the lock
    // both would read the same pre-correction snapshot and the later
    // rename would publish an EPUB missing the earlier edit.
    let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));
    let first = {
        let lock = a1.clone();
        let order = order.clone();
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            order.lock().unwrap().push("first-enter");
            tokio::time::sleep(Duration::from_millis(50)).await;
            order.lock().unwrap().push("first-exit");
        })
    };
    tokio::time::sleep(Duration::from_millis(10)).await;
    let second = {
        let lock = a2.clone();
        let order = order.clone();
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            order.lock().unwrap().push("second-enter");
        })
    };
    first.await.expect("first correction should finish");
    second.await.expect("second correction should finish");

    assert_eq!(
        *order.lock().unwrap(),
        vec!["first-enter", "first-exit", "second-enter"],
        "the second correction must wait for the first to finish"
    );
}

#[tokio::test]
async fn save_manual_translation_rejects_missing_or_wrong_csrf_without_mutating_store() {
    let fixture = build_mutation_fixture();
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));
    let uri = format!(
        "/api/jobs/{}/segments/{}/translation",
        fixture.job_id, fixture.segment_a
    );
    let body = json!({ "blocks": [{ "block_id": "whatever", "text": "corrupted" }] });

    let missing = post_json(&router, &uri, None, body.clone()).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    assert!(
        !store
            .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
            .expect("lookup should succeed"),
        "a rejected request must not human-correct the segment"
    );
}

#[tokio::test]
async fn set_segment_flag_rejects_missing_or_wrong_csrf_without_mutating_store() {
    let fixture = build_mutation_fixture();
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));
    let uri = format!(
        "/api/jobs/{}/segments/{}/flag",
        fixture.job_id, fixture.segment_b
    );
    let body = json!({ "flagged": true });

    let missing = post_json(&router, &uri, None, body.clone()).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    let flagged = store
        .dashboard_flagged_segment_ids(&fixture.job_id)
        .expect("flags should load");
    assert!(
        !flagged.contains(&fixture.segment_b),
        "a rejected request must not persist a flag"
    );
}

#[tokio::test]
async fn retry_segment_rejects_missing_or_wrong_csrf_without_mutating_store() {
    let fixture = build_mutation_fixture();
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));
    let uri = format!(
        "/api/jobs/{}/segments/{}/retry",
        fixture.job_id, fixture.segment_b
    );
    let body = json!({ "guidance": "please redo" });

    let missing = post_json(&router, &uri, None, body.clone()).await;
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

    let wrong = post_json(&router, &uri, Some("wrong-token"), body).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    let guidance = store
        .load_retry_guidance(&fixture.job_id)
        .expect("guidance should load");
    assert!(
        !guidance.contains_key(&fixture.segment_b),
        "a rejected request must not persist retry guidance"
    );
    let records = store
        .segment_records(&fixture.job_id)
        .expect("records should load");
    let segment_b = records
        .iter()
        .find(|record| record.id == fixture.segment_b)
        .expect("segment_b should exist");
    assert_eq!(
        segment_b.status, "succeeded",
        "a rejected retry must not move the segment to retry_pending"
    );
}

#[tokio::test]
async fn dashboard_review_and_mutation_endpoints_end_to_end() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let fixture = build_mutation_fixture();
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));

    // 1. Review data: per-block source/target text is present for both segments.
    let review_uri = format!("/api/jobs/{}/review", fixture.job_id);
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&review_uri)
                .header("host", TEST_HOST)
                .header(CSRF_HEADER, &fixture.csrf)
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should read");
    let review: serde_json::Value = serde_json::from_slice(&bytes).expect("review should be json");
    let segments = review["segments"].as_array().expect("segments array");
    // Two chapter segments plus the synthesized OPF `dc:title` metadata
    // segment (see `build_mutation_fixture`).
    assert_eq!(segments.len(), 3);
    for segment in segments {
        let blocks = segment["blocks"].as_array().expect("blocks array");
        assert!(!blocks.is_empty(), "each segment should have blocks");
        for block in blocks {
            assert!(
                !block["target_text"].as_str().unwrap_or_default().is_empty(),
                "each block should carry non-empty target text"
            );
        }
    }

    // 2. Save a corrected translation for segment_a.
    let segment_a_json = segments
        .iter()
        .find(|segment| segment["segment_id"] == fixture.segment_a)
        .expect("segment_a should appear in the review");
    let block_ids = segment_a_json["blocks"]
        .as_array()
        .expect("blocks array")
        .iter()
        .map(|block| {
            block["block_id"]
                .as_str()
                .expect("block_id should be a string")
                .to_string()
        })
        .collect::<Vec<_>>();
    let correction_body = json!({
        "blocks": block_ids
            .iter()
            .map(|id| json!({ "block_id": id, "text": "Corrected by reviewer." }))
            .collect::<Vec<_>>(),
    });
    let translation_uri = format!(
        "/api/jobs/{}/segments/{}/translation",
        fixture.job_id, fixture.segment_a
    );
    let response = post_json(
        &router,
        &translation_uri,
        Some(&fixture.csrf),
        correction_body,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let store = JobStore::open(&fixture.store_path).expect("store should reopen");
    assert!(
        store
            .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
            .expect("lookup should succeed"),
        "segment_a should be marked human_corrected"
    );
    assert!(
        fixture.output_path.exists(),
        "the rebuilt output EPUB should exist after the correction"
    );

    // 3. Flag, then clear, segment_b.
    let flag_uri = format!(
        "/api/jobs/{}/segments/{}/flag",
        fixture.job_id, fixture.segment_b
    );
    for flagged in [true, false] {
        let response = post_json(
            &router,
            &flag_uri,
            Some(&fixture.csrf),
            json!({ "flagged": flagged }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let flagged_ids = store
            .dashboard_flagged_segment_ids(&fixture.job_id)
            .expect("flags should load");
        assert_eq!(flagged_ids.contains(&fixture.segment_b), flagged);
    }

    // 4. Request a retry with guidance for segment_b.
    let retry_uri = format!(
        "/api/jobs/{}/segments/{}/retry",
        fixture.job_id, fixture.segment_b
    );
    let response = post_json(
        &router,
        &retry_uri,
        Some(&fixture.csrf),
        json!({ "guidance": "Please redo more literally." }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let guidance = store
        .load_retry_guidance(&fixture.job_id)
        .expect("guidance should load");
    assert_eq!(
        guidance.get(&fixture.segment_b).map(String::as_str),
        Some("Please redo more literally.")
    );
    let records = store
        .segment_records(&fixture.job_id)
        .expect("records should load");
    let segment_b_record = records
        .iter()
        .find(|record| record.id == fixture.segment_b)
        .expect("segment_b should exist");
    assert_eq!(segment_b_record.status, "retry_pending");

    // 5. A retry request against the now-frozen (human-corrected) segment_a
    //    is rejected, and does not disturb its correction.
    let retry_a_uri = format!(
        "/api/jobs/{}/segments/{}/retry",
        fixture.job_id, fixture.segment_a
    );
    let response = post_json(
        &router,
        &retry_a_uri,
        Some(&fixture.csrf),
        json!({ "guidance": "try again" }),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body should read");
    let payload: serde_json::Value =
        serde_json::from_slice(&bytes).expect("error body should be json");
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("frozen human correction"),
        "rejection should explain the segment is frozen: {payload}"
    );
    assert!(
        store
            .translation_is_human_corrected(&fixture.job_id, &fixture.segment_a)
            .expect("lookup should succeed"),
        "the rejected retry must not un-freeze the human correction"
    );
}

// -----------------------------------------------------------------------
// H-5 / SERVE-1: session-token authentication on every route
// -----------------------------------------------------------------------

#[tokio::test]
async fn auth_on_requires_session_tokens_on_representative_api_routes() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let router = dashboard_router(test_state("sekrit-token"));

    // Missing tokens: 401 on reads AND mutations, including the two heavy
    // protectees (review documents, job listing) and options metadata.
    for uri in [
        "/api/jobs",
        "/api/jobs/some-job/review",
        "/api/jobs/some-job",
        "/api/options",
        "/api/providers",
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("host", TEST_HOST)
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{uri} must reject an anonymous request"
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body should read");
        let payload: serde_json::Value = serde_json::from_slice(&body).expect("401 should be JSON");
        assert!(
            !payload.to_string().contains("sekrit-token"),
            "the 401 must not echo any credential material"
        );
    }

    // Wrong tokens are also bare 401s with no detail about which half failed.
    let wrong = get_route_with_token(&router, "/api/jobs", Some("not-the-token")).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // The right token passes through to the handler (options needs no store).
    let good = get_route_with_token(&router, "/api/options", Some("sekrit-token")).await;
    assert_eq!(good.status(), StatusCode::OK);
}

#[tokio::test]
async fn root_exchange_bootstraps_browsers_without_leaking_the_token() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let secret = "feedface-feedface-feedface";
    let router = dashboard_router(test_state(secret));

    // Anonymous GET / gets guidance only.
    let anonymous = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("host", TEST_HOST)
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(anonymous.status(), StatusCode::OK);
    let body = axum::body::to_bytes(anonymous.into_body(), usize::MAX)
        .await
        .expect("login body should read");
    let login_page = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(
        login_page.contains("bookforge serve"),
        "points at the console link"
    );
    assert!(!login_page.contains(secret));
    assert!(!login_page.contains(CSRF_TOKEN_PLACEHOLDER));

    // Wrong ?token= is rejected without echoing the expected value.
    let rejected = get_route_with_token(&router, "/?token=deadbeef", None).await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    // Right ?token= gets the bootstrap page that seeds sessionStorage under
    // the same header name every API fetch sends, mirroring the old CSRF
    // wiring, then redirects to the clean root.
    let bootstrapped = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(bootstrapped.status(), StatusCode::OK);
    let body = axum::body::to_bytes(bootstrapped.into_body(), usize::MAX)
        .await
        .expect("bootstrap body should read");
    let bootstrap = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(bootstrap.contains(CSRF_HEADER));
    assert!(bootstrap.contains(&format!(
        "sessionStorage.setItem(\"{CSRF_HEADER}\", \"{secret}\")"
    )));
    assert!(bootstrap.contains("location.replace(\"/\")"));
    assert!(!bootstrap.is_empty(), "bootstrap page served");

    // A caller already holding the header may load the dashboard directly.
    let direct = get_route_with_token(&router, "/", Some(secret)).await;
    assert_eq!(direct.status(), StatusCode::OK);
    let body = axum::body::to_bytes(direct.into_body(), usize::MAX)
        .await
        .expect("dashboard body should read");
    let page = String::from_utf8(body.to_vec()).expect("dashboard is utf-8");
    assert!(page.contains(&format!(
        "const CSRF_TOKEN = sessionStorage.getItem(CSRF_HEADER) || \"{secret}\""
    )));
}

#[tokio::test]
async fn hardened_headers_stamp_every_response_not_just_the_index() {
    for status_response in [
        get_route_with_token(
            &dashboard_router(test_state("token-123")),
            "/api/options",
            Some("token-123"),
        )
        .await,
        get_route_with_token(
            &dashboard_router(test_state("token-123")),
            "/api/jobs",
            None,
        )
        .await,
    ] {
        for (name, value) in [
            ("content-security-policy", DASHBOARD_CONTENT_SECURITY_POLICY),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "no-referrer"),
            ("cache-control", "no-store"),
            ("x-frame-options", "DENY"),
        ] {
            assert_eq!(
                status_response.headers().get(name),
                Some(&HeaderValue::from_static(value)),
                "{name} missing on a {} response",
                status_response.status()
            );
        }
    }
}

#[tokio::test]
async fn no_auth_restores_the_previous_unauthenticated_behavior() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let mut state = test_state_with_store("quiet-token", temp.path().join("jobs.sqlite"));
    state.auth_enabled = false;
    let router = dashboard_router(state);

    // Reads reach handlers without any session credential...
    let jobs = get_route_with_token(&router, "/api/jobs", None).await;
    assert_eq!(jobs.status(), StatusCode::OK);
    // ...and / once again serves the full dashboard with its embedded CSRF
    // token (still CSRF-guarded per mutation by the legacy checks).
    let index = get_route_with_token(&router, "/", None).await;
    assert_eq!(index.status(), StatusCode::OK);
    let body = axum::body::to_bytes(index.into_body(), usize::MAX)
        .await
        .expect("index body should read");
    let page = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(
        page.contains("quiet-token"),
        "--no-auth embeds the CSRF token again"
    );
    // Mutations keep the legacy cross-site/CSRF rejection shape (403).
    let retry = post_json(&router, "/api/jobs/not-real/retry", None, json!({})).await;
    assert_eq!(retry.status(), StatusCode::FORBIDDEN);
}

#[test]
fn valid_job_id_mirrors_audiobook_id_strictness() {
    for valid in ["job_1750000000000000000_deadbeef1234", "a", "x-9_Z"] {
        assert!(valid_job_id(valid), "rejected {valid:?}");
    }
    for invalid in [
        "",
        "../escape",
        "..",
        "with/slash",
        "with\\backslash",
        "with.dot",
        "with space",
        "%2e%2e",
        &"x".repeat(161),
    ] {
        assert!(!valid_job_id(invalid), "accepted {invalid:?}");
    }
}

#[tokio::test]
async fn traversal_job_ids_are_refused_before_touching_the_filesystem() {
    let fixture = build_mutation_fixture();
    let router = dashboard_router(test_state_with_store(
        &fixture.csrf,
        fixture.store_path.clone(),
    ));

    // Axum percent-decodes path params, so these land in the handler as
    // traversal payloads ("../..", "with/slash"); both must die at the
    // validation boundary with a client error — never a store/fs read that
    // could fold unrelated JSONL into the response.
    for encoded in ["..%2F..", "a%2Fhidden-run", "%2E%2E%2Fescapes"] {
        let response = get_route_with_token(
            &router,
            &format!("/api/jobs/{encoded}/review"),
            Some(&fixture.csrf),
        )
        .await;
        assert!(
            response.status().is_client_error(),
            "traversal id {encoded} returned {}",
            response.status()
        );
        assert_ne!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "traversal id {encoded} must be a deliberate rejection"
        );
    }
}

// -----------------------------------------------------------------------
// SERVE-3: cancel verifies PID liveness + BookForge ownership before kill
// -----------------------------------------------------------------------

#[tokio::test]
async fn audiobook_cancel_refuses_pid_that_cannot_be_verified_as_bookforge() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let pid_slot = u32::MAX - 11; // effectively never a live BookForge process
    let out_dir = write_test_audiobook_operation(
        temp.path(),
        "unverifiable",
        json!({"status": "running", "chunks": []}),
        json!({
            "status": "running",
            "pid": pid_slot,
            "updated_at_ms": 10,
        }),
    );
    let cancelled = Arc::new(Mutex::new(Vec::new()));
    let mut state = test_state_with_upload_dir("token-123", temp.path().to_path_buf());
    state.audio_restart_cancels = Some(cancelled.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        "/api/audiobooks/unverifiable/cancel",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
    let payload = response_json(response).await;
    assert!(
        payload["error"]
            .as_str()
            .unwrap_or_default()
            .contains("nothing was signalled"),
        "refusal explains itself: {payload}"
    );
    assert!(cancelled.lock().unwrap().is_empty(), "no signal may fire");
    let process: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("process.json")).expect("process state remains"),
    )
    .expect("process state is JSON");
    assert_eq!(process["status"], "running", "state stays untouched");
}

#[test]
fn liveness_identity_check_accepts_our_own_live_process() {
    // The test binary IS the current bookforge executable, so the recorded
    // self-pid is exactly what a genuine restart scenario would produce.
    assert!(live_process_is_bookforge(std::process::id()));
}

// -----------------------------------------------------------------------
// SERVE-6: launch slot cap in AppState
// -----------------------------------------------------------------------

#[test]
fn launch_slots_block_at_the_cap_and_release_on_drop() {
    let state = test_state("token-123");
    let mut guards = Vec::new();
    for _ in 0..MAX_CONCURRENT_DASHBOARD_LAUNCHES {
        match try_acquire_launch_slot(&state).expect("registry locks") {
            LaunchSlot::Acquired(guard) => guards.push(guard),
            LaunchSlot::Exhausted => panic!("cap reached too early"),
        }
    }
    assert!(
        matches!(
            try_acquire_launch_slot(&state).expect("registry locks"),
            LaunchSlot::Exhausted
        ),
        "fifth concurrent launch must be refused"
    );

    drop(guards.pop());
    assert!(
        matches!(
            try_acquire_launch_slot(&state).expect("registry locks"),
            LaunchSlot::Acquired(_)
        ),
        "a released slot becomes acquirable"
    );
    drop(guards);
    let held = *state.launch_slots.lock().unwrap();
    assert_eq!(held, 0, "dropping every guard empties the registry");
}

// -----------------------------------------------------------------------
// Quality: idle correction-lock entries are evicted
// -----------------------------------------------------------------------

#[tokio::test]
async fn correction_lock_registry_evicts_idle_entries_only() {
    let state = test_state("token-123");

    // Acquiring creates the entry; eviction while uncontended removes it.
    let first = job_correction_lock(&state, "job-idle").expect("lock resolves");
    drop(first);
    evict_idle_correction_lock(&state, "job-idle");
    assert!(state.correction_locks.lock().unwrap().is_empty());

    // A contended lock is never evicted (another correction is mid-flight),
    // but once the holder finishes it becomes evictable again.
    let held = job_correction_lock(&state, "job-busy").expect("lock resolves");
    let guard_task = tokio::spawn({
        let held = held.clone();
        async move {
            let _guard = held.lock().await;
            // Keep the mutex locked long enough for the evict attempt below.
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    tokio::time::sleep(Duration::from_millis(10)).await;
    evict_idle_correction_lock(&state, "job-busy");
    assert_eq!(
        state.correction_locks.lock().unwrap().len(),
        1,
        "busy locks stay registered"
    );
    guard_task.await.expect("holder task joins");
    evict_idle_correction_lock(&state, "job-busy");
    assert!(state.correction_locks.lock().unwrap().is_empty());
}

// -----------------------------------------------------------------------
// H-6 / SERVE-2: private directories and files at the serve entry points
// -----------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn private_dirs_are_created_and_loose_ancestors_tightened() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("temp dir should be created");
    let base = root.path().join(".bookforge");
    let target = base.join("serve-uploads");

    // Pre-existing, previously-loose components get tightened in place
    // (the exact H-6 regression: an older release's world-readable root).
    std::fs::create_dir_all(&base).expect("base should exist");
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755))
        .expect("permissions should apply");
    ensure_private_dir_under(root.path(), &target).expect("dir should be created");

    for dir in [base.clone(), target] {
        let mode = std::fs::metadata(&dir)
            .expect("metadata should read")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{} must be 0700", dir.display());
    }
}

#[cfg(unix)]
#[test]
fn write_private_file_yields_owner_only_permissions_even_over_stale_files() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().expect("temp dir should be created");
    let path = root.path().join("upload.epub");
    // Stale file from an older release with everyone-readable bits.
    std::fs::write(&path, b"old").expect("stale file should be written");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
        .expect("permissions should apply");

    write_private_file(&path, b"private book contents").expect("write should succeed");

    assert_eq!(
        std::fs::read(&path).expect("read back"),
        b"private book contents"
    );
    let mode = std::fs::metadata(&path)
        .expect("metadata should read")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[tokio::test]
async fn estimate_endpoint_parses_upload_from_a_private_temp_dir_end_to_end() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    // A real EPUB through a real multipart upload and parser: exercises the
    // per-request temp-dir path that replaced shared predictable names, with
    // no provider network access (mock).
    let upload_dir = tempfile::tempdir().expect("temp dir should be created");
    let epub = tempfile::tempdir().expect("fixture dir should be created");
    let epub_path = epub.path().join("fixture.epub");
    build_fixture_epub(&epub_path);
    let bytes = std::fs::read(&epub_path).expect("fixture EPUB should read");

    let boundary = "B";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.epub\"\r\nContent-Type: application/epub+zip\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"provider\"\r\n\r\nmock\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Disposition: form-data; name=\"target\"\r\n\r\nItalian\r\n--{boundary}--\r\n")
            .as_bytes(),
    );

    let response = dashboard_router(test_state_with_upload_dir(
        "token-123",
        upload_dir.path().to_path_buf(),
    ))
    .oneshot(
        Request::builder()
            .method("POST")
            .uri("/api/estimate")
            .header("host", TEST_HOST)
            .header(CSRF_HEADER, "token-123")
            .header("content-type", "multipart/form-data; boundary=B")
            .body(Body::from(body))
            .expect("request should build"),
    )
    .await
    .expect("route should respond");
    assert_eq!(response.status(), StatusCode::OK);
    let payload = response_json(response).await;
    // Three segments (two chapters + OPF title) parsed from the upload that
    // only ever lived inside the per-request private temp directory.
    assert_eq!(payload["segments"], json!(3), "payload: {payload}");
    assert_eq!(payload["model"], json!("mock-prefix-target"));
    // 36 under the canonical chars/4-style estimator (was 33 under the
    // retired 4.5-chars/token dominant-class formula).
    assert_eq!(payload["input_tokens"], json!(36));
}
