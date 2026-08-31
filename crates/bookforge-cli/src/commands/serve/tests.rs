use super::*;
use super::{assets::*, audio::*, glossary::*, jobs::*, options::*, security::*, translation::*};
use axum::http::HeaderValue;

const TEST_HOST: &str = "127.0.0.1:8765";
const TEST_DEADLOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// An auth-on test state that has pre-established a session with the given id
/// (so router tests authenticate by sending that value as the session cookie,
/// without waiting for a real bootstrap exchange).
fn test_state(session: &str) -> AppState {
    test_state_opt(session, true)
}

fn test_state_opt(session: &str, auth_enabled: bool) -> AppState {
    let auth = AuthState::new(auth_enabled).expect("auth state should build");
    auth.seed_session(session);
    AppState {
        refresh: Duration::from_millis(250),
        auth: Arc::new(auth),
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
        retry_launches: None,
        audio_restart_cancels: None,
    }
}

/// Like [`test_state`], but pointed at an isolated store path instead of
/// the process-relative default — lets tests exercise the store-backed
/// mutation endpoints against a temp-dir database without chdir'ing the
/// (shared, per-process) current directory, which would race across
/// parallel test threads.
fn test_state_with_store(session: &str, store_path: PathBuf) -> AppState {
    AppState {
        store_path,
        ..test_state(session)
    }
}

fn test_state_with_upload_dir(session: &str, upload_dir: PathBuf) -> AppState {
    AppState {
        upload_dir,
        ..test_state(session)
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
fn mutating_routes_require_an_authenticated_session() {
    let state = test_state("token-123");
    let headers = HeaderMap::new();
    assert!(reject_mutation(&headers, &state).is_some());

    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        HeaderValue::from_static("bookforge_session=token-123"),
    );
    assert!(reject_mutation(&headers, &state).is_none());
}

#[test]
fn mutating_routes_reject_cross_site_browser_requests() {
    let state = test_state("token-123");
    let mut headers = HeaderMap::new();
    headers.insert(
        "cookie",
        HeaderValue::from_static("bookforge_session=token-123"),
    );
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
            .header("cookie", session_cookie_value("token-123"))
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
                .header("cookie", session_cookie_value("token-123"))
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
                .header("cookie", session_cookie_value("token-123"))
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
                .header("cookie", session_cookie_value("token-123"))
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
                .header("cookie", session_cookie_value("token-123"))
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

/// Audit Feature-asymmetry closers: style-sheet + entity store management,
/// and the audiobook flags that previously had no dashboard surface
/// (chapters subset, text normalization, timeout, prune with a preview step,
/// retry-failed relaunch). Every mutating call keeps the session-CSRF header
/// convention.
#[test]
fn dashboard_ships_store_curation_screens_and_audio_parity_controls() {
    for marker in [
        "case \"styles\": return renderStyles(stage)",
        "case \"entities\": return renderEntities(stage)",
        "function renderStyles",
        "async function loadStyles",
        "async function bfStyleAdd",
        "async function bfStyleSave",
        "async function bfStyleRemove",
        "function renderEntities",
        "async function loadEntities",
        "async function bfEntityAdd",
        "async function bfEntitySave",
        "async function bfEntityRemove",
        "\"/api/styles\"",
        "/api/styles/",
        "\"/api/entities\"",
        "/api/entities/",
        // Audiobook parity remainder.
        "id=\"a_chapters\"",
        "id=\"a_text_normalization\"",
        "id=\"a_timeout\"",
        "fd.append(\"chapters\"",
        "fd.append(\"text_normalization\"",
        "timeout_seconds",
        "bfAudiobookRetryFailed",
        "/api/audiobooks/${encodeURIComponent(id)}/retry-failed",
        "bfPrunePreview",
        "/prune-preview",
        "/prune",
        "restricted",
    ] {
        assert!(DASHBOARD_HTML.contains(marker), "missing {marker}");
    }
}

#[test]
fn dashboard_assets_reassemble_byte_stably() {
    use sha2::{Digest, Sha256};

    assert_eq!(DASHBOARD_HTML.len(), 139_947);
    assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_CSS}}"));
    assert!(!DASHBOARD_HTML.contains("{{BOOKFORGE_DASHBOARD_JS}}"));
    let digest = Sha256::digest(DASHBOARD_HTML.as_bytes());
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    assert_eq!(
        digest_hex,
        "b7acaffc1faebe59b8838d69c41a180875c29a289368f068303e6e9cf4c66c8d"
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
fn dashboard_js_never_reads_or_injects_a_session_credential() {
    // Auth is a same-origin HttpOnly cookie: the browser JS must hold no
    // secret — no header literal, no sessionStorage, no placeholder.
    for forbidden_marker in ["sessionStorage", "x-bookforge-csrf", "__BOOKFORGE_"] {
        assert!(
            !DASHBOARD_JS.contains(forbidden_marker),
            "dashboard.js contains a client-readable credential marker"
        );
    }
    // Sign-out plumbing is present so the session can be ended from the UI.
    assert!(DASHBOARD_JS.contains("async function bfSignOut"));
    assert!(DASHBOARD_HTML.contains("onclick=\"bfSignOut()\""));
}

/// Fetch-seam audit: the browser authenticates purely via the cookie, and
/// every network call still routes through the single seam:
///
/// 1. exactly one raw `fetch(` may exist, and only between the
///    `BFAPISEAM-BEGIN`/`BFAPISEAM-END` markers where `bfFetch` lives;
/// 2. every screen's endpoints are asserted to reach the server through
///    `apiGet`/`apiSend`, covering Library, Progress polling, Review,
///    Glossary, Styles/Entities lists, Audiobook wizard/voices/status/
///    artifact hydration and the provider/options metadata bootstraps.
#[test]
fn dashboard_fetches_all_route_through_the_cookie_api_seam() {
    const SEAM_BEGIN: &str = "BFAPISEAM-BEGIN";
    const SEAM_END: &str = "BFAPISEAM-END";

    let js = DASHBOARD_JS;
    let total_fetch_calls = count_fetch_calls(js);
    let outside_seam = match (js.find(SEAM_BEGIN), js.find(SEAM_END)) {
        (Some(begin), Some(end)) if begin < end => {
            count_fetch_calls(&js[..begin]) + count_fetch_calls(&js[end..])
        }
        _ => panic!("dashboard.js must carry unique {SEAM_BEGIN}/{SEAM_END} markers"),
    };
    assert_eq!(
        outside_seam, 0,
        "every raw fetch( must live between {SEAM_BEGIN}/{SEAM_END} \
         and go through bfFetch/apiGet/apiSend"
    );
    assert!(
        total_fetch_calls >= 1,
        "the seam itself should perform the transport"
    );

    for marker in [
        // The seam is a thin pass-through now: the HttpOnly session cookie
        // rides along on the same-origin request automatically.
        "function bfFetch(path, options)",
        "async function apiGet(",
        "async function apiSend(",
        // Sign-out reuses the seam.
        "await apiSend(\"/api/auth/logout\", { method: \"POST\" })",
        // Library: job list + audiobook list poll through the seam.
        "Promise.all([apiGet(\"/api/jobs\"), apiGet(\"/api/audiobooks\")])",
        // Narrate-from-library job hydration.
        "await apiGet(\"/api/jobs/\" + encodeURIComponent(id))",
        // Progress polling: job + runtime settings refresh + SSE stream.
        "apiGet(\"/api/jobs/\" + encodeURIComponent(id) + \"/reconfigure\")",
        "apiGet(\"/api/jobs/\" + encodeURIComponent(state.id) + \"/events\"",
        "await apiGet(`/api/jobs/${encodeURIComponent(id)}/reconfigure`)",
        // Review: document load + post-save reload.
        "apiGet(\"/api/jobs/\" + encodeURIComponent(id) + \"/review\")",
        "apiGet(\"/api/jobs/\" + encodeURIComponent(App.selected) + \"/review\")",
        // Glossary list.
        "apiGet(\"/api/glossary\" + q)",
        // Styles + Entities lists (new store-curation screens).
        "apiGet(\"/api/styles\")",
        "apiGet(`/api/styles/${id}`)",
        "apiGet(\"/api/entities\")",
        // Audiobook wizard: estimate, voices, status polling, artifact
        // playback/download hydration, and the launch handshake.
        "apiSend(\"/api/audiobook/estimate\", {method:\"POST\", body:fd})",
        "apiGet(\"/api/audio/voices?provider=elevenlabs\")",
        "apiSend(\"/api/audiobook\", {method:\"POST\", body:fd})",
        "apiGet(`/api/audiobooks/${encodeURIComponent(id)}`)",
        "apiGet(`/api/audiobooks/${encodeURIComponent(id)}/artifact?disposition=inline`)",
        "apiGet(`/api/audiobooks/${encodeURIComponent(id)}/artifact`)",
        // Maintenance + control mutations reuse the same seam.
        "apiGet(`/api/audiobooks/${encodeURIComponent(id)}/prune-preview`)",
        "apiSend(`/api/audiobooks/${encodeURIComponent(id)}/prune`",
        "apiSend(`/api/audiobooks/${encodeURIComponent(id)}/retry-failed`",
        "apiSend(`/api/audiobooks/${encodeURIComponent(id)}/cancel`",
        // Bootstraps: options + provider key status.
        "apiGet(\"/api/options\")",
        "apiGet(\"/api/providers\")",
    ] {
        assert!(js.contains(marker), "missing api-seam usage: {marker}");
    }

    // Mutations with bodies keep their explicit content-type; nothing else is
    // added by the seam.
    for marker in [
        "\"content-type\": \"application/json\"",
        "{ method: \"DELETE\" }",
    ] {
        assert!(js.contains(marker), "missing {marker}");
    }
}

/// Count textual `fetch(` occurrences with a real word boundary (so template
/// helpers like `prefetch(` can never hide from the audit).
fn count_fetch_calls(source: &str) -> usize {
    let mut count = 0;
    let mut offset = 0;
    while let Some(found) = source[offset..].find("fetch(") {
        let absolute = offset + found;
        let boundary_ok = absolute == 0
            || !source[..absolute]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        if boundary_ok {
            count += 1;
        }
        offset = absolute + "fetch(".len();
    }
    count
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
use bookforge_store::{CreateJob, NewEntity, NewStyleSheet, SaveTranslation};
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
    session: String,
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
        session: "fixture-session-id".to_string(),
        _temp: temp,
    }
}

/// Sends `body` to `uri` with the given session cookie value (or none), and
/// returns the response.
async fn post_json(
    router: &Router,
    uri: &str,
    session: Option<&str>,
    body: serde_json::Value,
) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("host", TEST_HOST)
        .header("content-type", "application/json");
    if let Some(session) = session {
        builder = builder.header("cookie", session_cookie_value(session));
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

/// PUT variant of [`post_json`] for the store-curation update routes.
async fn axum_put_json(
    router: &Router,
    uri: &str,
    session: Option<&str>,
    body: serde_json::Value,
) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder()
        .method("PUT")
        .uri(uri)
        .header("host", TEST_HOST)
        .header("content-type", "application/json");
    if let Some(session) = session {
        builder = builder.header("cookie", session_cookie_value(session));
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

/// DELETE with the given session cookie value (or none).
async fn axum_delete(router: &Router, uri: &str, session: Option<&str>) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("host", TEST_HOST);
    if let Some(session) = session {
        builder = builder.header("cookie", session_cookie_value(session));
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request should build"))
        .await
        .expect("route should respond")
}

async fn get_route(router: &Router, uri: &str) -> Response {
    get_route_with_session(router, uri, Some("token-123")).await
}

async fn get_route_with_token(router: &Router, uri: &str, token: Option<&str>) -> Response {
    get_route_with_session(router, uri, token).await
}

async fn get_route_with_session(router: &Router, uri: &str, session: Option<&str>) -> Response {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let mut builder = Request::builder().uri(uri).header("host", TEST_HOST);
    if let Some(session) = session {
        builder = builder.header("cookie", session_cookie_value(session));
    }
    router
        .clone()
        .oneshot(builder.body(Body::empty()).expect("request should build"))
        .await
        .expect("route should respond")
}

/// A `Cookie` header value carrying the given session id.
fn session_cookie_value(session: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{SESSION_COOKIE_NAME}={session}"))
        .expect("session cookie header should parse")
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
        &fixture.session,
        fixture.store_path.clone(),
    ));
    let uri = format!("/api/jobs/{}/reconfigure", fixture.job_id);

    let initial = get_route_with_token(&router, &uri, Some(&fixture.session)).await;
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
        Some(&fixture.session),
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
        Some(&fixture.session),
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
        Some(&fixture.session),
        json!({ "concurrency": 3 }),
    )
    .await;
    assert_eq!(second.status(), StatusCode::OK);
    let second = response_json(second).await;
    assert_eq!(second["revision"], 2);
    assert_eq!(second["effective"]["concurrency"], 3);
    assert_eq!(second["effective"]["batch_max_items"], 2);

    let replayed =
        response_json(get_route_with_token(&router, &uri, Some(&fixture.session)).await).await;
    assert_eq!(replayed["revision"], 2);
    assert_eq!(replayed["effective"]["concurrency"], 3);

    clean_runtime_files(&fixture.job_id);
}

