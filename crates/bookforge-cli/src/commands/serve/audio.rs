use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/audio/voices", get(audio_voices))
        .route("/api/audiobook/estimate", post(estimate_audiobook))
        .route("/api/audiobook", post(launch_audiobook))
        .route("/api/audiobooks", get(list_audiobooks))
        .route("/api/audiobooks/{id}", get(audiobook_status))
        .route("/api/audiobooks/{id}/cancel", post(cancel_audiobook))
        .route("/api/audiobooks/{id}/artifact", get(audiobook_artifact))
}

pub(super) struct AudiobookSource {
    pub(super) bytes: Vec<u8>,
    pub(super) file_name: String,
}

/// Resolve either a direct browser upload or the output of a finished
/// translation. The latter is addressed by job id rather than by a
/// browser-supplied filesystem path, keeping the handoff scoped to records the
/// server already trusts.
pub(super) async fn resolve_audiobook_source(
    state: &AppState,
    fields: &HashMap<String, String>,
    file_bytes: Option<Vec<u8>>,
    file_name: String,
) -> Result<AudiobookSource> {
    if let Some(bytes) = file_bytes.filter(|bytes| !bytes.is_empty()) {
        return Ok(AudiobookSource { bytes, file_name });
    }

    let source_job_id = field_value(fields, "source_job_id")
        .context("upload an EPUB file or choose a finished translation")?;
    let store_path = state.store_path.clone();
    tokio::task::spawn_blocking(move || -> Result<AudiobookSource> {
        let store = JobStore::open(store_path)?;
        let job = store
            .get_job(&source_job_id)?
            .with_context(|| format!("no translation job '{source_job_id}'"))?;
        if job.status != "succeeded" {
            anyhow::bail!("translation must be finished before it can be narrated");
        }
        let metadata = std::fs::metadata(&job.output_path).with_context(|| {
            format!(
                "translated EPUB is not available at {}",
                job.output_path.display()
            )
        })?;
        if metadata.len() > MAX_UPLOAD_BYTES as u64 {
            anyhow::bail!("translated EPUB exceeds the dashboard upload limit");
        }
        let file_name = job
            .output_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("translated.epub")
            .to_string();
        let bytes = std::fs::read(&job.output_path).with_context(|| {
            format!(
                "failed to read translated EPUB at {}",
                job.output_path.display()
            )
        })?;
        if bytes.is_empty() {
            anyhow::bail!("translated EPUB is empty");
        }
        Ok(AudiobookSource { bytes, file_name })
    })
    .await?
}

/// Plan audiobook synthesis from an uploaded EPUB or finished translation
/// without making provider requests. Parsing and chunk construction are
/// blocking, so both happen off the async worker after the source has been
/// saved to a temporary path.
async fn estimate_audiobook(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = "upload.epub".to_string();
    let mut fields = HashMap::<String, String>::new();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if let Some(value) = field.file_name().filter(|value| !value.is_empty()) {
                file_name = value.to_string();
            }
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }
    let source = match resolve_audiobook_source(&state, &fields, file_bytes, file_name).await {
        Ok(source) => source,
        Err(error) => return Ok(bad_request(&error.to_string())),
    };
    let bytes = source.bytes;

    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !matches!(
        provider.as_str(),
        "mock" | "openai" | "gemini" | "elevenlabs"
    ) {
        return Ok(bad_request("unsupported audiobook provider"));
    }
    let model = field_value(&fields, "model").unwrap_or_else(|| match provider.as_str() {
        "openai" => "gpt-4o-mini-tts".to_string(),
        "gemini" => "gemini-3.1-flash-tts-preview".to_string(),
        "elevenlabs" => String::new(),
        _ => "mock-silence".to_string(),
    });
    let max_chars = match fields.get("max_chars") {
        Some(value) => match value.trim().parse::<usize>() {
            Ok(value) => value,
            Err(_) => return Ok(bad_request("max_chars must be a positive integer")),
        },
        None => 2_000,
    };
    let provider_max_chars = audio_provider_max_chars(&provider, &model);
    if max_chars == 0 || max_chars > provider_max_chars {
        return Ok(bad_request(&format!(
            "{provider} accepts between 1 and {provider_max_chars} characters per request"
        )));
    }
    let chapter_filter = match field_value(&fields, "chapters") {
        Some(value) => match super::audiobook::parse_chapter_ranges(&value) {
            Ok(chapters) => Some(chapters),
            Err(error) => return Ok(bad_request(&format!("invalid chapters: {error}"))),
        },
        None => None,
    };

    let sequence = ESTIMATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = std::env::temp_dir().join(format!(
        "bookforge-audio-estimate-{}-{}-{sequence}.epub",
        std::process::id(),
        now_ms(),
    ));
    std::fs::write(&temp_path, bytes)?;
    let plan_path = temp_path.clone();
    let plan_result = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize)> {
        let book = bookforge_epub::read_epub(&plan_path)?;
        let options = bookforge_audio::AudiobookOptions {
            max_chars,
            chapter_filter,
            ..bookforge_audio::AudiobookOptions::default()
        };
        let plan = bookforge_audio::plan_chunks(&book, &options);
        let chapters = plan
            .iter()
            .map(|chunk| chunk.chapter_index)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        let characters = plan.iter().map(|chunk| chunk.chars).sum();
        Ok((chapters, plan.len(), characters))
    })
    .await;
    let _ = std::fs::remove_file(&temp_path);
    let result = match plan_result? {
        Ok(result) => result,
        Err(error) => {
            return Ok(bad_request(&format!(
                "could not estimate audiobook: {error}"
            )));
        }
    };
    let (chapters, chunks, characters) = result;
    let cost = crate::audio_cost::estimate_audio_cost(&provider, &model, characters);
    let mut payload = json!({
        "chapters": chapters,
        "chunks": chunks,
        "characters": characters,
        "est_cost_usd": cost.and_then(|value| value.usd),
        "est_credits": cost.and_then(|value| value.credits),
    });

    if provider == "elevenlabs"
        && let Some(api_key) = resolve_audio_provider_key(&state, "elevenlabs")?
        && let Some(subscription) = fetch_dashboard_elevenlabs_subscription(&api_key).await
    {
        let remaining = subscription
            .character_limit
            .saturating_sub(subscription.character_count);
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "quota".to_string(),
                json!({
                    "remaining": remaining,
                    "limit": subscription.character_limit,
                    "fits": characters as u128 <= remaining as u128,
                }),
            );
        }
    }

    Ok(Json(payload).into_response())
}

