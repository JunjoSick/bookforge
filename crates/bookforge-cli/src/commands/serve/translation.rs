use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/estimate", post(estimate_translate))
        .route("/api/translate", post(launch_translate))
}

async fn launch_translate(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = "upload.epub".to_string();
    let mut fields: HashMap<String, String> = HashMap::new();

    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if let Some(fname) = field.file_name()
                && !fname.is_empty()
            {
                file_name = fname.to_string();
            }
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }

    let Some(bytes) = file_bytes.filter(|b| !b.is_empty()) else {
        return Ok(bad_request("upload an EPUB file"));
    };
    let Some(target) = field_value(&fields, "target") else {
        return Ok(bad_request("target language is required"));
    };
    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !supported_provider(&provider) {
        return Ok(bad_request("unsupported provider"));
    }
    let openai_base_url = if provider == "openai-compatible" {
        let Some(base_url) = field_value(&fields, "base_url") else {
            return Ok(bad_request("base URL is required for openai-compatible"));
        };
        if !dashboard_base_url_uses_https(&base_url) {
            return Ok(bad_request(
                "base URL must use https:// for openai-compatible",
            ));
        }
        if field_value(&fields, "model").is_none() {
            return Ok(bad_request("model is required for openai-compatible"));
        }
        Some(base_url)
    } else {
        None
    };

    let supplied_key = (provider != "mock")
        .then(|| field_value(&fields, "api_key"))
        .flatten();
    let key = resolve_dashboard_provider_key(
        &state,
        &provider,
        supplied_key,
        provider_key_env(&provider),
    )?;
    if provider != "mock" && key.is_none() {
        return Ok(bad_request("provider API key is required"));
    }

    // SERVE-6: bound simultaneous launches so a stray tab or looping script
    // cannot start unbounded billable runs against remembered keys.
    let slot = match try_acquire_launch_slot(&state)? {
        LaunchSlot::Acquired(slot) => slot,
        LaunchSlot::Exhausted => return Ok(launch_slot_exhausted()),
    };

    // The monotonic sequence disambiguates two uploads of the same file name
    // landing within one millisecond (launch-filename-collision quality fix);
    // the run directory itself stays job-id keyed once the child registers.
    let stem = sanitize_component(strip_epub_suffix(&file_name));
    let tag = format!("{}-{}-{stem}", now_ms(), next_launch_seq());
    let upload_dir = state.upload_dir.clone();
    let input_path = upload_dir.join(format!("{tag}.epub"));
    let out_path = upload_dir.join(format!("{tag}.{}.epub", sanitize_component(&target)));
    let write_input_path = input_path.clone();
    // A 64 MB EPUB must not be memcpy'd to disk on an async worker thread.
    tokio::task::spawn_blocking(move || -> Result<()> {
        ensure_private_dir_under(Path::new(".bookforge"), &upload_dir)?;
        write_private_file(&write_input_path, &bytes)?;
        Ok(())
    })
    .await??;

    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    command
        .arg("translate")
        .arg(&input_path)
        .arg("--target")
        .arg(&target)
        .arg("--provider")
        .arg(&provider)
        .arg("--ui")
        .arg("quiet")
        .arg("--out")
        .arg(&out_path);
    if let Some(source) = field_value(&fields, "source") {
        command.arg("--source").arg(source);
    }
    // Offline mock runs are identity translations unless told otherwise.
    let model = field_value(&fields, "model")
        .or_else(|| (provider == "mock").then(|| "mock-identity".to_string()));
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    if let Some(profile) = field_value(&fields, "profile") {
        command.arg("--profile").arg(profile);
    }
    if provider == "deepseek" {
        command.arg("--no-thinking");
    }
    // Advanced tuning from the wizard, each validated before forwarding so the
    // child never receives arbitrary argv from the browser.
    if let Some(concurrency) = field_value(&fields, "concurrency")
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.clamp(1, 16))
    {
        command.arg("--concurrency").arg(concurrency.to_string());
    }
    if let Some(context) = field_value(&fields, "context_window")
        .and_then(|value| value.parse::<usize>().ok())
        .map(|value| value.min(16))
    {
        command.arg("--context-window").arg(context.to_string());
    }
    if let Some(qa) = field_value(&fields, "qa")
        .filter(|value| matches!(value.as_str(), "off" | "suspicious" | "all"))
    {
        command.arg("--qa").arg(qa);
    }
    if field_value(&fields, "validate_output")
        .is_some_and(|value| matches!(value.as_str(), "true" | "on" | "1"))
    {
        command.arg("--validate-output");
    }
    if let Some(base_url) = openai_base_url {
        command.arg("--base-url").arg(base_url);
    }
    let api_key_env = (provider != "mock" && key.is_some())
        .then(|| provider_key_env(&provider).expect("provider was validated"));
    configure_dashboard_child_environment(&mut command, api_key_env.zip(key.as_deref()));

    // Point the run at the one canonical provider key copied into its otherwise
    // scrubbed environment. The child records the env-var name in its job
    // snapshot, so `bookforge resume` can use the same name later.
    if let Some(env) = api_key_env {
        command.arg("--api-key-env").arg(env);
    }

    // Detached: the run outlives this request. The short startup check catches
    // immediate argv/binary failures before the dashboard reports success.
    // Either failure mode below cleans up the freshly written upload so a
    // dead launch does not leave an orphaned book on disk.
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&input_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(anyhow::Error::from(error)
                .context("failed to spawn translation process")
                .into());
        }
    };
    let pid = child.id();
    let completed_immediately = if let Some(status) =
        child_exit_status_after(&mut child, CHILD_STARTUP_CHECK).await?
    {
        if !status.success() {
            let _ = std::fs::remove_file(&input_path);
            let _ = std::fs::remove_file(&out_path);
            return Err(anyhow::anyhow!(
                    "translation process exited immediately with {status}; check the serve console for details"
                )
                .into());
        }
        true
    } else {
        false
    };
    drop(slot);

    Ok(Json(json!({
        "ok": true,
        "input_path": input_path.display().to_string(),
        "provider": provider,
        "pid": pid,
        "completed_immediately": completed_immediately,
    }))
    .into_response())
}