#[tokio::test]
async fn dashboard_controls_require_a_fresh_lease_and_signal_one_when_present() {
    let fixture = build_mutation_fixture();
    make_stopped_fixture_resumable(&fixture);
    clean_runtime_files(&fixture.job_id);
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.runtime_lease_stale_after = Duration::from_millis(u64::MAX);
    let router = dashboard_router(state);

    for command in ["pause", "stop"] {
        let response = post_json(
            &router,
            &format!("/api/jobs/{}/{}", fixture.job_id, command),
            Some(&fixture.session),
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
            Some(&fixture.session),
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
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
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
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    let keys = state.keys.clone();
    state.resume_launches = Some(launches.clone());
    state.resume_child_environments = Some(environments.clone());
    let router = dashboard_router(state);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);
    let uri = format!("/api/jobs/{}/resume", fixture.job_id);

    let (first, second) = tokio::join!(
        post_json(&router, &uri, Some(&fixture.session), json!({})),
        post_json(&router, &uri, Some(&fixture.session), json!({}))
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let view = response_json(
        get_route_with_token(
            &router,
            &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            Some(&fixture.session),
        )
        .await,
    )
    .await;
    assert_eq!(view["resumable_work"], true);
    assert_eq!(view["editable"], true);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);
    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.session),
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
    let mut state = test_state_with_store(&fixture.session, fixture.store_path.clone());
    state.resume_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let view = response_json(
        get_route_with_token(
            &router,
            &format!("/api/jobs/{}/reconfigure", fixture.job_id),
            Some(&fixture.session),
        )
        .await,
    )
    .await;
    assert_eq!(view["resumable_work"], false);
    assert_eq!(view["editable"], false);

    let response = post_json(
        &router,
        &format!("/api/jobs/{}/resume", fixture.job_id),
        Some(&fixture.session),
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
        &fixture.session,
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
        &fixture.session,
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
        &fixture.session,
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
        &fixture.session,
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
                .header("cookie", session_cookie_value(&fixture.session))
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
        Some(&fixture.session),
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
            Some(&fixture.session),
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
        Some(&fixture.session),
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
        Some(&fixture.session),
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
    // protectees (review documents, job listing) and options metadata, plus
    // the store-curation reads added for style/entity parity.
    for uri in [
        "/api/jobs",
        "/api/jobs/some-job/review",
        "/api/jobs/some-job",
        "/api/options",
        "/api/providers",
        "/api/styles",
        "/api/entities",
        "/api/audiobooks/some-op/prune-preview",
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
async fn root_exchange_mints_a_session_cookie_and_redirects_without_leaking() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    let router = dashboard_router(state);

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
    assert!(!login_page.contains(&secret));
    assert!(!login_page.contains("bookforge_session="));

    // Wrong ?token= is rejected without echoing the expected value.
    let rejected = get_route_with_token(&router, "/?token=deadbeef", None).await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);

    // Right ?token= exchanges for a 302 to the clean root plus an HttpOnly
    // browser-session cookie. The bootstrap token is never echoed and never
    // stored in the cookie — the cookie carries a fresh session id.
    let bootstrapped = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(bootstrapped.status(), StatusCode::FOUND);
    assert_eq!(
        bootstrapped.headers().get("location"),
        Some(&HeaderValue::from_static("/")),
        "redirect must land on the clean root, never echoing the token"
    );
    let set_cookie = bootstrapped
        .headers()
        .get("set-cookie")
        .expect("session cookie issued")
        .to_str()
        .expect("cookie is ascii");
    for attribute in [
        "bookforge_session=",
        "HttpOnly",
        "SameSite=Strict",
        "Path=/",
    ] {
        assert!(
            set_cookie.contains(attribute),
            "session cookie must carry {attribute:?}"
        );
    }
    assert!(
        !set_cookie.contains("Max-Age"),
        "session cookie must be browser-session lifetime"
    );
    assert!(
        !set_cookie.contains("Secure"),
        "loopback HTTP must not require the Secure flag"
    );
    let session_id = set_cookie
        .split(';')
        .next()
        .and_then(|pair| pair.strip_prefix("bookforge_session="))
        .expect("cookie value");
    assert_ne!(
        session_id, secret,
        "session id must not be the bootstrap token"
    );
    assert_eq!(
        session_id.len(),
        32,
        "session ids are fresh 128-bit hex values"
    );
    assert!(session_id.chars().all(|ch| ch.is_ascii_hexdigit()));

    // A caller holding the freshly minted session cookie loads the dashboard.
    let direct = get_route_with_token(&router, "/", Some(session_id)).await;
    assert_eq!(direct.status(), StatusCode::OK);
    // ...while the bootstrap token alone is not a session credential.
    let token_not_a_session = get_route_with_token(&router, "/", Some(&secret)).await;
    let body = axum::body::to_bytes(token_not_a_session.into_body(), usize::MAX)
        .await
        .expect("login body should read");
    let page = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(
        page.contains("bookforge serve"),
        "bootstrap token alone logs nobody in"
    );
    assert!(!page.contains(&secret));
}

#[tokio::test]
async fn bootstrap_token_is_single_use_and_replays_cannot_mint_a_second_session() {
    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    let router = dashboard_router(state);

    // First exchange succeeds and issues a session cookie.
    let first = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(first.status(), StatusCode::FOUND);
    assert!(
        first.headers().get("set-cookie").is_some(),
        "first exchange mints a session cookie"
    );

    // Replaying the exact console token is refused: no redirect, no cookie,
    // and no second session is minted for the replayed link.
    let replay = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(replay.headers().get("set-cookie"), None);

    // The consumed token is not a session credential either.
    let as_session = get_route_with_token(&router, "/api/jobs", Some(&secret)).await;
    assert_eq!(as_session.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bootstrap_exchange_rejects_cross_site_browser_requests() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    let router = dashboard_router(state);

    // A cross-site browser request carrying ?token= is refused outright, so it
    // can neither mint a session nor burn the failed-exchange limiter.
    let cross_site = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/?token={secret}"))
                .header("host", TEST_HOST)
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    assert_eq!(cross_site.headers().get("set-cookie"), None);

    // The token is untouched: a normal top-level navigation still exchanges.
    let nav = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(nav.status(), StatusCode::FOUND);
    assert!(
        nav.headers().get("set-cookie").is_some(),
        "a non-cross-site navigation still exchanges the token"
    );
}

#[tokio::test]
async fn expired_bootstrap_token_gets_guidance_and_no_session() {
    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    state.auth.expire_bootstrap_for_test();
    let router = dashboard_router(state);

    // The correct token past its TTL gets the helpful expiry page (200), not
    // an authenticated dashboard and not a session cookie.
    let expired = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(expired.status(), StatusCode::OK);
    assert_eq!(expired.headers().get("set-cookie"), None);
    let body = axum::body::to_bytes(expired.into_body(), usize::MAX)
        .await
        .expect("expiry body should read");
    let page = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(
        page.contains("expired"),
        "expiry screen gives actionable guidance"
    );
    assert!(
        !page.contains(&secret),
        "expiry screen never echoes the token"
    );

    // API routes remain unauthenticated.
    let rejected = get_route_with_token(&router, "/api/jobs", None).await;
    assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn failed_bootstrap_exchanges_are_bounded_by_the_limiter() {
    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    let router = dashboard_router(state);

    // Wrong guesses are all refused (401, no leak), then the limiter trips and
    // even the correct token is refused within the same window.
    for _ in 0..8 {
        let rejected = get_route_with_token(&router, "/?token=deadbeef", None).await;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
    }
    let locked_out = get_route_with_token(&router, &format!("/?token={secret}"), None).await;
    assert_eq!(locked_out.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(locked_out.into_body(), usize::MAX)
        .await
        .expect("rejection body should read");
    let body = String::from_utf8(body.to_vec()).expect("body is utf-8");
    assert!(!body.contains(&secret), "rejections never echo the token");
}

#[tokio::test]
async fn logout_invalidates_the_session_and_expires_the_cookie() {
    let state = test_state("token-123");
    let router = dashboard_router(state);

    // Authenticated first.
    let jobs = get_route_with_token(&router, "/api/jobs", Some("token-123")).await;
    assert_eq!(jobs.status(), StatusCode::OK);

    // POST /api/auth/logout with the session cookie: 204 + expiring cookie.
    let logout = post_json(&router, "/api/auth/logout", Some("token-123"), json!({})).await;
    assert_eq!(logout.status(), StatusCode::NO_CONTENT);
    let set_cookie = logout
        .headers()
        .get("set-cookie")
        .expect("logout sends an expiring cookie")
        .to_str()
        .expect("cookie is ascii");
    assert!(set_cookie.contains("Max-Age=0"), "cookie must be expired");
    assert!(set_cookie.contains("HttpOnly"));

    // The invalidated session no longer authenticates any protected route.
    let after = get_route_with_token(&router, "/api/jobs", Some("token-123")).await;
    assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(after.into_body(), usize::MAX)
        .await
        .expect("rejection body should read");
    let body = String::from_utf8(body.to_vec()).expect("body is utf-8");
    assert!(
        !body.contains("token-123"),
        "rejections never echo the session"
    );

    // Logout is idempotent: clearing an already-dead session is still 204.
    let again = post_json(&router, "/api/auth/logout", Some("token-123"), json!({})).await;
    assert_eq!(again.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn unauthenticated_api_routes_are_rejected_with_a_bare_401() {
    let state = test_state("token-123");
    let router = dashboard_router(state);

    for uri in ["/api/jobs", "/api/options", "/api/audiobooks"] {
        let response = get_route_with_token(&router, uri, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("rejection body should read");
        let body = String::from_utf8(body.to_vec()).expect("body is utf-8");
        assert!(
            !body.contains("token-123"),
            "401 bodies must not echo the expected credential ({uri})"
        );
    }

    // A wrong cookie is as opaque as a missing one.
    let wrong = get_route_with_token(&router, "/api/jobs", Some("wrong-session")).await;
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn mutations_are_gated_by_authenticated_session_and_same_origin() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    let temp = tempfile::tempdir().expect("temp dir should be created");
    let state = test_state_with_store("token-123", temp.path().join("jobs.sqlite"));
    let router = dashboard_router(state);

    // Valid session cookie but a cross-site fetch-metadata marker: refused.
    let cross_site = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs/not-real/retry")
                .header("host", TEST_HOST)
                .header("cookie", session_cookie_value("token-123"))
                .header("sec-fetch-site", "cross-site")
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);

    // Same-origin with a valid session passes both gates and reaches the
    // handler (an unknown job on an empty store -> "retried": 0).
    let same_origin = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs/not-real/retry")
                .header("host", TEST_HOST)
                .header("cookie", session_cookie_value("token-123"))
                .body(Body::empty())
                .expect("request should build"),
        )
        .await
        .expect("route should respond");
    assert_ne!(same_origin.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(same_origin.status(), StatusCode::FORBIDDEN);
    assert_eq!(same_origin.status(), StatusCode::OK);
}

/// No token or session id may appear in any response a browser can read:
/// the dashboard HTML, the login pages, and the API JSON.
#[tokio::test]
async fn no_secret_ever_reaches_dashboard_bytes_or_api_responses() {
    let state = test_state("session-1");
    let secret = state.auth.bootstrap_token();
    let router = dashboard_router(state);

    // Authenticated dashboard HTML.
    let index = get_route_with_token(&router, "/", Some("session-1")).await;
    assert_eq!(index.status(), StatusCode::OK);
    let body = axum::body::to_bytes(index.into_body(), usize::MAX)
        .await
        .expect("index body should read");
    let page = String::from_utf8(body.to_vec()).expect("index is utf-8");
    assert!(
        !page.contains(&secret),
        "bootstrap token must not appear in HTML"
    );
    assert!(
        !page.contains("session-1"),
        "session id must not appear in HTML"
    );

    // Authenticated API JSON.
    let options = get_route_with_token(&router, "/api/options", Some("session-1")).await;
    assert_eq!(options.status(), StatusCode::OK);
    let body = axum::body::to_bytes(options.into_body(), usize::MAX)
        .await
        .expect("options body should read");
    let options = String::from_utf8(body.to_vec()).expect("options is utf-8");
    assert!(
        !options.contains(&secret),
        "API JSON must not leak the bootstrap token"
    );
    assert!(
        !options.contains("session-1"),
        "API JSON must not leak the session id"
    );

    // Login page.
    let login = get_route_with_token(&router, "/", None).await;
    let body = axum::body::to_bytes(login.into_body(), usize::MAX)
        .await
        .expect("login body should read");
    let login = String::from_utf8(body.to_vec()).expect("login is utf-8");
    assert!(!login.contains(&secret));
    assert!(!login.contains("session-1"));
}

#[tokio::test]
async fn no_auth_restores_the_previous_unauthenticated_behavior() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let state = AppState {
        store_path: temp.path().join("jobs.sqlite"),
        ..test_state_opt("quiet-token", false)
    };
    let router = dashboard_router(state);

    // Reads reach handlers without any session credential...
    let jobs = get_route_with_token(&router, "/api/jobs", None).await;
    assert_eq!(jobs.status(), StatusCode::OK);
    // ...and / serves the full dashboard with no token or session id embedded
    // anywhere in the bytes.
    let index = get_route_with_token(&router, "/", None).await;
    assert_eq!(index.status(), StatusCode::OK);
    let body = axum::body::to_bytes(index.into_body(), usize::MAX)
        .await
        .expect("index body should read");
    let page = String::from_utf8(body.to_vec()).expect("page is utf-8");
    assert!(
        !page.contains("quiet-token"),
        "--no-auth must never embed a session id"
    );
    assert!(!page.contains("bookforge_session="));
    assert!(!page.contains("sessionStorage"));

    // Mutations skip the session gate entirely (only same-origin checks apply):
    // with no cookie this reaches the handler rather than 401/403.
    let retry = post_json(&router, "/api/jobs/not-real/retry", None, json!({})).await;
    assert_ne!(retry.status(), StatusCode::UNAUTHORIZED);
    assert_ne!(retry.status(), StatusCode::FORBIDDEN);
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
        &fixture.session,
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
            Some(&fixture.session),
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
            .header("cookie", session_cookie_value("token-123"))
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
    // Pass-cost planning surcharges: one entry per pass plus a REAL total
    // (primary + surcharges). The mock provider prices everything at $0, so
    // both are exact zeros — but they must be present and coherent.
    let passes = payload["est_cost_usd_passes"]
        .as_object()
        .expect("est_cost_usd_passes is an object keyed per pass");
    assert_eq!(
        passes.keys().collect::<Vec<_>>(),
        vec!["qa review", "repair share"]
    );
    assert!(passes.values().all(|value| value.as_f64() == Some(0.0)));
    assert_eq!(payload["est_cost_usd_total"], json!(0.0));
}

// -----------------------------------------------------------------------
// Provider options <-> core registry sync (DUP-6 follow-through): the
// dashboard must never grow a second copy of endpoint defaults, and the
// per-provider capability flags it serves come from the same matrix
// synthesis consults.
// -----------------------------------------------------------------------

#[test]
fn dashboard_provider_options_stay_synced_with_core_registry() {
    let options = dashboard_options_payload();

    for provider in &options.providers {
        match bookforge_core::providers::provider_defaults(provider.id) {
            Some(defaults) => {
                assert_eq!(
                    provider.requires_base_url,
                    defaults.base_url.is_none(),
                    "{} base-url requirement must follow the registry",
                    provider.id
                );
                assert!(
                    provider.requires_key,
                    "registry-backed cloud providers require keys"
                );
                assert_eq!(provider_key_env(provider.id), Some(defaults.api_key_env));
                if let Some(default_model) = defaults.default_model {
                    assert_eq!(provider.default_model, default_model);
                }
            }
            None => {
                // The only intentionally unregistered chip is the offline
                // mock; anything else that appears here must be added to the
                // core registry first.
                assert_eq!(provider.id, "mock");
                assert!(!provider.requires_key);
                assert!(!provider.requires_base_url);
            }
        }
    }

    // Every env mapping the dashboard remembers must equal the registry's.
    for (provider, env) in PROVIDER_KEY_ENVS {
        assert_eq!(
            bookforge_core::providers::provider_defaults(provider)
                .map(|defaults| defaults.api_key_env),
            Some(*env),
            "{provider} env mapping drifted from the core registry"
        );
    }
}

#[test]
fn audio_options_flag_text_normalization_from_the_capability_matrix() {
    let options = dashboard_options_payload();
    for provider in &options.audio_providers {
        let expected = bookforge_audio::feature_set_for_id(provider.id)
            .is_some_and(|features| features.text_normalization);
        assert_eq!(
            provider.supports_text_normalization, expected,
            "{} text-normalization flag must mirror feature_set_for_id",
            provider.id
        );
        assert!(
            DASHBOARD_JS.contains("supports_text_normalization"),
            "the browser must gate the control on the served flag"
        );
    }
}

// -----------------------------------------------------------------------
// Style-sheet store CRUD on the dashboard (audit Feature-asymmetry: zero
// dashboard endpoints -> full create/read/update/delete).
// -----------------------------------------------------------------------

fn global_style_toml(target_language: &str) -> String {
    format!(
        r#"[meta]
schema_version = 1
target_language = "{target_language}"

[meta.scope]
kind = "global"

[register]
narration = "literary"

[voice]

[do_not]

[free_text]
instructions = "Keep loanwords as-is."
"#
    )
}

#[tokio::test]
async fn styles_crud_end_to_end_with_precise_single_row_delete() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let store_path = temp.path().join("jobs.sqlite");
    let router = dashboard_router(test_state_with_store("token-123", store_path.clone()));

    // Create.
    let created = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({
            "target_language": "Italian",
            "content_toml": global_style_toml("Italian"),
            "scope": "global",
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK, "create");
    let italian_id = response_json(created).await["id"]
        .as_i64()
        .expect("created id");

    // Read (list + single + fingerprint derived like `style import` does).
    let list = response_json(get_route(&router, "/api/styles").await).await;
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["id"], italian_id);
    assert_eq!(list[0]["target_language"], "Italian");
    assert_eq!(list[0]["scope"], "global");
    let record =
        response_json(get_route(&router, &format!("/api/styles/{italian_id}")).await).await;
    let content = record["content_toml"].as_str().expect("stored content");
    assert_eq!(content, global_style_toml("Italian"));
    let sheet = crate::commands::style::parse_style_toml(content).expect("valid stored TOML");
    let merged = bookforge_core::style::merge_style_sheets(&[sheet]);
    assert_eq!(
        record["fingerprint"],
        bookforge_core::style::style_fingerprint(merged.as_ref())
    );

    // Upsert duplicate identity updates rather than forks.
    let mut updated_content = global_style_toml("Italian");
    updated_content = updated_content.replace("\"literary\"", "\"lyrical\"");
    let upserted = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({ "target_language": "Italian", "content_toml": updated_content, "scope": "global" }),
    )
    .await;
    assert_eq!(upserted.status(), StatusCode::OK);
    assert_eq!(response_json(upserted).await["id"], json!(italian_id));

    // Honest error states.
    let empty_lang = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({ "target_language": "  ", "content_toml": global_style_toml("Italian"), "scope": "global" }),
    )
    .await;
    assert_eq!(empty_lang.status(), StatusCode::BAD_REQUEST);
    let garbage = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({ "target_language": "Italian", "content_toml": "[meta\nbroken", "scope": "global" }),
    )
    .await;
    assert_eq!(garbage.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(garbage).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("invalid style sheet"),
        "a malformed payload explains itself"
    );
    let no_scope_id = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({
            "target_language": "Italian",
            "content_toml": global_style_toml("Italian"),
            "scope": "book"
        }),
    )
    .await;
    assert_eq!(no_scope_id.status(), StatusCode::BAD_REQUEST);

    // Update in place; identity change is refused with guidance.
    let put = axum_put_json(
        &router,
        &format!("/api/styles/{italian_id}"),
        Some("token-123"),
        json!({ "content_toml": global_style_toml("Italian") }),
    )
    .await;
    assert_eq!(put.status(), StatusCode::OK);
    assert_eq!(response_json(put).await["updated"], true);
    let relanguage = axum_put_json(
        &router,
        &format!("/api/styles/{italian_id}"),
        Some("token-123"),
        json!({ "content_toml": global_style_toml("Spanish"), "target_language": "Spanish" }),
    )
    .await;
    assert_eq!(relanguage.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(relanguage).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("delete and recreate")
    );

    // Precise delete: a sibling Spanish row written directly into the same
    // scope survives exactly intact while the Italian one is gone.
    let direct_store = JobStore::open(store_path.clone()).expect("store reopen");
    let spanish_content = global_style_toml("Spanish");
    let spanish_sheet =
        crate::commands::style::parse_style_toml(&spanish_content).expect("spanish parse");
    let spanish_merged = bookforge_core::style::merge_style_sheets(&[spanish_sheet]);
    let spanish_fp = bookforge_core::style::style_fingerprint(spanish_merged.as_ref());
    direct_store
        .upsert_style_sheet(&NewStyleSheet {
            scope_kind: GlossaryScopeKind::Global,
            scope_id: None,
            target_language: "Spanish",
            content_toml: &spanish_content,
            fingerprint: &spanish_fp,
        })
        .expect("sibling seeded");

    let missing_delete = axum_delete(&router, "/api/styles/99999", Some("token-123")).await;
    assert_eq!(missing_delete.status(), StatusCode::NOT_FOUND);

    // Pin the sibling id before the delete: with the atomic single-row
    // primitive the surviving sibling must keep exactly this id (the retired
    // snapshot-clear-restore path reassigned sibling ids on every removal).
    let before = response_json(get_route(&router, "/api/styles").await).await;
    let spanish_id_before = before
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["target_language"] == "Spanish")
        .map(|row| row["id"].clone())
        .expect("spanish sibling present");

    let deleted = axum_delete(
        &router,
        &format!("/api/styles/{italian_id}"),
        Some("token-123"),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(response_json(deleted).await["removed"], 1);

    let remaining = response_json(get_route(&router, "/api/styles").await).await;
    let rows = remaining.as_array().expect("rows");
    assert_eq!(
        rows.len(),
        1,
        "only the untouched sibling remains: {remaining}"
    );
    assert_eq!(rows[0]["target_language"], "Spanish");
    assert_eq!(
        rows[0]["id"], spanish_id_before,
        "sibling ids must stay stable across a delete (F1)"
    );
    assert_eq!(rows[0]["content_toml"], json!(spanish_content));
    assert_eq!(rows[0]["fingerprint"], json!(spanish_fp));

    // Removal is proven by content: the Italian triple cannot resolve again
    // (a matching-sheet read now misses), while unknown numeric ids stay 404.
    let repolished = post_json(
        &router,
        "/api/styles",
        Some("token-123"),
        json!({
            "target_language": "Italian",
            "content_toml": global_style_toml("Italian"),
            "scope": "global"
        }),
    )
    .await;
    let second_italian_id = response_json(repolished).await["id"].as_i64().expect("id");
    let wiped = axum_delete(
        &router,
        &format!("/api/styles/{second_italian_id}"),
        Some("token-123"),
    )
    .await;
    assert_eq!(wiped.status(), StatusCode::OK);
}