async fn fetch_dashboard_elevenlabs_subscription(
    api_key: &str,
) -> Option<bookforge_audio::ElevenLabsSubscription> {
    bookforge_audio::fetch_elevenlabs_subscription_with_key(
        ELEVENLABS_BASE_URL,
        api_key,
        ELEVENLABS_VOICE_TIMEOUT_SECONDS,
    )
    .await
    .ok()
}

/// Launch audiobook synthesis directly from an uploaded source or translated
/// EPUB. Progress is durable: the audio builder creates `manifest.json` before
/// its first provider request and atomically checkpoints it after every chunk.
async fn launch_audiobook(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name = "upload.epub".to_string();
    let mut fields = HashMap::<String, String>::new();
    while let Some(field) = multipart.next_field().await? {
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            if let Some(value) = field.file_name().filter(|value| !value.is_empty()) {
                file_name = value.to_string();
            }
            file_bytes = Some(field.bytes().await?.to_vec());
        } else {
            fields.insert(name, field.text().await?);
        }
    }
    let source = match resolve_audiobook_source(&state, &fields, file_bytes, file_name).await {
        Ok(source) => source,
        Err(error) => return Ok(bad_request(&error.to_string())),
    };
    let bytes = source.bytes;
    let file_name = source.file_name;

    let provider = field_value(&fields, "provider").unwrap_or_else(|| "mock".to_string());
    if !matches!(
        provider.as_str(),
        "mock" | "openai" | "gemini" | "elevenlabs"
    ) {
        return Ok(bad_request("unsupported audiobook provider"));
    }
    let model = field_value(&fields, "model").unwrap_or_else(|| match provider.as_str() {
        "openai" => "gpt-4o-mini-tts".to_string(),
        "gemini" => "gemini-3.1-flash-tts-preview".to_string(),
        "elevenlabs" => String::new(),
        _ => "mock-silence".to_string(),
    });
    let auto_model = provider == "elevenlabs" && model.is_empty();
    let voice = field_value(&fields, "voice").or_else(|| match provider.as_str() {
        "openai" => Some("alloy".to_string()),
        "gemini" => Some("Kore".to_string()),
        "mock" => Some("mock".to_string()),
        _ => None,
    });
    let Some(voice) = voice else {
        return Ok(bad_request("ElevenLabs requires a voice ID"));
    };
    let format = field_value(&fields, "format").unwrap_or_else(|| {
        if matches!(provider.as_str(), "gemini" | "mock") {
            "wav".to_string()
        } else {
            "mp3".to_string()
        }
    });
    let allowed_format = matches!(
        format.as_str(),
        "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm"
    );
    if !allowed_format {
        return Ok(bad_request("unsupported audio format"));
    }
    if provider == "mock" && format != "wav" {
        return Ok(bad_request("the mock provider supports WAV only"));
    }
    if provider == "gemini" && !matches!(format.as_str(), "wav" | "pcm") {
        return Ok(bad_request("Gemini supports WAV or PCM only"));
    }
    if provider == "elevenlabs" && matches!(format.as_str(), "aac" | "flac") {
        return Ok(bad_request("ElevenLabs supports MP3, Opus, WAV, or PCM"));
    }

    let speed = field_value(&fields, "speed")
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(1.0);
    if !speed.is_finite() || !(0.25..=4.0).contains(&speed) {
        return Ok(bad_request("speed must be between 0.25 and 4.0"));
    }
    if provider == "gemini" && (speed - 1.0).abs() > f32::EPSILON {
        return Ok(bad_request("Gemini does not expose playback-speed control"));
    }
    if provider == "elevenlabs" && model == "eleven_v3" && (speed - 1.0).abs() > f32::EPSILON {
        return Ok(bad_request(
            "eleven_v3 does not support speed control; use speed 1.0",
        ));
    }
    let instructions = field_value(&fields, "instructions");
    if provider == "elevenlabs" && instructions.is_some() {
        return Ok(bad_request(
            "ElevenLabs does not accept free-form instructions",
        ));
    }
    let max_chars = field_value(&fields, "max_chars")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2_000);
    let provider_max_chars = audio_provider_max_chars(&provider, &model);
    if max_chars == 0 || max_chars > provider_max_chars {
        if auto_model {
            return Ok(bad_request(
                "ElevenLabs Auto accepts between 1 and 10000 characters per request; pick eleven_flash_v2_5 explicitly for larger values",
            ));
        }
        return Ok(bad_request(&format!(
            "{provider} accepts between 1 and {provider_max_chars} characters per request"
        )));
    }
    let concurrency = field_value(&fields, "concurrency")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4)
        .clamp(1, 16);
    let gap_chapter_ms = match parse_audio_gap(&fields, "gap_chapter_ms") {
        Ok(value) => value,
        Err(error) => return Ok(bad_request(&error)),
    };
    let gap_title_ms = match parse_audio_gap(&fields, "gap_title_ms") {
        Ok(value) => value,
        Err(error) => return Ok(bad_request(&error)),
    };
    let seed = match field_value(&fields, "seed") {
        Some(value) => match value.parse::<u32>() {
            Ok(value) => Some(value),
            Err(_) => return Ok(bad_request("seed must be a valid unsigned 32-bit integer")),
        },
        None => None,
    };
    let language = match fields.get("language") {
        Some(value) => {
            let value = value.trim();
            if !valid_audio_language(value) {
                return Ok(bad_request(
                    "language must be a code such as it, en-US, or pt-BR",
                ));
            }
            Some(value.to_string())
        }
        None => None,
    };
    let advanced = AudiobookCommandOptions {
        gap_chapter_ms,
        gap_title_ms,
        single: truthy_field(&fields, "single"),
        loudnorm: truthy_field(&fields, "loudnorm"),
        seed,
        language,
    };
    let make_m4b = truthy_field(&fields, "m4b");
    let stitch_audio = truthy_field(&fields, "stitch") || make_m4b;
    if stitch_audio && format == "pcm" {
        return Ok(bad_request(
            "raw PCM cannot be stitched; choose a container format",
        ));
    }
    if make_m4b && !bookforge_audio::ffmpeg_available() {
        return Ok(bad_request("ffmpeg is required to create an M4B"));
    }
    let base_url = field_value(&fields, "base_url");
    if base_url
        .as_deref()
        .is_some_and(|value| !dashboard_audio_base_url_allowed(value))
    {
        return Ok(bad_request(
            "base URL must use HTTPS, except for loopback HTTP endpoints",
        ));
    }

    let key_slot = format!("audio:{provider}");
    let supplied_key = (provider != "mock")
        .then(|| field_value(&fields, "api_key"))
        .flatten();
    let key = if let Some(key) = supplied_key {
        lock_keys(&state)?.insert(key_slot.clone(), key.clone());
        Some(key)
    } else {
        resolve_audio_provider_key(&state, &provider)?
    };
    if provider != "mock"
        && key.is_none()
        && !audio_provider_env_has_key(&provider)
        && !(provider == "openai" && base_url.as_deref().is_some_and(audio_base_url_is_loopback))
    {
        return Ok(bad_request("TTS provider API key is required"));
    }

    let stem = sanitize_component(strip_epub_suffix(&file_name));
    let sequence = ESTIMATE_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = format!("{}-{sequence}-{stem}", now_ms());
    let upload_dir = state.upload_dir.clone();
    std::fs::create_dir_all(&upload_dir)?;
    let input_path = upload_dir.join(format!("audiobook-{id}.epub"));
    let out_dir = upload_dir.join(format!("audiobook-{id}"));
    std::fs::write(&input_path, bytes)?;
    let inspect_path = input_path.clone();
    if let Err(error) =
        tokio::task::spawn_blocking(move || bookforge_epub::inspect_epub(&inspect_path)).await?
    {
        let _ = std::fs::remove_file(&input_path);
        return Ok(bad_request(&format!("could not read EPUB: {error}")));
    }
    std::fs::create_dir_all(&out_dir)?;
    write_audio_process_state(&out_dir, "starting", None, None, auto_model)?;

    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    let api_key_env = (provider != "mock" && key.is_some())
        .then(|| audio_provider_key_env(&provider).expect("audio provider was validated"));
    command.args(audiobook_command_args(
        &input_path,
        &out_dir,
        &provider,
        (!auto_model).then_some(model.as_str()),
        &voice,
        &format,
        speed,
        max_chars,
        concurrency,
        instructions.as_deref(),
        base_url.as_deref(),
        make_m4b,
        stitch_audio,
        api_key_env,
        &advanced,
    ));
    configure_dashboard_child_environment(&mut command, api_key_env.zip(key.as_deref()));

    let mut child = command
        .spawn()
        .context("failed to spawn audiobook process")?;
    let pid = child.id();
    write_audio_process_state(&out_dir, "running", pid, None, auto_model)?;
    let cancel = tokio_util::sync::CancellationToken::new();
    state
        .audio_cancels
        .lock()
        .map_err(|_| anyhow::anyhow!("audiobook cancellation registry is unavailable"))?
        .insert(id.clone(), cancel.clone());
    let monitor_dir = out_dir.clone();
    let monitor_id = id.clone();
    let cancel_registry = state.audio_cancels.clone();
    tokio::spawn(async move {
        let operation_state = tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => ("succeeded", None),
                Ok(status) => ("failed", Some(format!("audiobook process exited with {status}"))),
                Err(error) => ("failed", Some(format!("could not wait for audiobook process: {error}"))),
            },
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                ("cancelled", None)
            }
        };
        let _ = write_audio_process_state(
            &monitor_dir,
            operation_state.0,
            pid,
            operation_state.1.as_deref(),
            auto_model,
        );
        if let Ok(mut registry) = cancel_registry.lock() {
            registry.remove(&monitor_id);
        }
    });

    Ok(Json(json!({
        "ok": true,
        "id": id,
        "input_path": input_path.display().to_string(),
        "out_dir": out_dir.display().to_string(),
        "provider": provider,
        "model": model,
        "voice": voice,
        "pid": pid,
    }))
    .into_response())
}