/// Estimate tokens and cost for an uploaded EPUB before the user commits to a
/// run. Shares [`estimate_epub`](super::estimate::estimate_epub) with the CLI
/// `estimate` command. The upload lives only inside a per-request private
/// temp directory (SERVE-5): 0700 on Unix, unpredictable name, deleted on
/// drop whether parsing succeeds, fails, or panics.
async fn estimate_translate(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut fields: HashMap<String, String> = HashMap::new();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }

    let Some(bytes) = file_bytes.filter(|b| !b.is_empty()) else {
        return Ok(bad_request("upload an EPUB file"));
    };
    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !supported_provider(&provider) {
        return Ok(bad_request("unsupported provider"));
    }
    let model = field_value(&fields, "model");
    let target = field_value(&fields, "target").unwrap_or_else(|| "Italian".to_string());
    let provider_for_passes = provider.clone();

    let result = tokio::task::spawn_blocking(move || {
        let temp = PrivateTempDir::create().context("failed to create a private temp directory")?;
        let path = temp.path.join("book.epub");
        write_private_file(&path, &bytes)?;
        super::estimate::estimate_epub(&path, &target, &provider, model.as_deref(), None)
    })
    .await?;

    let est = match result {
        Ok(est) => est,
        Err(err) => return Ok(bad_request(&format!("could not estimate: {err}"))),
    };
    // Pass-cost planning surcharges (same heuristics and catalog as
    // `estimate --pass-costs`): one JSON entry per pass plus a REAL total
    // (primary + surcharges). Existing keys stay unchanged.
    let (passes, surcharge_total) =
        super::estimate::pass_cost_surcharges(&provider_for_passes, &est, None)?;
    let est_cost_usd_passes = passes
        .iter()
        .map(|(label, usd)| ((*label).to_string(), serde_json::Value::from(*usd)))
        .collect::<serde_json::Map<String, serde_json::Value>>();
    let est_cost_usd_total = match (est.cost_usd, surcharge_total) {
        (Some(primary), Some(surcharges)) => Some(primary + surcharges),
        _ => None,
    };
    Ok(Json(json!({
        "segments": est.segments,
        "input_tokens": est.input_tokens,
        "output_tokens": est.output_tokens,
        "model": est.model,
        "cost_usd": est.cost_usd,
        "est_cost_usd_passes": est_cost_usd_passes,
        "est_cost_usd_total": est_cost_usd_total,
    }))
    .into_response())
}