#[tokio::test]
async fn store_asset_mutations_reject_missing_dashboard_token() {
    // Styles: create / update / delete.
    let add_style = post_json(
        &dashboard_router(test_state("token-123")),
        "/api/styles",
        None,
        json!({ "target_language": "Italian", "content_toml": "x", "scope": "global" }),
    )
    .await;
    assert_eq!(add_style.status(), StatusCode::UNAUTHORIZED);

    let put_style = axum_put_json(
        &dashboard_router(test_state("token-123")),
        "/api/styles/1",
        None,
        json!({ "content_toml": "x" }),
    )
    .await;
    assert_eq!(put_style.status(), StatusCode::UNAUTHORIZED);

    let delete_style = axum_delete(
        &dashboard_router(test_state("token-123")),
        "/api/styles/1",
        None,
    )
    .await;
    assert_eq!(delete_style.status(), StatusCode::UNAUTHORIZED);

    // Entities: create / update / delete.
    let add_entity = post_json(
        &dashboard_router(test_state("token-123")),
        "/api/entities",
        None,
        json!({
            "source_name": "a", "target_name": "b",
            "source_language": "English", "target_language": "Italian"
        }),
    )
    .await;
    assert_eq!(add_entity.status(), StatusCode::UNAUTHORIZED);

    let put_entity = axum_put_json(
        &dashboard_router(test_state("token-123")),
        "/api/entities/1",
        None,
        json!({ "target_name": "b" }),
    )
    .await;
    assert_eq!(put_entity.status(), StatusCode::UNAUTHORIZED);

    let delete_entity = axum_delete(
        &dashboard_router(test_state("token-123")),
        "/api/entities/1",
        None,
    )
    .await;
    assert_eq!(delete_entity.status(), StatusCode::UNAUTHORIZED);
}