#[derive(Debug, Default)]
pub(super) struct AudiobookCommandOptions {
    pub(super) gap_chapter_ms: Option<u32>,
    pub(super) gap_title_ms: Option<u32>,
    pub(super) single: bool,
    pub(super) loudnorm: bool,
    pub(super) seed: Option<u32>,
    pub(super) language: Option<String>,
}

pub(super) fn clamp_audio_gap(value: u32) -> u32 {
    value.min(10_000)
}

fn parse_audio_gap(
    fields: &HashMap<String, String>,
    name: &str,
) -> std::result::Result<Option<u32>, String> {
    let Some(value) = fields.get(name) else {
        return Ok(None);
    };
    let value = value
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("{name} must be a non-negative integer"))?;
    Ok(Some(clamp_audio_gap(value)))
}

pub(super) fn valid_audio_language(value: &str) -> bool {
    let mut parts = value.split('-');
    let Some(primary) = parts.next() else {
        return false;
    };
    if !(2..=3).contains(&primary.len()) || !primary.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return false;
    }
    parts.all(|part| {
        (2..=8).contains(&part.len()) && part.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn audiobook_command_args(
    input_path: &Path,
    out_dir: &Path,
    provider: &str,
    model: Option<&str>,
    voice: &str,
    format: &str,
    speed: f32,
    max_chars: usize,
    concurrency: usize,
    instructions: Option<&str>,
    base_url: Option<&str>,
    make_m4b: bool,
    stitch_audio: bool,
    api_key_env: Option<&str>,
    advanced: &AudiobookCommandOptions,
) -> Vec<OsString> {
    let mut args = vec![
        "audiobook".into(),
        input_path.as_os_str().to_owned(),
        "--out".into(),
        out_dir.as_os_str().to_owned(),
        "--provider".into(),
        provider.into(),
    ];
    if let Some(model) = model.filter(|value| !value.is_empty()) {
        args.extend([OsString::from("--model"), model.into()]);
    }
    args.extend([
        "--voice".into(),
        voice.into(),
        "--format".into(),
        format.into(),
        "--speed".into(),
        speed.to_string().into(),
        "--max-chars".into(),
        max_chars.to_string().into(),
        "--concurrency".into(),
        concurrency.to_string().into(),
        "--ui".into(),
        "quiet".into(),
    ]);
    if let Some(instructions) = instructions {
        args.extend([OsString::from("--instructions"), instructions.into()]);
    }
    if let Some(base_url) = base_url {
        args.extend([OsString::from("--base-url"), base_url.into()]);
    }
    if make_m4b {
        args.push("--m4b".into());
    } else if stitch_audio {
        args.push("--stitch".into());
    }
    if let Some(env) = api_key_env {
        args.extend([OsString::from("--api-key-env"), env.into()]);
    }
    if let Some(gap) = advanced.gap_chapter_ms {
        args.extend([OsString::from("--gap-chapter-ms"), gap.to_string().into()]);
    }
    if let Some(gap) = advanced.gap_title_ms {
        args.extend([OsString::from("--gap-title-ms"), gap.to_string().into()]);
    }
    if advanced.single {
        args.push("--single".into());
    }
    if advanced.loudnorm {
        args.push("--loudnorm".into());
    }
    if let Some(seed) = advanced.seed {
        args.extend([OsString::from("--seed"), seed.to_string().into()]);
    }
    if let Some(language) = advanced.language.as_deref() {
        args.extend([OsString::from("--language"), language.into()]);
    }
    args
}

pub(super) fn resolved_model_from_synthesis_id(synthesis_id: &str) -> Option<&str> {
    let (_, model) = synthesis_id.rsplit_once(':')?;
    (!model.is_empty()).then_some(model)
}

async fn list_audiobooks(
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, AppError> {
    let upload_dir = state.upload_dir.clone();
    let items =
        tokio::task::spawn_blocking(move || list_audiobook_summaries(&upload_dir)).await??;
    Ok(Json(items))
}

fn list_audiobook_summaries(upload_dir: &Path) -> Result<Vec<serde_json::Value>> {
    let entries = match std::fs::read_dir(upload_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut items = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_prefix("audiobook-"))
            .filter(|id| valid_audiobook_id(id))
        else {
            continue;
        };
        let out_dir = entry.path();
        if !out_dir.join("manifest.json").is_file() && !out_dir.join("process.json").is_file() {
            continue;
        }
        if let Some(payload) = read_audiobook_payload(upload_dir, id) {
            let process_updated = payload
                .get("process")
                .and_then(|process| process.get("updated_at_ms"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let manifest_updated = payload
                .get("updated_at_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            items.push(json!({
                "id": payload.get("id"),
                "title": payload.get("title"),
                "status": payload.get("status"),
                "process_status": payload
                    .get("process")
                    .and_then(|process| process.get("status")),
                "input_path": payload.get("input_path"),
                "out_dir": payload.get("out_dir"),
                "synthesis_id": payload.get("synthesis_id"),
                "voice": payload.get("voice"),
                "chapters": payload.get("chapters"),
                "completed_chunks": payload.get("completed_chunks"),
                "total_chunks": payload
                    .get("chunks")
                    .and_then(serde_json::Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0),
                "artifact": payload.get("artifact"),
                "warnings": payload.get("warnings"),
                "updated_at_ms": manifest_updated.max(process_updated),
            }));
        }
    }
    items.sort_by_key(|item| {
        std::cmp::Reverse(
            item.get("updated_at_ms")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
        )
    });
    Ok(items)
}

fn audiobook_warnings(process: &serde_json::Value) -> Vec<serde_json::Value> {
    match process.get("warnings") {
        Some(serde_json::Value::Array(warnings)) => warnings.clone(),
        Some(serde_json::Value::String(warning)) if !warning.is_empty() => {
            vec![json!(warning)]
        }
        Some(serde_json::Value::Object(warning)) if !warning.is_empty() => {
            vec![serde_json::Value::Object(warning.clone())]
        }
        _ => Vec::new(),
    }
}

fn read_audiobook_payload(upload_dir: &Path, id: &str) -> Option<serde_json::Value> {
    let out_dir = upload_dir.join(format!("audiobook-{id}"));
    if !out_dir.is_dir() {
        return None;
    }
    let process: serde_json::Value = std::fs::read(out_dir.join("process.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({"status": "starting"}));
    let mut payload: serde_json::Value = std::fs::read(out_dir.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| {
            json!({
                "status": "starting",
                "completed_chunks": 0,
                "chunks": [],
            })
        });
    let resolved_model = payload
        .get("synthesis_id")
        .and_then(serde_json::Value::as_str)
        .and_then(resolved_model_from_synthesis_id)
        .map(str::to_string);
    let warnings = audiobook_warnings(&process);
    let object = payload.as_object_mut()?;
    object.insert("id".to_string(), json!(id));
    object.insert(
        "input_path".to_string(),
        json!(
            upload_dir
                .join(format!("audiobook-{id}.epub"))
                .display()
                .to_string()
        ),
    );
    object.insert("out_dir".to_string(), json!(out_dir.display().to_string()));
    object.insert("process".to_string(), process.clone());
    object.insert("warnings".to_string(), json!(warnings));
    object.insert("resolved_model".to_string(), json!(resolved_model));
    match process.get("status").and_then(serde_json::Value::as_str) {
        Some("succeeded") => {
            object.insert("status".to_string(), json!("succeeded"));
        }
        Some("failed") => {
            object.insert("status".to_string(), json!("failed"));
            object.insert(
                "error".to_string(),
                process
                    .get("error")
                    .cloned()
                    .unwrap_or_else(|| json!("audiobook process failed")),
            );
        }
        Some("cancelled") => {
            object.insert("status".to_string(), json!("cancelled"));
        }
        _ => {}
    }
    if out_dir.join("audiobook.m4b").is_file() {
        object.insert(
            "artifact".to_string(),
            json!(out_dir.join("audiobook.m4b").display().to_string()),
        );
    }
    Some(payload)
}

async fn audiobook_status(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    match read_audiobook_payload(&state.upload_dir, &id) {
        Some(payload) => Ok(Json(payload).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no such audiobook operation"})),
        )
            .into_response()),
    }
}

async fn cancel_audiobook(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    let cancel = state
        .audio_cancels
        .lock()
        .map_err(|_| anyhow::anyhow!("audiobook cancellation registry is unavailable"))?
        .get(&id)
        .cloned();
    if let Some(cancel) = cancel {
        cancel.cancel();
        return Ok(Json(json!({"ok": true, "status": "cancelling"})).into_response());
    }

    let out_dir = state.upload_dir.join(format!("audiobook-{id}"));
    let process: serde_json::Value = match std::fs::read(out_dir.join("process.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(process) => process,
        None => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({"error": "audiobook operation has no running process"})),
            )
                .into_response());
        }
    };
    if !matches!(
        process.get("status").and_then(serde_json::Value::as_str),
        Some("starting" | "running")
    ) {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "audiobook operation is not running"})),
        )
            .into_response());
    }
    let Some(pid) = process
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
    else {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "audiobook operation has no running process"})),
        )
            .into_response());
    };
    if let Err(error) = terminate_restarted_audiobook(&state, pid).await {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": format!("could not cancel audiobook process: {error}")})),
        )
            .into_response());
    }
    let auto_model = process
        .get("auto_model")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    write_audio_process_state(&out_dir, "cancelled", Some(pid), None, auto_model)?;
    Ok(Json(json!({"ok": true, "status": "cancelled"})).into_response())
}