fn supported_provider(provider: &str) -> bool {
    provider == "mock" || provider_key_env(provider).is_some()
}

pub(super) fn dashboard_base_url_uses_https(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).is_ok_and(|url| url.scheme() == "https" && url.host().is_some())
}

pub(super) fn provider_key_env(provider: &str) -> Option<&'static str> {
    PROVIDER_KEY_ENVS
        .iter()
        .find_map(|(known, env)| (*known == provider).then_some(*env))
}

/// Resolve a translation provider key without persisting it.
///
/// A key supplied by the browser replaces the session's remembered key for that
/// provider. Otherwise the session key wins over the one expected in the serve
/// process's environment. Callers still choose the env-var name injected into
/// the scrubbed child, which lets resume honor the run snapshot.
pub(super) fn resolve_dashboard_provider_key(
    state: &AppState,
    provider: &str,
    supplied_key: Option<String>,
    expected_env: Option<&str>,
) -> Result<Option<String>> {
    let supplied_key = supplied_key.and_then(|key| {
        let key = key.trim();
        (!key.is_empty()).then(|| key.to_string())
    });
    if let Some(supplied_key) = supplied_key {
        lock_keys(state)?.insert(provider.to_string(), supplied_key.clone());
        return Ok(Some(supplied_key));
    }
    if let Some(remembered_key) = lock_keys(state)?.get(provider).cloned() {
        return Ok(Some(remembered_key));
    }
    Ok(expected_env
        .and_then(|env| std::env::var(env).ok())
        .filter(|value| !value.is_empty()))
}

pub(super) fn configure_dashboard_child_environment(
    command: &mut tokio::process::Command,
    provider_key: Option<(&str, &str)>,
) {
    configure_dashboard_child_environment_from(command, std::env::vars_os(), provider_key);
}

pub(super) fn configure_dashboard_child_environment_from(
    command: &mut tokio::process::Command,
    parent_environment: impl IntoIterator<Item = (OsString, OsString)>,
    provider_key: Option<(&str, &str)>,
) {
    command.env_clear();
    for (name, value) in parent_environment {
        if dashboard_child_environment_variable_allowed(&name) {
            command.env(name, value);
        }
    }

    if let Some((name, value)) = provider_key {
        command.env(name, value);
    }
}

/// Variables every platform must forward to a spawned job.
///
/// Unlike poppler or ffmpeg, this child is BookForge itself doing the actual
/// provider calls, so withholding network configuration does not harden
/// anything — it just breaks the run. `reqwest` reads the proxy variables
/// (in both cases) and some distributions rely on the `SSL_CERT_*` pair to
/// locate a trust store; drop them and a dashboard-launched job fails to
/// reach the provider on exactly the machines where the CLI works, which is
/// a miserable thing to debug. `RUST_LOG` is kept so a job launched from the
/// dashboard can be traced like any other.
const DASHBOARD_CHILD_NETWORK_VARIABLES: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "RUST_LOG",
];

fn dashboard_child_environment_variable_allowed(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();

    // Proxy variables are conventionally read in either case, so match both.
    if DASHBOARD_CHILD_NETWORK_VARIABLES
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
    {
        return true;
    }

    // Exactly one of the following blocks compiles, so each is written as the
    // function's trailing expression rather than an early return.
    #[cfg(windows)]
    {
        [
            "PATH",
            "SYSTEMROOT",
            "TEMP",
            "TMP",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
        ]
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed))
    }

    #[cfg(unix)]
    {
        matches!(
            name.as_ref(),
            "PATH" | "HOME" | "LANG" | "LANGUAGE" | "TMPDIR"
        ) || name.starts_with("LC_")
    }

    #[cfg(not(any(windows, unix)))]
    {
        name == "PATH"
    }
}