fn spanish_sibling_row() -> NewEntity<'static> {
    NewEntity {
        scope_kind: GlossaryScopeKind::Global,
        scope_id: None,
        source_name: "Samwise Gamgee",
        target_name: "Sancho Panza",
        gender_target: None,
        role: Some("gardener"),
        notes: None,
        source_language: "English",
        target_language: "Spanish",
    }
}

fn seed_spanish_sibling(store_path: &std::path::Path) {
    let store = JobStore::open(store_path).expect("store reopen");
    store
        .upsert_entities(&[spanish_sibling_row()])
        .expect("sibling row seeded");
}

#[tokio::test]
async fn entities_crud_end_to_end_with_precise_single_row_delete() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let store_path = temp.path().join("jobs.sqlite");
    let router = dashboard_router(test_state_with_store("token-123", store_path.clone()));

    // Create.
    let created = post_json(
        &router,
        "/api/entities",
        Some("token-123"),
        json!({
            "source_name": "Frodo Baggins",
            "target_name": "Frodo Baggins",
            "gender": "m",
            "role": "ring-bearer",
            "notes": "protagonist",
            "source_language": "English",
            "target_language": "Italian",
            "scope": "global"
        }),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK, "create");
    let frodo_id = response_json(created).await["id"]
        .as_i64()
        .expect("created id");

    // Read list + single.
    let list = response_json(get_route(&router, "/api/entities").await).await;
    assert_eq!(list.as_array().expect("rows").len(), 1);
    assert_eq!(list[0]["id"], frodo_id);
    assert_eq!(list[0]["source"], "Frodo Baggins");
    assert_eq!(list[0]["gender"], "m");
    assert_eq!(list[0]["role"], "ring-bearer");
    assert_eq!(list[0]["target_language"], "Italian");
    let single =
        response_json(get_route(&router, &format!("/api/entities/{frodo_id}")).await).await;
    assert_eq!(single["id"], frodo_id);

    // Validation refusals: unknown gender code and scoped-without-id.
    let bad_gender = post_json(
        &router,
        "/api/entities",
        Some("token-123"),
        json!({
            "source_name": "x", "target_name": "y", "gender": "q",
            "source_language": "English", "target_language": "Italian"
        }),
    )
    .await;
    assert_eq!(bad_gender.status(), StatusCode::BAD_REQUEST);
    let missing_scope_id = post_json(
        &router,
        "/api/entities",
        Some("token-123"),
        json!({
            "source_name": "x", "target_name": "y",
            "source_language": "English", "target_language": "Italian",
            "scope": "book"
        }),
    )
    .await;
    assert_eq!(missing_scope_id.status(), StatusCode::BAD_REQUEST);

    // Update mutable fields; identity echo mismatch is refused.
    let put = axum_put_json(
        &router,
        &format!("/api/entities/{frodo_id}"),
        Some("token-123"),
        json!({ "target_name": "Frodo", "gender": null, "role": null, "notes": null }),
    )
    .await;
    assert_eq!(put.status(), StatusCode::OK);
    assert_eq!(response_json(put).await["updated"], true);
    let renamed_identity = axum_put_json(
        &router,
        &format!("/api/entities/{frodo_id}"),
        Some("token-123"),
        json!({ "target_name": "Frodo", "source_name": "Bilbo" }),
    )
    .await;
    assert_eq!(renamed_identity.status(), StatusCode::BAD_REQUEST);

    // Precise delete with a Spanish sibling seeded directly through the
    // store upsert API.
    seed_spanish_sibling(&store_path);
    let before = response_json(get_route(&router, "/api/entities").await).await;
    let samwise_id_before = before
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["source"] == "Samwise Gamgee")
        .map(|row| row["id"].clone())
        .expect("spanish sibling present");
    let missing_delete = axum_delete(&router, "/api/entities/99999", Some("token-123")).await;
    assert_eq!(missing_delete.status(), StatusCode::NOT_FOUND);
    let deleted = axum_delete(
        &router,
        &format!("/api/entities/{frodo_id}"),
        Some("token-123"),
    )
    .await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(response_json(deleted).await["removed"], 1);

    let remaining = response_json(get_route(&router, "/api/entities").await).await;
    let rows = remaining.as_array().expect("rows");
    assert_eq!(
        rows.len(),
        1,
        "only the untouched sibling remains: {remaining}"
    );
    assert_eq!(rows[0]["target_language"], "Spanish");
    assert_eq!(rows[0]["source"], "Samwise Gamgee");
    assert_eq!(
        rows[0]["id"], samwise_id_before,
        "sibling ids must stay stable across a delete (F1)"
    );

    // The sibling kept its stored fields untouched, including the manual
    // correction applied above, which the request never saw.
    let rows = JobStore::open(store_path)
        .expect("store reopen")
        .list_entities(None, None, None, None)
        .expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source_name, "Samwise Gamgee");
    assert_eq!(rows[0].target_name, "Sancho Panza");
    assert_eq!(rows[0].role.as_deref(), Some("gardener"));
}