async fn terminate_restarted_audiobook(state: &AppState, pid: u32) -> Result<()> {
    #[cfg(test)]
    if let Some(cancelled) = &state.audio_restart_cancels {
        cancelled
            .lock()
            .map_err(|_| anyhow::anyhow!("test cancellation recorder is unavailable"))?
            .push(pid);
        return Ok(());
    }
    #[cfg(not(test))]
    let _ = state;

    #[cfg(windows)]
    let status = tokio::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .status()
        .await
        .context("failed to run taskkill")?;
    #[cfg(unix)]
    let status = tokio::process::Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .await
        .context("failed to run kill")?;
    #[cfg(not(any(windows, unix)))]
    anyhow::bail!("process cancellation is unsupported on this platform");

    if !status.success() {
        anyhow::bail!("process signalling exited with {status}");
    }
    Ok(())
}

#[derive(Deserialize)]
struct ArtifactQuery {
    disposition: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ArtifactByteRange {
    Full,
    Partial { start: u64, end: u64 },
    Unsatisfiable,
}

fn parse_artifact_range(headers: &HeaderMap, length: u64) -> ArtifactByteRange {
    let Some(value) = headers.get(axum::http::header::RANGE) else {
        return ArtifactByteRange::Full;
    };
    let Ok(value) = value.to_str() else {
        return ArtifactByteRange::Unsatisfiable;
    };
    let Some(spec) = value.trim().strip_prefix("bytes=") else {
        return ArtifactByteRange::Unsatisfiable;
    };
    if spec.contains(',') {
        return ArtifactByteRange::Unsatisfiable;
    }
    let Some((start, end)) = spec.split_once('-') else {
        return ArtifactByteRange::Unsatisfiable;
    };
    if length == 0 {
        return ArtifactByteRange::Unsatisfiable;
    }
    if start.is_empty() {
        let Ok(suffix_length) = end.parse::<u64>() else {
            return ArtifactByteRange::Unsatisfiable;
        };
        if suffix_length == 0 {
            return ArtifactByteRange::Unsatisfiable;
        }
        let start = length.saturating_sub(suffix_length.min(length));
        return ArtifactByteRange::Partial {
            start,
            end: length - 1,
        };
    }
    let Ok(start) = start.parse::<u64>() else {
        return ArtifactByteRange::Unsatisfiable;
    };
    if start >= length {
        return ArtifactByteRange::Unsatisfiable;
    }
    let end = if end.is_empty() {
        length - 1
    } else {
        let Ok(end) = end.parse::<u64>() else {
            return ArtifactByteRange::Unsatisfiable;
        };
        end.min(length - 1)
    };
    if end < start {
        return ArtifactByteRange::Unsatisfiable;
    }
    ArtifactByteRange::Partial { start, end }
}

async fn audiobook_artifact(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Query(query): Query<ArtifactQuery>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    let out_dir = state.upload_dir.join(format!("audiobook-{id}"));
    let m4b_path = out_dir.join("audiobook.m4b");
    let (path, content_type, download_name) = if m4b_path.is_file() {
        (m4b_path, "audio/mp4", "audiobook.m4b")
    } else {
        let archive_dir = out_dir.clone();
        let archive =
            tokio::task::spawn_blocking(move || ensure_audio_download_zip(&archive_dir)).await??;
        (archive, "application/zip", "audiobook-audio.zip")
    };
    let Ok(mut file) = tokio::fs::File::open(&path).await else {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "audiobook artifact is not available"})),
        )
            .into_response());
    };
    let inline = query.disposition.as_deref() == Some("inline");
    let content_disposition = match (inline, download_name.ends_with(".m4b")) {
        (true, true) => "inline; filename=\"audiobook.m4b\"",
        (true, false) => "inline; filename=\"audiobook-audio.zip\"",
        (false, true) => "attachment; filename=\"audiobook.m4b\"",
        (false, false) => "attachment; filename=\"audiobook-audio.zip\"",
    };
    let length = file.metadata().await?.len();
    let requested = parse_artifact_range(&headers, length);
    if requested == ArtifactByteRange::Unsatisfiable {
        return Ok(Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(axum::http::header::ACCEPT_RANGES, "bytes")
            .header(
                axum::http::header::CONTENT_RANGE,
                format!("bytes */{length}"),
            )
            .body(axum::body::Body::empty())
            .context("failed to build range error response")?);
    }

    let mut builder = Response::builder()
        .header(axum::http::header::CONTENT_TYPE, content_type)
        .header(axum::http::header::CONTENT_DISPOSITION, content_disposition)
        .header(axum::http::header::ACCEPT_RANGES, "bytes");
    match requested {
        ArtifactByteRange::Full => Ok(builder
            .header(axum::http::header::CONTENT_LENGTH, length)
            .body(axum::body::Body::from_stream(
                tokio_util::io::ReaderStream::new(file),
            ))
            .context("failed to build artifact response")?),
        ArtifactByteRange::Partial { start, end } => {
            use tokio::io::{AsyncReadExt, AsyncSeekExt};

            file.seek(std::io::SeekFrom::Start(start)).await?;
            let partial_length = end - start + 1;
            builder = builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(axum::http::header::CONTENT_LENGTH, partial_length)
                .header(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{length}"),
                );
            Ok(builder
                .body(axum::body::Body::from_stream(
                    tokio_util::io::ReaderStream::new(file.take(partial_length)),
                ))
                .context("failed to build partial artifact response")?)
        }
        ArtifactByteRange::Unsatisfiable => unreachable!("handled above"),
    }
}

fn ensure_audio_download_zip(out_dir: &std::path::Path) -> Result<PathBuf> {
    use std::io::{Read, Write};

    let archive_path = out_dir.join("audiobook-audio.zip");
    if archive_path.is_file() {
        return Ok(archive_path);
    }
    let manifest_path = out_dir.join("manifest.json");
    let manifest: bookforge_audio::AudiobookManifest = serde_json::from_slice(
        &std::fs::read(&manifest_path)
            .with_context(|| format!("failed to read {}", manifest_path.display()))?,
    )?;
    let staged = out_dir.join("audiobook-audio.zip.tmp");
    let _ = std::fs::remove_file(&staged);
    let file = std::fs::File::create(&staged)?;
    let mut archive = zip::ZipWriter::new(file);
    let options =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    archive.start_file("manifest.json", options)?;
    archive.write_all(&std::fs::read(&manifest_path)?)?;
    for chunk in &manifest.chunks {
        let name = std::path::Path::new(&chunk.file);
        if name.file_name().and_then(|value| value.to_str()) != Some(chunk.file.as_str()) {
            anyhow::bail!("unsafe audio filename in manifest");
        }
        let source = out_dir.join(&chunk.file);
        let mut source_file = std::fs::File::open(&source)
            .with_context(|| format!("failed to read audio part {}", source.display()))?;
        archive.start_file(&chunk.file, options)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = source_file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            archive.write_all(&buffer[..read])?;
        }
    }
    archive.finish()?;
    if let Err(error) = std::fs::rename(&staged, &archive_path) {
        let _ = std::fs::remove_file(&staged);
        if !archive_path.is_file() {
            return Err(error.into());
        }
    }
    Ok(archive_path)
}