// -----------------------------------------------------------------------
// AUDIO-6/8 remainder parity on the dashboard: chapters passthrough,
// text normalization gating, timeout, prune preview/execute, retry-failed
// relaunch validation.
// -----------------------------------------------------------------------

#[test]
fn audiobook_command_args_forward_chapters_normalization_and_timeout() {
    let advanced = AudiobookCommandOptions {
        seed: Some(7),
        language: Some("pt-BR".to_string()),
        chapters: Some("1-3,7".to_string()),
        text_normalization: Some("off".to_string()),
        timeout_seconds: Some(90),
        ..AudiobookCommandOptions::default()
    };
    let args = audiobook_command_args(
        Path::new("book.epub"),
        Path::new("audio-out"),
        "elevenlabs",
        None,
        "voice-id",
        "mp3",
        1.0,
        2_000,
        4,
        None,
        None,
        true,
        true,
        None,
        &advanced,
    );
    let args: Vec<String> = args
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    for pair in [
        ("--chapters", "1-3,7"),
        ("--text-normalization", "off"),
        ("--timeout-seconds", "90"),
    ] {
        let index = args
            .iter()
            .position(|value| value == pair.0)
            .unwrap_or_else(|| panic!("{} must be forwarded", pair.0));
        assert_eq!(args.get(index + 1).map(String::as_str), Some(pair.1));
    }

    // Unset values stay off the command line (CLI defaults cover them),
    // mirroring launch_audiobook's skip rules.
    let default_args = audiobook_command_args(
        Path::new("book.epub"),
        Path::new("audio-out"),
        "openai",
        Some("gpt-4o-mini-tts"),
        "alloy",
        "mp3",
        1.0,
        4_096,
        4,
        None,
        None,
        true,
        true,
        None,
        &AudiobookCommandOptions::default(),
    );
    for flag in ["--chapters", "--text-normalization", "--timeout-seconds"] {
        assert!(
            !default_args.iter().any(|arg| arg == flag),
            "{flag} must be omitted when unset"
        );
    }
}

#[test]
fn chapter_ranges_roundtrip_through_the_shared_cli_parser() {
    use crate::commands::audiobook::parse_chapter_ranges;

    assert_eq!(
        format_chapter_ranges(&parse_chapter_ranges("3").unwrap()),
        "3"
    );
    assert_eq!(
        format_chapter_ranges(&parse_chapter_ranges("1-3, 7").unwrap()),
        "1-3,7"
    );
    assert_eq!(
        format_chapter_ranges(&parse_chapter_ranges("2,3,9-11,5").unwrap()),
        "2-3,5,9-11"
    );
}

fn write_prune_fixture(upload_dir: &Path, id: &str, options: serde_json::Value) -> PathBuf {
    let out_dir = upload_dir.join(format!("audiobook-{id}"));
    std::fs::create_dir_all(&out_dir).expect("operation directory");
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "title": null,
            "synthesis_id": "mock:mock-silence",
            "voice": "mock",
            "format": "wav",
            "speed": 1.0,
            "max_chars": 2000,
            "gaps": {"chapter_ms": 1200, "title_ms": 800, "paragraph_ms": 0},
            "chapters": 1,
            "chunks": [{
                "chapter_index": 0,
                "chapter_title": "One",
                "part": 1,
                "kind": "body",
                "file": "chapter-001-part-001-aabbccddeeff0011.wav",
                "chars": 40,
                "synthesis_sha256": "a".repeat(64),
                "status": "synthesized"
            }],
            "status": "succeeded"
        }))
        .expect("manifest should serialize"),
    )
    .expect("manifest written");
    std::fs::write(
        out_dir.join("process.json"),
        serde_json::to_vec(&json!({
            "status": "succeeded",
            "pid": null,
            "error": null,
            "auto_model": false,
            "options": options,
            "updated_at_ms": 1
        }))
        .expect("process state should serialize"),
    )
    .expect("process written");
    // One crash-debris file plus one orphaned-but-managed chunk from an older
    // settings mix; both are prunable when the plan is a full plan, while the
    // kept chunk above is not.
    std::fs::write(out_dir.join(".audiobook.m4b.42.part.m4b"), b"debris").expect("debris written");
    std::fs::write(
        out_dir.join("chapter-001-part-002-deadbeefdeadbeef.wav"),
        vec![0u8; 100],
    )
    .expect("orphan chunk written");
    out_dir
}