pub(super) fn valid_audiobook_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 160
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn write_audio_process_state(
    out_dir: &std::path::Path,
    status: &str,
    pid: Option<u32>,
    error: Option<&str>,
    auto_model: bool,
) -> Result<()> {
    let path = out_dir.join("process.json");
    let temp = out_dir.join("process.part.tmp");
    let warnings = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|process| process.get("warnings").cloned());
    let mut process = json!({
        "status": status,
        "pid": pid,
        "error": error,
        "auto_model": auto_model,
        "updated_at_ms": now_ms(),
    });
    if let (Some(object), Some(warnings)) = (process.as_object_mut(), warnings) {
        object.insert("warnings".to_string(), warnings);
    }
    let bytes = serde_json::to_vec_pretty(&process)?;
    std::fs::write(&temp, bytes)?;
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    std::fs::rename(temp, path)?;
    Ok(())
}

#[derive(Deserialize)]
struct AudioVoicesQuery {
    provider: Option<String>,
}

async fn audio_voices(
    State(state): State<AppState>,
    Query(query): Query<AudioVoicesQuery>,
) -> Result<Response, AppError> {
    if query.provider.as_deref() != Some("elevenlabs") {
        return Ok(bad_request(
            "voice listing is available for ElevenLabs only",
        ));
    }

    let Some(api_key) = resolve_audio_provider_key(&state, "elevenlabs")? else {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({ "error": "ElevenLabs API key is not configured" })),
        )
            .into_response());
    };

    if let Some(voices) = state
        .elevenlabs_voices
        .lock()
        .map_err(|_| anyhow::anyhow!("ElevenLabs voice cache is unavailable"))?
        .as_ref()
        .filter(|cache| cache.fetched_at.elapsed() < ELEVENLABS_VOICE_CACHE_TTL)
        .map(|cache| cache.voices.clone())
    {
        return Ok(Json(json!({ "voices": voices })).into_response());
    }

    let voices = match bookforge_audio::list_elevenlabs_voices(
        ELEVENLABS_BASE_URL,
        &api_key,
        ELEVENLABS_VOICE_TIMEOUT_SECONDS,
    )
    .await
    {
        Ok(voices) => voices,
        Err(_) => {
            return Ok((
                StatusCode::BAD_GATEWAY,
                Json(json!({ "error": "could not load ElevenLabs voices" })),
            )
                .into_response());
        }
    };
    *state
        .elevenlabs_voices
        .lock()
        .map_err(|_| anyhow::anyhow!("ElevenLabs voice cache is unavailable"))? =
        Some(ElevenLabsVoiceCache {
            fetched_at: Instant::now(),
            voices: voices.clone(),
        });

    Ok(Json(json!({ "voices": voices })).into_response())
}

fn audio_provider_key_env(provider: &str) -> Option<&'static str> {
    AUDIO_PROVIDER_KEY_ENVS
        .iter()
        .find_map(|(known, env)| (*known == provider).then_some(*env))
}

fn audio_provider_env_has_key(provider: &str) -> bool {
    audio_provider_key_env(provider)
        .and_then(|env| std::env::var(env).ok())
        .is_some_and(|value| !value.is_empty())
}

fn resolve_audio_provider_key(state: &AppState, provider: &str) -> Result<Option<String>> {
    let key_slot = format!("audio:{provider}");
    if let Some(key) = lock_keys(state)?.get(&key_slot).cloned() {
        return Ok(Some(key));
    }
    Ok(audio_provider_key_env(provider)
        .and_then(|env| std::env::var(env).ok())
        .filter(|value| !value.is_empty()))
}

fn audio_base_url_is_loopback(value: &str) -> bool {
    reqwest::Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1"))
}

fn dashboard_audio_base_url_allowed(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| {
        url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && (url.scheme() == "https"
                || (url.scheme() == "http" && audio_base_url_is_loopback(value)))
    })
}

pub(super) fn audio_provider_max_chars(provider: &str, model: &str) -> usize {
    match provider {
        "mock" => 40_000,
        "elevenlabs" if model.is_empty() => 10_000,
        "elevenlabs" => bookforge_audio::elevenlabs_model_max_input_chars(model),
        "openai" | "gemini" => 4_096,
        _ => 4_096,
    }
}

fn truthy_field(fields: &HashMap<String, String>, key: &str) -> bool {
    field_value(fields, key)
        .is_some_and(|value| matches!(value.as_str(), "true" | "on" | "1" | "yes"))
}