#[tokio::test]
async fn prune_preview_lists_debris_then_confirm_removes_it() {
    let temp = tempfile::tempdir().expect("temp dir");
    let _ = write_prune_fixture(temp.path(), "fullplan", full_plan_options_json());
    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));

    let preview = get_route(&router, "/api/audiobooks/fullplan/prune-preview").await;
    assert_eq!(preview.status(), StatusCode::OK);
    let payload = response_json(preview).await;
    assert_eq!(
        payload["stale_files"], 2,
        "debris + orphan chunk: {payload}"
    );
    assert_eq!(payload["restricted"], false);
    assert!(payload["stale_bytes"].as_u64().expect("bytes") >= 106);

    let executed = post_json(
        &router,
        "/api/audiobooks/fullplan/prune",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(executed.status(), StatusCode::OK);
    let removed = response_json(executed).await;
    assert_eq!(removed["removed"], 2);
    assert!(removed["freed_bytes"].as_u64().expect("freed") > 0);

    let after =
        response_json(get_route(&router, "/api/audiobooks/fullplan/prune-preview").await).await;
    assert_eq!(after["stale_files"], 0, "second preview is empty: {after}");
}

fn full_plan_options_json() -> serde_json::Value {
    json!({
        "provider": "mock", "model": "", "voice": "mock", "format": "wav",
        "speed": 1.0, "max_chars": 2000, "concurrency": 2,
        "instructions": null, "base_url": null, "gap_chapter_ms": null,
        "gap_title_ms": null, "single": false, "loudnorm": false,
        "m4b": false, "stitch": false, "seed": null, "language": null,
        "chapters": null, "text_normalization": null, "timeout_seconds": null
    })
}

#[tokio::test]
async fn prune_degrades_to_debris_only_for_subset_runs_and_refuses_running_ops() {
    let temp = tempfile::tempdir().expect("temp dir");
    let _ = write_prune_fixture(temp.path(), "subsetrun", json!({ "chapters": "1-3" }));
    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));

    let preview =
        response_json(get_route(&router, "/api/audiobooks/subsetrun/prune-preview").await).await;
    assert_eq!(preview["restricted"], true, "{preview}");
    // Only the crash-debris shape is offered; managed chunk names cannot be
    // judged stale without the source book to re-plan against.
    assert_eq!(preview["stale_files"], 1);

    let unknown = get_route(&router, "/api/audiobooks/nope/prune-preview").await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // A live operation is refused before anything is listed or deleted.
    let out_dir = temp.path().join("audiobook-liveone");
    std::fs::create_dir_all(&out_dir).expect("operation dir");
    std::fs::write(
        out_dir.join("process.json"),
        serde_json::to_vec(&json!({"status":"running"})).expect("json should serialize"),
    )
    .expect("process overwritten");
    let preview_live = get_route(&router, "/api/audiobooks/liveone/prune-preview").await;
    assert_eq!(preview_live.status(), StatusCode::CONFLICT);
    let prune_live = post_json(
        &router,
        "/api/audiobooks/liveone/prune",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(prune_live.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn retry_failed_validates_manifest_state_before_spawning_anything() {
    let temp = tempfile::tempdir().expect("temp dir");
    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));

    // Unknown operation.
    let unknown = post_json(
        &router,
        "/api/audiobooks/nosuch/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    // A run with recorded settings but zero failed chunks refuses honestly.
    let _ = write_prune_fixture(temp.path(), "cleanop", full_plan_options_json());
    let clean = post_json(
        &router,
        "/api/audiobooks/cleanop/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(clean.status(), StatusCode::BAD_REQUEST, "{clean:?}");
    let clean_error = response_json(clean).await["error"]
        .as_str()
        .expect("error string")
        .to_string();
    assert!(
        clean_error.contains("no failed chunks"),
        "clean relaunch must refuse honestly: {clean_error}"
    );

    // A genuinely failed chunk whose process.json predates relaunch metadata
    // explains why it cannot relaunch rather than guessing at flags.
    let legacy_dir = write_prune_fixture(temp.path(), "legacyop", json!({}));
    std::fs::write(
        legacy_dir.join("process.json"),
        serde_json::to_vec(&json!({"status": "failed"})).expect("json should serialize"),
    )
    .expect("process rewritten");
    flip_first_chunk_status(&legacy_dir, "failed");

    let legacy = post_json(
        &router,
        "/api/audiobooks/legacyop/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(legacy.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(legacy).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("relaunch settings are unavailable"),
        "legacy relaunch must be refused with a usable explanation"
    );

    // The original upload is required to relaunch in place.
    let with_options_dir = write_prune_fixture(temp.path(), "noinp", full_plan_options_json());
    std::fs::write(
        with_options_dir.join("process.json"),
        serde_json::to_vec(&json!({
            "status": "failed",
            "pid": null,
            "error": null,
            "auto_model": false,
            "options": full_plan_options_json(),
            "updated_at_ms": 3
        }))
        .expect("json should serialize"),
    )
    .expect("process rewritten");
    flip_first_chunk_status(&with_options_dir, "failed");
    std::fs::remove_file(temp.path().join("audiobook-noinp.epub")).ok();

    let no_input = post_json(
        &router,
        "/api/audiobooks/noinp/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(no_input.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(no_input).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("original EPUB is no longer stored")
    );

    // Auth: every new mutation endpoint rejects anonymous callers.
    for uri in ["/api/audiobooks/x/prune", "/api/audiobooks/x/retry-failed"] {
        let rejected = post_json(&router, uri, None, json!({})).await;
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED, "{uri}");
    }
}

/// Flip the first manifest chunk's status so `failed_chunk_files` reports a
/// genuine failure state for relaunch validation tests.
fn flip_first_chunk_status(operation_dir: &Path, status: &str) {
    let path = operation_dir.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("manifest read"))
            .expect("manifest parse");
    let chunks = manifest["chunks"].as_array_mut().expect("chunks array");
    chunks[0]["status"] = json!(status);
    std::fs::write(&path, manifest.to_string()).expect("manifest rewritten");
}

/// A fully relaunchable failed operation: recorded options, one failed chunk,
/// and the original EPUB still stored so preparation reaches the spawn seam.
fn write_retryable_fixture(temp: &std::path::Path, id: &str) -> PathBuf {
    let dir = write_prune_fixture(temp, id, full_plan_options_json());
    flip_first_chunk_status(&dir, "failed");
    std::fs::write(
        temp.join(format!("audiobook-{id}.epub")),
        b"fixture epub bytes",
    )
    .expect("input epub written");
    dir
}

// -----------------------------------------------------------------------
// F3: retry-failed double-click protection (atomic claim + launch slot)
// -----------------------------------------------------------------------

#[tokio::test]
async fn retry_failed_conflicts_while_another_retry_holds_the_claim() {
    let temp = tempfile::tempdir().expect("temp dir");
    let router = dashboard_router(test_state_with_upload_dir(
        "token-123",
        temp.path().to_path_buf(),
    ));
    let dir = write_retryable_fixture(temp.path(), "claimedop");

    // Simulate a first retry sitting between its atomic rename and its spawn:
    // process.json has been claimed away and the claim temp exists.
    std::fs::rename(
        dir.join("process.json"),
        dir.join("process.retry-claim.tmp"),
    )
    .expect("claim simulated");

    let loser = post_json(
        &router,
        "/api/audiobooks/claimedop/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(loser.status(), StatusCode::CONFLICT);
    assert_eq!(
        response_json(loser).await["error"],
        json!("retry already starting")
    );

    // The settled (`running`) branch keeps its distinct refusal so operators
    // can tell "in flight" from "starting up".
    std::fs::rename(
        dir.join("process.retry-claim.tmp"),
        dir.join("process.json"),
    )
    .expect("restore original state");
    let mut process: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("process.json")).expect("state read"))
            .expect("state parse");
    process["status"] = json!("running");
    std::fs::write(dir.join("process.json"), process.to_string()).expect("state rewritten");

    let in_flight = post_json(
        &router,
        "/api/audiobooks/claimedop/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(in_flight.status(), StatusCode::CONFLICT);
    assert_eq!(
        response_json(in_flight).await["error"],
        json!("audiobook operation is not finished")
    );
}

/// Concurrent handler calls against one shared AppState (the harness shape
/// used by the SERVE-6 slot-cap tests): exactly one of the two simultaneous
/// "double clicks" may spawn; the other must be refused without spending.
#[tokio::test]
async fn retry_failed_double_click_starts_exactly_one_operation() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let temp = tempfile::tempdir().expect("temp dir");
    let _ = write_retryable_fixture(temp.path(), "racecase");

    let launches = Arc::new(AtomicUsize::new(0));
    let mut state = test_state_with_upload_dir("token-123", temp.path().to_path_buf());
    // Test hook (parity with resume_launches): records the relaunch instead
    // of exec'ing this binary as an audiobook child.
    state.retry_launches = Some(launches.clone());
    let router = dashboard_router(state);

    let (winner, loser) = tokio::join!(
        post_json(
            &router,
            "/api/audiobooks/racecase/retry-failed",
            Some("token-123"),
            json!({}),
        ),
        post_json(
            &router,
            "/api/audiobooks/racecase/retry-failed",
            Some("token-123"),
            json!({}),
        ),
    );

    let statuses = [
        (winner.status(), response_json(winner).await),
        (loser.status(), response_json(loser).await),
    ];
    let successes = statuses
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .count();
    let conflicts = statuses
        .iter()
        .filter(|(status, _)| *status == StatusCode::CONFLICT)
        .count();
    assert_eq!(successes, 1, "exactly one click wins: {statuses:?}");
    assert_eq!(conflicts, 1, "the losing click is refused: {statuses:?}");
    assert_eq!(
        launches.load(Ordering::SeqCst),
        1,
        "only one child may start regardless of interleaving"
    );

    // The winner leaves durable `running` state; no stray claim temp survives.
    let process: serde_json::Value = serde_json::from_slice(
        &std::fs::read(temp.path().join("audiobook-racecase/process.json")).expect("state read"),
    )
    .expect("state parse");
    assert_eq!(process["status"], json!("running"));
    assert!(
        !temp
            .path()
            .join("audiobook-racecase/process.retry-claim.tmp")
            .exists(),
        "the atomic claim must be consumed by the successful relaunch"
    );
}

/// Failure paths below the claim restore the durable state file and release
/// the launch slot, so a refused retry costs nothing and blocks nobody.
#[tokio::test]
async fn retry_failed_refusals_restore_state_and_release_the_launch_slot() {
    let temp = tempfile::tempdir().expect("temp dir");
    let state = test_state_with_upload_dir("token-123", temp.path().to_path_buf());
    let launch_slots = Arc::clone(&state.launch_slots);
    let router = dashboard_router(state);
    let dir = write_prune_fixture(temp.path(), "refusedop", full_plan_options_json());
    flip_first_chunk_status(&dir, "failed");
    // No input EPUB on purpose: preparation only fails after the atomic
    // claim was already taken.

    let before = std::fs::read(dir.join("process.json")).expect("original state");

    let refused = post_json(
        &router,
        "/api/audiobooks/refusedop/retry-failed",
        Some("token-123"),
        json!({}),
    )
    .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(
        response_json(refused).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("original EPUB is no longer stored")
    );

    assert!(
        !dir.join("process.retry-claim.tmp").exists(),
        "the claim must be rolled back after a refusal"
    );
    assert_eq!(
        std::fs::read(dir.join("process.json")).expect("restored state"),
        before,
        "the pre-retry process.json must survive byte-for-byte"
    );

    // Launch-slot bookkeeping: nothing leaked from the refused request; four
    // more launches must still fit under the cap afterwards.
    assert_eq!(*launch_slots.lock().unwrap(), 0, "slot released");
}
