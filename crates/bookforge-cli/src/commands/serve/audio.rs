use super::*;

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/audio/voices", get(audio_voices))
        .route("/api/audiobook/estimate", post(estimate_audiobook))
        .route("/api/audiobook", post(launch_audiobook))
        .route("/api/audiobooks", get(list_audiobooks))
        .route("/api/audiobooks/{id}", get(audiobook_status))
        .route("/api/audiobooks/{id}/cancel", post(cancel_audiobook))
        .route(
            "/api/audiobooks/{id}/prune-preview",
            get(audiobook_prune_preview),
        )
        .route("/api/audiobooks/{id}/prune", post(prune_audiobook))
        .route(
            "/api/audiobooks/{id}/retry-failed",
            post(retry_failed_chunks),
        )
        .route("/api/audiobooks/{id}/artifact", get(audiobook_artifact))
}

pub(super) struct AudiobookSource {
    pub(super) bytes: Vec<u8>,
    pub(super) file_name: String,
}

/// SERVE-7 containment boundary for parsing untrusted EPUBs inside the
/// key-holding serve process.
///
/// Why a child process like the translation path uses was *not* implemented:
/// true isolation would mean planning chunks via the `audiobook` CLI, but that
/// subcommand exists only in full-synthesis form (it demands provider/voice/
/// format flags and would synthesize audio), and threading a plan-only mode
/// through `audiobook.rs` belongs to another crate's workstream. What is done
/// instead, cheaply and locally:
/// - the parse runs on a blocking worker thread rather than an async task;
/// - a panic is caught at this boundary and becomes a 4xx-style refusal, so
///   hostile input can unwind the parse but never abort the server or leak
///   beyond the request; the private temp dir (SERVE-5) still cleans up
///   because it drops during the same unwind;
/// - memory stays bounded by construction: the EPUB reader's archive budgets
///   (H-2 fix upstream in bookforge-epub) cap decompression and entity sizes,
///   so the parser cannot balloon RSS unboundedly before this guard runs.
fn contain_epub_parser_panics<T>(
    parse: impl FnOnce() -> Result<T> + std::panic::UnwindSafe,
) -> Result<T> {
    match std::panic::catch_unwind(parse) {
        Ok(result) => result,
        Err(panic) => {
            let detail = panic
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "parser panicked".to_string());
            Err(anyhow::anyhow!(
                "this EPUB could not be read safely ({detail}); re-save or re-download the file"
            ))
        }
    }
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
/// blocking, so both happen off the async worker inside a per-request private
/// temp directory (SERVE-5) that is deleted on drop regardless of outcome,
/// panics included.
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

    let plan_result = tokio::task::spawn_blocking(move || -> Result<(usize, usize, usize)> {
        let temp = PrivateTempDir::create().context("failed to create a private temp directory")?;
        let plan_path = temp.path.join("book.epub");
        write_private_file(&plan_path, &bytes)?;
        let scratch_dir = temp.path.clone();
        contain_epub_parser_panics(move || {
            // AUDIO-7 parity: estimates preprocess through the exact launcher
            // pipeline (PDF-cleanup reflow -> read -> PDF page grouping), so
            // PDF-derived books stop quoting different chapter/chunk counts
            // than the launches they precede. Also lets the panic guard clean
            // up any staged EPUB the shared pipeline leaves behind.
            let narration = bookforge_audio::read_narration_source(&plan_path, &scratch_dir)?;
            let options = bookforge_audio::AudiobookOptions {
                max_chars,
                chapter_filter,
                pdf_page_grouping: narration.pdf_page_grouping,
                ..bookforge_audio::AudiobookOptions::default()
            };
            let plan = bookforge_audio::plan_chunks(&narration.book, &options);
            let chapters = plan
                .iter()
                .map(|chunk| chunk.chapter_index)
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            let characters = plan.iter().map(|chunk| chunk.chars).sum();
            Ok((chapters, plan.len(), characters))
        })
    })
    .await;
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
        // AUDIO-17: metadata preflights ride the cancellation-safe library
        // twins. These transient routes have no job id yet (and their handler
        // futures are aborted when the browser disconnects), so the request
        // passes its own token through — job-owned tokens slot into the same
        // seam.
        && let Some(subscription) =
            fetch_dashboard_elevenlabs_subscription(
                &api_key,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
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
    cancel_token: tokio_util::sync::CancellationToken,
) -> Option<bookforge_audio::ElevenLabsSubscription> {
    bookforge_audio::fetch_elevenlabs_subscription_with_key_and_cancel(
        ELEVENLABS_BASE_URL,
        api_key,
        ELEVENLABS_VOICE_TIMEOUT_SECONDS,
        cancel_token,
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
    let chapters = match field_value(&fields, "chapters") {
        Some(value) => match super::audiobook::parse_chapter_ranges(&value) {
            // A normalized 1-based range list ("1-3,7"), mirroring the CLI
            // parser's semantics before it reaches the child command line.
            Ok(chapters) => Some(format_chapter_ranges(&chapters)),
            Err(error) => return Ok(bad_request(&format!("invalid chapters: {error}"))),
        },
        None => None,
    };
    let text_normalization = match field_value(&fields, "text_normalization") {
        Some(value) => {
            let value = value.to_ascii_lowercase();
            if !matches!(value.as_str(), "auto" | "on" | "off") {
                return Ok(bad_request("text normalization must be auto, on, or off"));
            }
            let supported = bookforge_audio::feature_set_for_id(&provider)
                .is_some_and(|features| features.text_normalization);
            if !supported {
                return Ok(bad_request(
                    "--text-normalization is supported only with --provider elevenlabs",
                ));
            }
            (value != "auto").then_some(value)
        }
        None => None,
    };
    let timeout_seconds = match field_value(&fields, "timeout_seconds") {
        Some(value) => match value.trim().parse::<u64>() {
            // One floor per the CLI parser's range(1..); the ceiling keeps a
            // fat-fingered dashboard field from parking a TTS child forever.
            Ok(seconds) if (1..=86_400).contains(&seconds) => Some(seconds),
            Ok(_) => {
                return Ok(bad_request("timeout seconds must be between 1 and 86400"));
            }
            Err(_) => return Ok(bad_request("timeout seconds must be an integer")),
        },
        None => None,
    };
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
    // ASYM-1 / AUDIO-6: the launched CLI hard-fails on combinations the
    // provider capability matrix marks unsupported (seed anywhere but
    // ElevenLabs). Reject them here so a doomed child is never spawned, and
    // no upload or operation directory lingers behind the failure. The rest
    // of the matrix gaps are warn-and-drop inside the CLI child.
    if seed.is_some()
        && bookforge_audio::feature_set_for_id(&provider).is_some_and(|features| !features.seed)
    {
        return Ok(bad_request(
            "--seed is supported only with --provider elevenlabs",
        ));
    }
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
        chapters,
        text_normalization,
        timeout_seconds,
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

    // SERVE-6: bound simultaneous launches sharing remembered provider keys.
    let slot = match try_acquire_launch_slot(&state)? {
        LaunchSlot::Acquired(slot) => slot,
        LaunchSlot::Exhausted => return Ok(launch_slot_exhausted()),
    };

    let stem = sanitize_component(strip_epub_suffix(&file_name));
    let sequence = next_launch_seq();
    let id = format!("{}-{sequence}-{stem}", now_ms());
    let upload_dir = state.upload_dir.clone();
    let input_path = upload_dir.join(format!("audiobook-{id}.epub"));
    let out_dir = upload_dir.join(format!("audiobook-{id}"));
    // Directory creation plus the (up to 64 MB) input write happen off the
    // async runtime; everything created is private to the owner (H-6).
    let inspect_out_dir = out_dir.clone();
    let write_input_path = input_path.clone();
    let inspect_path = input_path.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        ensure_private_dir_under(Path::new(".bookforge"), &upload_dir)?;
        write_private_file(&write_input_path, &bytes)?;
        std::fs::create_dir_all(&inspect_out_dir)?;
        tighten_private_under(&upload_dir, &inspect_out_dir);
        Ok(())
    })
    .await??;
    // The panic boundary and the ordinary parse failure share one anyhow
    // error channel here; both mean "refuse this upload".
    let inspected = match tokio::task::spawn_blocking(move || {
        contain_epub_parser_panics(move || {
            bookforge_epub::inspect_epub(&inspect_path).map_err(|error| anyhow::anyhow!("{error}"))
        })
    })
    .await
    {
        Ok(Ok(inspection)) => Ok(inspection),
        Ok(Err(error)) => Err(error),
        Err(join_error) => Err(anyhow::Error::from(join_error)),
    };
    if let Err(error) = inspected {
        let _ = std::fs::remove_file(&input_path);
        return Ok(bad_request(&format!("could not read EPUB: {error}")));
    }
    // Relaunchable launch-shaping settings persisted next to the run so the
    // retry-failed endpoint can reproduce the exact command later without
    // browser cooperation (and without secrets: env names only).
    let launch_options = json!({
        "provider": provider.clone(),
        "model": model.clone(),
        "voice": voice.clone(),
        "format": format.clone(),
        "speed": speed,
        "max_chars": max_chars,
        "concurrency": concurrency,
        "instructions": instructions.clone(),
        "base_url": base_url.clone(),
        "gap_chapter_ms": advanced.gap_chapter_ms,
        "gap_title_ms": advanced.gap_title_ms,
        "single": advanced.single,
        "loudnorm": advanced.loudnorm,
        "seed": advanced.seed,
        "language": advanced.language.clone(),
        "chapters": advanced.chapters.clone(),
        "text_normalization": advanced.text_normalization.clone(),
        "timeout_seconds": advanced.timeout_seconds,
    });
    write_audio_process_state(
        &out_dir,
        "starting",
        None,
        None,
        auto_model,
        Some(&launch_options),
    )?;

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

    // On spawn failure the freshly written upload (and empty operation dir)
    // must not linger as orphans; remove them before surfacing the error.
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_file(&input_path);
            let _ = std::fs::remove_dir_all(&out_dir);
            return Err(anyhow::Error::from(error)
                .context("failed to spawn audiobook process")
                .into());
        }
    };
    let pid = child.id();
    write_audio_process_state(
        &out_dir,
        "running",
        pid,
        None,
        auto_model,
        Some(&launch_options),
    )?;
    register_audio_cancellation(&state, id.clone(), child, out_dir.clone(), pid, auto_model);
    // The child now owns the operation's lifetime; the launch slot is free.
    drop(slot);

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
    /// Canonical 1-based chapter ranges ("1-3,7") already validated through
    /// the shared CLI parser.
    pub(super) chapters: Option<String>,
    /// Non-auto ElevenLabs text-normalization policy ("on" / "off").
    pub(super) text_normalization: Option<String>,
    pub(super) timeout_seconds: Option<u64>,
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
    if let Some(chapters) = advanced.chapters.as_deref() {
        args.extend([OsString::from("--chapters"), chapters.into()]);
    }
    if let Some(policy) = advanced.text_normalization.as_deref() {
        args.extend([OsString::from("--text-normalization"), policy.into()]);
    }
    if let Some(seconds) = advanced.timeout_seconds {
        args.extend([
            OsString::from("--timeout-seconds"),
            seconds.to_string().into(),
        ]);
    }
    args
}

/// Compact a validated chapter set back into CLI range syntax ("1-3,7").
pub(super) fn format_chapter_ranges(chapters: &std::collections::BTreeSet<usize>) -> String {
    let mut parts = Vec::new();
    let mut iter = chapters.iter().copied();
    let mut run_start = match iter.next() {
        Some(first) => first,
        None => return String::new(),
    };
    let mut run_end = run_start;
    for next in iter {
        if next == run_end + 1 {
            run_end = next;
        } else {
            parts.push(render_chapter_run(run_start, run_end));
            run_start = next;
            run_end = next;
        }
    }
    parts.push(render_chapter_run(run_start, run_end));
    parts.join(",")
}

fn render_chapter_run(start: usize, end: usize) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}-{end}")
    }
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
    // SERVE-3: this PID came from disk and may belong to a different process
    // than when it was written — a server restart plus PID reuse would turn a
    // blind kill into killing someone else's process tree (the same reason
    // translation resumes gate on fresh runtime leases). Verify the PID is
    // alive *and* plausibly one of ours before signalling anything.
    if !live_process_is_bookforge(pid) {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "could not verify the recorded audiobook process; nothing was signalled"})),
        )
            .into_response());
    }
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
    write_audio_process_state(&out_dir, "cancelled", Some(pid), None, auto_model, None)?;
    Ok(Json(json!({"ok": true, "status": "cancelled"})).into_response())
}

/// True when `pid` names a live process whose executable plausibly belongs to
/// BookForge (SERVE-3). Verification is best-effort but *fail-closed*: any
/// platform where identity cannot be established refuses to signal.
///
/// - Linux: resolve `/proc/<pid>/exe` and compare with our own executable.
/// - Other Unix: `ps -p <pid> -o comm=` must report our executable name.
/// - Windows: `tasklist` image name must match our executable file name.
pub(super) fn live_process_is_bookforge(pid: u32) -> bool {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };

    #[cfg(target_os = "linux")]
    {
        let ours = std::fs::canonicalize(&exe);
        let theirs = std::fs::read_link(format!("/proc/{pid}/exe")).and_then(std::fs::canonicalize);
        matches!((ours, theirs), (Ok(ours), Ok(theirs)) if ours == theirs)
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let Some(expected) = exe.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let Ok(output) = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "comm="])
            .output()
        else {
            return false;
        };
        let actual = String::from_utf8_lossy(&output.stdout);
        let actual = actual.trim();
        // `comm=` may carry a full path; only the final component is stable.
        let name = actual.rsplit('/').next().unwrap_or(actual);
        !name.is_empty() && name == expected
    }

    #[cfg(windows)]
    {
        let Some(expected) = exe
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
        else {
            return false;
        };
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let line = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
        line.contains(&expected)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = exe;
        false
    }
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

/// Register a spawned audiobook child with the cancellation registry and
/// watch it to completion, durably folding its terminal state back into
/// `process.json`. Shared by the launch and retry-failed endpoints so both
/// children are cancellable identically.
fn register_audio_cancellation(
    state: &AppState,
    id: String,
    mut child: tokio::process::Child,
    out_dir: PathBuf,
    pid: Option<u32>,
    auto_model: bool,
) {
    let cancel = tokio_util::sync::CancellationToken::new();
    if let Ok(mut registry) = state.audio_cancels.lock() {
        registry.insert(id.clone(), cancel.clone());
    }
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
            &out_dir,
            operation_state.0,
            pid,
            operation_state.1.as_deref(),
            auto_model,
            None,
        );
        if let Ok(mut registry) = cancel_registry.lock() {
            registry.remove(&id);
        }
    });
}

fn write_audio_process_state(
    out_dir: &std::path::Path,
    status: &str,
    pid: Option<u32>,
    error: Option<&str>,
    auto_model: bool,
    options: Option<&serde_json::Value>,
) -> Result<()> {
    let path = out_dir.join("process.json");
    let temp = out_dir.join("process.part.tmp");
    let previous = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let warnings = previous
        .as_ref()
        .and_then(|process| process.get("warnings").cloned());
    // Launch-shaping settings ride the durable state so a relaunch (the
    // retry-failed endpoint) can reproduce the exact command without the
    // browser resending anything. A fresh launch's snapshot wins over the
    // preserved one; every rewrite preserves the newest known value.
    let effective_options = options.cloned().or_else(|| {
        previous
            .as_ref()
            .and_then(|process| process.get("options").cloned())
            .filter(serde_json::Value::is_object)
    });
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
    if let (Some(object), Some(options)) = (
        process.as_object_mut(),
        effective_options.filter(serde_json::Value::is_object),
    ) {
        object.insert("options".to_string(), options);
    }
    let bytes = serde_json::to_vec_pretty(&process)?;
    // Durability contract: a concurrent reader (e.g. another dashboard
    // process deciding whether this run is finished) must never observe a
    // missing or partial process.json. Stage to a temp file, fsync it, then
    // atomically rename over the target — `std::fs::rename` replaces on both
    // Unix and Windows — so the published file is always a complete state
    // snapshot whose bytes are already on disk before the retry claim is
    // released.
    let staged = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    staged?;
    // Make the rename itself durable once it has happened (Unix directory
    // fsync). Windows has no std directory-sync primitive; the rename is
    // already atomic there.
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
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

    // AUDIO-17: cancellation-safe voice listing. The handler's future is
    // dropped when the client disconnects; passing an explicit token keeps
    // this on the same cancellation seam as the other metadata calls.
    let voices = match bookforge_audio::list_elevenlabs_voices_with_cancel(
        ELEVENLABS_BASE_URL,
        &api_key,
        ELEVENLABS_VOICE_TIMEOUT_SECONDS,
        tokio_util::sync::CancellationToken::new(),
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

// ---------------------------------------------------------------------------
// AUDIO-6/8 remainder parity on durable operations: prune (with a dry-run
// preview first) and retry-failed relaunches of a failed run.
//
// Both operate on the SAME operation directory as the original launch, which
// is what makes chunk-reuse semantics true rather than theatrical: prune
// never deletes a file the recorded plan still references, and
// --retry-failed passes through to the CLI's cost-capped retry mode, so
// previously successful chunks are validated but can never call the provider
// again.
// ---------------------------------------------------------------------------

fn audiobook_operation_out_dir(upload_dir: &Path, id: &str) -> PathBuf {
    upload_dir.join(format!("audiobook-{id}"))
}

/// Resolve an existing operation only when its canonical path remains a direct
/// child of the canonical upload root. This rejects traversal and symlink
/// escapes before the path reaches any read or write operation.
pub(super) fn existing_audiobook_operation_out_dir(
    upload_dir: &Path,
    id: &str,
) -> Result<Option<PathBuf>> {
    if !valid_audiobook_id(id) {
        return Ok(None);
    }
    let root = match std::fs::canonicalize(upload_dir) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let candidate = root.join(format!("audiobook-{id}"));
    let resolved = match std::fs::canonicalize(&candidate) {
        Ok(resolved) => resolved,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if !resolved.starts_with(&root)
        || resolved.parent() != Some(root.as_path())
        || !resolved.is_dir()
    {
        return Ok(None);
    }
    Ok(Some(resolved))
}

/// True while the operation still owns a live child; maintenance endpoints
/// refuse to touch it until the builder exits.
fn audiobook_process_is_running(process: &serde_json::Value) -> bool {
    matches!(
        process.get("status").and_then(serde_json::Value::as_str),
        Some("starting" | "running"),
    )
}

enum DebrisScan {
    NotFound,
    Running,
    /// Safe-to-remove files plus whether the scan had to degrade to
    /// crash-debris-only matching.
    Found {
        stale: Vec<bookforge_audio::StaleChunk>,
        restricted: bool,
    },
}

/// Identify deletable files in one audiobook operation directory.
///
/// The preferred full scan feeds `find_stale_chunks` the manifest's own chunk
/// records as the kept set — exactly what a CLI `--prune` pass does. That is
/// only trustworthy when the manifest provably covers every chapter, so
/// launches record their `chapters` filter in process.json and an unknown or
/// subset-filtered manifest degrades honestly to a crash-debris-only sweep
/// instead of deleting valid reusable chunks from chapters outside the
/// original filter.
fn scan_audiobook_debris(upload_dir: &Path, id: &str) -> Result<DebrisScan> {
    let out_dir = audiobook_operation_out_dir(upload_dir, id);
    if !out_dir.is_dir() {
        return Ok(DebrisScan::NotFound);
    }
    let process = std::fs::read(out_dir.join("process.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    if audiobook_process_is_running(&process) {
        return Ok(DebrisScan::Running);
    }
    let chapters_recorded = process
        .get("options")
        .filter(|value| value.is_object())
        .and_then(|options| options.get("chapters"))
        .and_then(serde_json::Value::as_str);
    // An absent or null chapter filter proves the manifest is a whole-book
    // plan; a non-empty one (or missing options entirely, i.e. an operation
    // from before relaunch metadata existed) means we cannot vouch for it.
    let known_unfiltered = process
        .get("options")
        .filter(|value| value.is_object())
        .map(|_| chapters_recorded.is_none_or(str::is_empty))
        .unwrap_or(false);
    if known_unfiltered
        && let Ok(manifest) = serde_json::from_slice::<bookforge_audio::AudiobookManifest>(
            &std::fs::read(out_dir.join("manifest.json")).unwrap_or_default(),
        )
    {
        let stale = bookforge_audio::find_stale_chunks(&out_dir, &manifest.chunks)?;
        return Ok(DebrisScan::Found {
            stale,
            restricted: false,
        });
    }
    Ok(DebrisScan::Found {
        stale: debris_only_chunks(&out_dir)?,
        restricted: true,
    })
}

/// Crash-debris shapes only (`is_debris_name`); managed chunk files are never
/// candidates here, whatever their cache state.
fn debris_only_chunks(out_dir: &Path) -> std::io::Result<Vec<bookforge_audio::StaleChunk>> {
    let mut stale = Vec::new();
    for entry in std::fs::read_dir(out_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if bookforge_audio::cleanup::is_debris_name(name) {
            stale.push(bookforge_audio::StaleChunk {
                path: entry.path(),
                bytes: entry.metadata()?.len(),
            });
        }
    }
    Ok(stale)
}

fn debris_not_found_response() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no such audiobook operation" })),
    )
        .into_response()
}

fn debris_running_response() -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "audiobook operation is not finished" })),
    )
        .into_response()
}

fn debris_preview_response(stale: &[bookforge_audio::StaleChunk], restricted: bool) -> Response {
    let stale_files = stale.len();
    let stale_bytes = stale.iter().map(|chunk| chunk.bytes).sum::<u64>();
    Json(json!({
        "stale_files": stale_files,
        "stale_bytes": stale_bytes,
        "restricted": restricted,
    }))
    .into_response()
}

/// Dry-run prune: report how many debris/orphan files a confirm would delete,
/// without touching anything.
async fn audiobook_prune_preview(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> Result<Response, AppError> {
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    let upload_dir = state.upload_dir.clone();
    let scan =
        tokio::task::spawn_blocking(move || scan_audiobook_debris(&upload_dir, &id)).await??;
    Ok(match scan {
        DebrisScan::NotFound => debris_not_found_response(),
        DebrisScan::Running => debris_running_response(),
        DebrisScan::Found { stale, restricted } => debris_preview_response(&stale, restricted),
    })
}

enum PruneOutcome {
    NotFound,
    Running,
    Ran(serde_json::Value),
}

/// Live prune: rescan then delete in one blocking task so nothing can widen
/// the gap between what was listed and what gets removed.
async fn prune_audiobook(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }
    let upload_dir = state.upload_dir.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<PruneOutcome> {
        match scan_audiobook_debris(&upload_dir, &id)? {
            DebrisScan::NotFound => Ok(PruneOutcome::NotFound),
            DebrisScan::Running => Ok(PruneOutcome::Running),
        DebrisScan::Found { stale, restricted } => {
            let listed = stale.len();
            let (removed, freed) = bookforge_audio::remove_stale_chunks(&stale)?;
            // F7 audit trail: a confirmed prune deletes files for good.
            eprintln!(
                "[serve] audiobook prune id={id} removed={removed}/{listed} freed_bytes={freed} restricted={restricted}"
            );
            Ok(PruneOutcome::Ran(json!({
                "removed": removed,
                "freed_bytes": freed,
                "listed": listed,
                "restricted": restricted,
            })))
        }
        }
    })
    .await??;
    Ok(match outcome {
        PruneOutcome::NotFound => debris_not_found_response(),
        PruneOutcome::Running => debris_running_response(),
        PruneOutcome::Ran(payload) => Json(payload).into_response(),
    })
}

// ---------------------------------------------------------------------------
// retry-failed relaunches
//
// Double-click protection (F3): between the last "is this run finished?"
// recheck and the child actually spawning there was an unprotected window in
// which two requests could both pass validation and both spawn -- double
// provider spend. Two independent guards close it:
//
// (a) an atomic filesystem claim: after the status recheck the winner takes an
//     OS advisory lock (`flock` on Unix, `LockFileEx` on Windows) on
//     [`RETRY_CLAIM_FILE_NAME`], which admits exactly one worker on every
//     platform. A rename-based claim was NOT Windows-safe: MoveFileExW opens
//     the source by path first, so a racing loser can hold a handle to the
//     file the winner has just moved away and its rename then "succeeds" too,
//     producing two 200s. An age heuristic is not safe either: a live owner
//     that is merely suspended or slow must never be preempted, or the next
//     winner double-spends on the provider. The lock is the liveness verdict
//     — the kernel drops it exactly when the owning process dies, never before
//     — so a live owner is always refused and a dead owner's leftover claim is
//     always reclaimable. The lock file keeps a stable identity (it is never
//     unlinked by the protocol), so every contender contends on the same lock.
//     It records an ownership nonce plus a claimed-at timestamp for
//     diagnostics and a defensive release check; losers report 409 "retry
//     already starting". The winner additionally publishes durable `running`
//     state BEFORE releasing the lock, and a late arrival re-verifies that
//     durable state after acquiring the lock, so once the winner finishes its
//     handoff no second job can start;
// (b) a SERVE-6 launch slot held around preparation + spawn (released on
//     every failure path by dropping [`RetryClaimLaunch`] alongside its
//     sibling slot guard).
// ---------------------------------------------------------------------------

const RETRY_CLAIM_FILE_NAME: &str = "process.retry-claim.tmp";

#[derive(Deserialize)]
struct RetryFailedRequest {
    #[serde(default)]
    api_key: Option<String>,
}

struct PreparedRetry {
    out_dir: PathBuf,
    claim: RetryClaim,
    input_path: PathBuf,
    provider: String,
    model_opt: Option<String>,
    voice: String,
    format: String,
    speed: f32,
    max_chars: usize,
    concurrency: usize,
    instructions: Option<String>,
    base_url: Option<String>,
    advanced: AudiobookCommandOptions,
    make_m4b: bool,
    stitch_audio: bool,
    auto_model: bool,
    options_snapshot: serde_json::Value,
    failed_count: usize,
    /// The durable process.json bytes as read before this retry claimed the
    /// operation, so a failed spawn can restore the truthful prior state
    /// instead of leaving a phantom "running" marker behind.
    prior_process_json: Vec<u8>,
}

enum PreparedOutcome {
    NotFound,
    Running,
    /// A concurrent retry won the atomic claim; this caller loses.
    RetryStarting,
    /// A leftover claim file cannot be proven safe to reclaim; fail closed
    /// and point the operator at the file.
    ClaimBlocked(PathBuf),
    /// A deliberate refusal with its user-facing reason.
    Client(String),
    Ready(Box<PreparedRetry>),
}

/// Outcome of the validation tail of [`prepare_retry_failed`], after the
/// atomic claim has already been taken.
enum PrepareStep {
    Refused(String),
    Ready(Box<PreparedRetry>),
}

/// A single-winner filesystem claim guarding the retry-failed relaunch.
///
/// Ownership is an OS advisory lock held open on [`RETRY_CLAIM_FILE_NAME`]
/// for the whole handoff, not a rename and not a timestamped file:
///
/// - atomic mutual exclusion across tasks *and* independent dashboard
///   processes on Unix and Windows alike;
/// - the kernel releases the lock the instant the owner dies, so reclaiming a
///   stale claim requires acquiring the lock — positive proof the previous
///   owner is gone. A live owner (even one suspended for minutes) can never
///   be preempted, so there is no age heuristic to get wrong;
/// - the lock file keeps a stable identity: it is created once and never
///   unlinked by the protocol, so every contender contends on the same lock.
///   An unlink-then-recreate on release would let a racer that opened the old
///   file lock a dead inode while another locks a fresh one — two "winners";
/// - releasing removes/empties the claim only while the lock is still held,
///   so a late release can never affect a claim a newer owner created (no
///   check-then-delete TOCTOU).
///
/// The file also carries a diagnostic record (nonce, pid, claimed-at). The
/// record is advisory — the lock is the source of truth — but the nonce makes
/// a release idempotent and gives an operator something to read when a claim
/// stalls.
#[derive(Debug)]
pub(super) struct RetryClaim {
    nonce: String,
    claimed_at_ms: u64,
    /// The open, exclusively locked claim file. The claim is moved (never
    /// cloned) from acquisition through the handoff, so exactly one owner
    /// exists and the diagnostic record survives until the final release;
    /// dropping this handle closes it and releases the OS lock.
    file: Option<std::fs::File>,
}

/// Take a non-blocking exclusive advisory lock on `file`.
///
/// Returns `Ok(true)` when this caller now holds the lock, `Ok(false)` when a
/// live owner holds it. Both `flock` (Unix) and `LockFileEx` (Windows) are
/// advisory locks tied to the open handle: the kernel releases them the
/// moment the owning process dies, so `Ok(false)` is a liveness verdict that
/// can never go stale — no procfs probe, no PID-reuse guessing, no clock.
#[cfg(unix)]
pub(super) fn try_lock_claim(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return Ok(false);
    }
    Err(error)
}

/// Windows counterpart of [`try_lock_claim`]: `LockFileEx` on a one-byte
/// range at the front of the file. Byte-range locks (like `flock`) are
/// released when the owning handle/process goes away.
#[cfg(windows)]
pub(super) fn try_lock_claim(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_LOCK_VIOLATION, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::{
        LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx,
    };
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let handle = file.as_raw_handle() as HANDLE;
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // Lock one byte at the front. A range past the current end of file is
    // lockable, so the record does not need to exist yet.
    let acquired = unsafe {
        LockFileEx(
            handle,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if acquired != 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        return Ok(false);
    }
    Err(error)
}

/// Advisory ownership payload written inside the claim file. `pid` and
/// `claimed_at_ms` are never consulted by the protocol (the OS lock is the
/// liveness verdict); they exist so an operator can `cat` a stuck claim file
/// and see who wrote it and when.
#[derive(Deserialize)]
struct RetryClaimRecord {
    nonce: String,
    #[allow(dead_code)]
    pid: u32,
    #[allow(dead_code)]
    claimed_at_ms: u64,
}

fn parse_claim_record(bytes: &[u8]) -> Option<RetryClaimRecord> {
    let record: RetryClaimRecord = serde_json::from_slice(bytes).ok()?;
    (!record.nonce.is_empty()).then_some(record)
}

impl RetryClaim {
    fn claim_file(out_dir: &Path) -> PathBuf {
        out_dir.join(RETRY_CLAIM_FILE_NAME)
    }

    /// Wrap a freshly locked file as this caller's claim and publish the
    /// diagnostic record through it (truncating any dead owner's bytes).
    fn with_locked_file(file: std::fs::File) -> Result<Self> {
        let mut randomness = [0u8; 16];
        getrandom::fill(&mut randomness).context("failed to generate retry claim nonce")?;
        let claim = RetryClaim {
            nonce: randomness
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            claimed_at_ms: now_ms(),
            file: Some(file),
        };
        claim.write_record()?;
        Ok(claim)
    }

    /// Truncate and write the diagnostic record through the locked handle,
    /// then sync it so a reader can never observe a partial record.
    fn write_record(&self) -> Result<()> {
        use std::io::{Seek, Write};
        let mut file = self
            .file
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("retry claim is not locked"))?;
        file.set_len(0)?;
        file.seek(std::io::SeekFrom::Start(0))?;
        file.write_all(self.record().as_bytes())?;
        file.sync_all()
            .context("retry claim record could not be synced")
    }

    fn record(&self) -> String {
        serde_json::to_string(&json!({
            "nonce": self.nonce,
            "pid": std::process::id(),
            "claimed_at_ms": self.claimed_at_ms,
        }))
        .expect("retry claim record should serialize")
    }

    fn read_nonce(&self) -> Option<String> {
        use std::io::{Read, Seek};

        let mut file = self.file.as_ref()?;
        file.seek(std::io::SeekFrom::Start(0)).ok()?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).ok()?;
        parse_claim_record(&content).map(|record| record.nonce)
    }

    /// Release this caller's claim: empty and unlock the claim file, but only
    /// while this caller still owns it. The OS lock makes the ownership check
    /// sound: while it is held nobody can replace the file, so the nonce
    /// cannot change between the read and the action. Losing the check means
    /// a newer claim exists and must never be touched. The file itself keeps
    /// its stable identity (it is not unlinked), so a concurrent acquirer that
    /// already opened it contends on the same lock.
    fn release_as_nonce(&self, nonce: &str) {
        if self.read_nonce().as_deref() != Some(nonce) {
            return;
        }
        if let Some(file) = &self.file {
            use std::io::Seek;
            let mut file = file;
            let _ = file.set_len(0);
            let _ = file.seek(std::io::SeekFrom::Start(0));
            let _ = file.sync_all();
        }
    }

    pub(super) fn release(&self) {
        self.release_as_nonce(&self.nonce);
    }
}

impl Drop for RetryClaim {
    fn drop(&mut self) {
        // Runs while the locked file is still alive (the `file` field is
        // dropped only after this body), so the release is atomic under the
        // lock. The claim is single-owned, so this runs exactly once — the
        // diagnostic record stays valid for the whole handoff.
        self.release();
    }
}

#[cfg(test)]
impl RetryClaim {
    /// Exercise a foreign release through this claim's real locked handle.
    pub(super) fn release_as_nonce_for_test(&self, nonce: &str) {
        self.release_as_nonce(nonce);
    }

    /// Inspect the published record through the owning handle. Opening the
    /// locked byte range by path is expected to fail on Windows.
    pub(super) fn is_published_for_test(&self) -> bool {
        self.read_nonce().as_deref() == Some(self.nonce.as_str())
    }
}

/// Result of trying to take the single-winner retry claim for `out_dir`.
#[derive(Debug)]
pub(super) enum RetryClaimAcquire {
    /// This caller holds the OS lock and owns the relaunch.
    Owned(RetryClaim),
    /// A live owner holds the lock; this caller must lose with 409.
    HeldByLiveOwner,
    /// A leftover claim file cannot be proven safe to reclaim (unreadable
    /// record); fail closed and point the operator at the file.
    ClaimBlocked(PathBuf),
}

/// Try to take the single-winner retry claim for `out_dir`.
///
/// The OS lock is the liveness verdict: `Ok(false)` means the owner process
/// is alive (or suspended — the lock is only dropped on death), so this
/// caller loses; acquiring the lock is positive proof the previous owner is
/// gone, so the leftover claim is reclaimed by reusing the same file (stable
/// identity) and overwriting its record — never by unlink-then-recreate,
/// which would split the lock across inodes. A claim whose bytes cannot be
/// read as a record is left untouched (fail closed) for an operator to review
/// rather than destroyed blindly.
pub(super) fn acquire_retry_claim(out_dir: &Path) -> Result<RetryClaimAcquire> {
    let claim_path = RetryClaim::claim_file(out_dir);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        // Deliberately not truncating on open: the existing bytes must be read
        // (and proven to be a claim record) before they may be overwritten.
        .truncate(false)
        .open(&claim_path)
        .with_context(|| {
            format!(
                "retry claim could not be opened at {}",
                claim_path.display()
            )
        })?;
    if !try_lock_claim(&file)? {
        drop(file);
        return Ok(RetryClaimAcquire::HeldByLiveOwner);
    }

    // The lock is ours: either we just created the file, or the previous
    // owner is provably dead. Read the existing bytes through the same locked
    // handle (never a fresh path-open, whose failure must not be mistaken for
    // an empty file). An empty file is our own create-before-write debris; a
    // readable record is a dead owner's claim. Anything else — including a
    // read error, which is never treated as "empty and safe to reclaim" — is
    // unknown bytes at our reserved name and is left untouched (fail closed)
    // rather than destroyed.
    use std::io::Read;
    let mut existing = Vec::new();
    if let Err(_error) = (&file).read_to_end(&mut existing) {
        drop(file); // release the lock without touching the bytes
        return Ok(RetryClaimAcquire::ClaimBlocked(claim_path));
    }
    if !existing.is_empty() && parse_claim_record(&existing).is_none() {
        drop(file); // release the lock without touching the bytes
        return Ok(RetryClaimAcquire::ClaimBlocked(claim_path));
    }

    Ok(RetryClaimAcquire::Owned(RetryClaim::with_locked_file(
        file,
    )?))
}

fn options_string(options: &serde_json::Value, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn prepare_retry_failed(upload_dir: &Path, id: &str) -> Result<PreparedOutcome> {
    let Some(out_dir) = existing_audiobook_operation_out_dir(upload_dir, id)? else {
        return Ok(PreparedOutcome::NotFound);
    };
    let process_path = out_dir.join("process.json");
    let claim_path = out_dir.join(RETRY_CLAIM_FILE_NAME);
    let process = match std::fs::read(&process_path)
        .ok()
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or(json!({})))
    {
        Some(process) => process,
        // A missing/unreadable state file alongside a live claim temp means a
        // concurrent retry already holds the claim.
        None if claim_path.exists() => return Ok(PreparedOutcome::RetryStarting),
        None => json!({}),
    };
    if audiobook_process_is_running(&process) {
        return Ok(PreparedOutcome::Running);
    }

    // F3(a): atomic single-winner claim AFTER the status recheck. Exactly one
    // concurrent caller can take the OS lock; every loser reports "already
    // starting" without touching anything.
    let claim = match acquire_retry_claim(&out_dir)? {
        RetryClaimAcquire::Owned(claim) => claim,
        RetryClaimAcquire::HeldByLiveOwner => return Ok(PreparedOutcome::RetryStarting),
        RetryClaimAcquire::ClaimBlocked(claim_path) => {
            return Ok(PreparedOutcome::ClaimBlocked(claim_path));
        }
    };

    // Re-verify the durable state now that the claim is held. A concurrent
    // winner that finished its whole handoff only releases the lock AFTER
    // publishing durable `running` state (see the spawn ordering in
    // `retry_failed_chunks`), so a claim acquired here guarantees `running` is
    // visible — and means that winner already spent provider credits. Without
    // this recheck a late arrival could take the now-free lock and spawn a
    // duplicate run.
    let recheck = std::fs::read(&process_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    if audiobook_process_is_running(&recheck) {
        // The claim drops on return, releasing the lock.
        return Ok(PreparedOutcome::Running);
    }

    let prior_process_json = std::fs::read(&process_path).unwrap_or_default();

    // The claim is moved into `finish_prepare_retry`: on a refusal or an
    // error it is dropped (and released) inside, and on success it is moved
    // into the `PreparedRetry` so exactly one owner carries the locked handle
    // through the whole handoff. [`RetryClaim`]'s `Drop` releases the lock —
    // a refusal, an early `?`, or a panic all unwind to a clean operation
    // directory with `process.json` untouched.
    match finish_prepare_retry(
        upload_dir,
        &out_dir,
        id,
        &process,
        claim,
        prior_process_json,
    )? {
        PrepareStep::Ready(prepared) => Ok(PreparedOutcome::Ready(prepared)),
        PrepareStep::Refused(message) => Ok(PreparedOutcome::Client(message)),
    }
}

/// Validation tail of [`prepare_retry_failed`], run only once the atomic
/// claim is held (moved in by value). Every refusal becomes
/// [`PrepareStep::Refused`], dropping the claim so it unwinds uniformly via
/// [`RetryClaim`]'s `Drop`.
fn finish_prepare_retry(
    upload_dir: &Path,
    out_dir: &Path,
    id: &str,
    process: &serde_json::Value,
    claim: RetryClaim,
    prior_process_json: Vec<u8>,
) -> Result<PrepareStep> {
    let refused = |message: &str| Ok(PrepareStep::Refused(message.to_string()));
    let Some(options) = process
        .get("options")
        .filter(|value| value.is_object())
        .cloned()
    else {
        return refused(
            "relaunch settings are unavailable for this run; recreate the audiobook instead",
        );
    };

    let failed = match bookforge_audio::failed_chunk_files(&out_dir.join("manifest.json")) {
        Ok(files) => files,
        Err(error) => {
            return refused(&format!(
                "--retry-failed requires a readable prior manifest ({error})"
            ));
        }
    };
    let failed_count = failed.len();
    if failed_count == 0 {
        return refused("--retry-failed found no failed chunks matching this run");
    }

    let input_path = upload_dir.join(format!("audiobook-{id}.epub"));
    if !input_path.is_file() {
        return refused(&format!(
            "the original EPUB is no longer stored at {}; recreate the audiobook instead",
            input_path.display()
        ));
    }

    let Some(provider) = options_string(&options, "provider")
        .filter(|value| matches!(value.as_str(), "mock" | "openai" | "gemini" | "elevenlabs"))
    else {
        return refused("the recorded provider is unknown; recreate the audiobook instead");
    };
    let Some(voice) = options_string(&options, "voice").filter(|value| !value.is_empty()) else {
        return refused("the recorded voice setting is unusable; recreate the audiobook instead");
    };
    let Some(format) = options_string(&options, "format").filter(|value| {
        matches!(
            value.as_str(),
            "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm"
        )
    }) else {
        return refused("the recorded output format is unusable; recreate the audiobook instead");
    };
    let speed = options
        .get("speed")
        .and_then(serde_json::Value::as_f64)
        .unwrap_or(1.0) as f32;
    if !speed.is_finite() || !(0.25..=4.0).contains(&speed) {
        return refused("the recorded speed setting is unusable; recreate the audiobook instead");
    }
    let max_chars_u64 = options
        .get("max_chars")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(2_000);
    let provider_max_chars = audio_provider_max_chars(&provider, "") as u64;
    if max_chars_u64 == 0 || max_chars_u64 > provider_max_chars {
        return refused(
            "the recorded characters-per-request setting is unusable; recreate the audiobook instead",
        );
    }
    let max_chars = max_chars_u64.min(usize::MAX as u64) as usize;
    let concurrency = options
        .get("concurrency")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(4)
        .clamp(1, 16) as usize;

    let recorded_model = options_string(&options, "model").unwrap_or_default();
    let synthesis_model = std::fs::read(out_dir.join("manifest.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<bookforge_audio::AudiobookManifest>(&bytes).ok())
        .as_ref()
        .and_then(|manifest| resolved_model_from_synthesis_id(&manifest.synthesis_id))
        .map(str::to_string);
    let model_for_child = (!recorded_model.is_empty())
        .then_some(recorded_model)
        .or(synthesis_model);

    // The recorded values were validated when written; anything that no
    // longer parses means a tampered or foreign state file, and silently
    // widening the work would be dishonest — refuse instead.
    let advanced = AudiobookCommandOptions {
        gap_chapter_ms: u32_checked(options.get("gap_chapter_ms")),
        gap_title_ms: u32_checked(options.get("gap_title_ms")),
        single: options
            .get("single")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        loudnorm: options
            .get("loudnorm")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        seed: match options.get("seed") {
            None | Some(serde_json::Value::Null) => None,
            Some(value) => match value.as_u64().and_then(|seed| u32::try_from(seed).ok()) {
                Some(seed) => Some(seed),
                None => {
                    return refused("the recorded seed setting is unusable");
                }
            },
        },
        language: options_string(&options, "language"),
        chapters: match options.get("chapters") {
            None | Some(serde_json::Value::Null) => None,
            Some(chapters) => {
                let text = chapters.as_str().unwrap_or_default();
                if super::audiobook::parse_chapter_ranges(text).is_ok() {
                    (!text.is_empty()).then(|| text.to_string())
                } else {
                    return refused("the recorded chapter filter no longer parses");
                }
            }
        },
        text_normalization: match options.get("text_normalization") {
            None | Some(serde_json::Value::Null) => None,
            Some(policy) => match policy.as_str() {
                Some(policy @ ("on" | "off")) => Some(policy.to_string()),
                _ => {
                    return refused("the recorded text-normalization policy is unusable");
                }
            },
        },
        timeout_seconds: options_u64(options.get("timeout_seconds")),
    };

    let auto_model = process
        .get("auto_model")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let make_m4b = options
        .get("m4b")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let stitch = options
        .get("stitch")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    Ok(PrepareStep::Ready(Box::new(PreparedRetry {
        out_dir: out_dir.to_path_buf(),
        claim,
        input_path,
        model_opt: model_for_child.filter(|model| !model.is_empty()),
        provider,
        voice,
        format,
        speed,
        max_chars,
        concurrency,
        instructions: options_string(&options, "instructions"),
        base_url: options_string(&options, "base_url"),
        advanced,
        make_m4b,
        stitch_audio: make_m4b || stitch,
        auto_model,
        options_snapshot: options,
        failed_count,
        prior_process_json,
    })))
}

fn u32_checked(value: Option<&serde_json::Value>) -> Option<u32> {
    value
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

/// Guard pair held for the whole spawn handoff of [`retry_failed_chunks`].
/// Dropping it releases the SERVE-6 launch slot (via the slot guard's own
/// `Drop`) and the atomic claim (via [`RetryClaim`]'s `Drop`), so every
/// failure path — early `?`, panic unwind, refused key, failed spawn —
/// unwinds to a consistent operation directory with no live claim left
/// behind.
struct RetryClaimLaunch {
    /// Held (never read) purely so its Drop releases the launch slot.
    #[allow(dead_code)]
    slot: LaunchSlotGuard,
    /// Held (never read) purely so its Drop releases the atomic claim.
    #[allow(dead_code)]
    claim: RetryClaim,
}

fn options_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    value.and_then(serde_json::Value::as_u64)
}

/// Relaunch one finished-but-failed audiobook operation in place with the
/// CLI's `--retry-failed` flag: only chunks the prior manifest marked failed
/// may reach the provider, everything else is cache-validated.
async fn retry_failed_chunks(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    headers: HeaderMap,
    Json(req): Json<RetryFailedRequest>,
) -> Result<Response, AppError> {
    if !valid_audiobook_id(&id) {
        return Ok(bad_request("invalid audiobook operation id"));
    }
    if let Some(response) = reject_mutation(&headers, &state) {
        return Ok(response);
    }

    // F3(b): one launch slot covers preparation + the spawn handoff. On every
    // failure path below (including `?` error returns and panics) dropping
    // the guard releases the capacity again.
    let LaunchSlot::Acquired(slot) = try_acquire_launch_slot(&state)? else {
        return Ok(launch_slot_exhausted());
    };

    let upload_dir = state.upload_dir.clone();
    let retry_id = id.clone();
    let prepared =
        tokio::task::spawn_blocking(move || prepare_retry_failed(&upload_dir, &retry_id)).await??;
    let prepared = match prepared {
        PreparedOutcome::NotFound => return Ok(debris_not_found_response()),
        PreparedOutcome::Running => return Ok(debris_running_response()),
        // F3(a): a concurrent double-click lost the atomic claim race.
        PreparedOutcome::RetryStarting => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": "retry already starting" })),
            )
                .into_response());
        }
        // Fail closed with an operator recovery path: the leftover claim's
        // bytes are unknown, so we will not destroy them blindly.
        PreparedOutcome::ClaimBlocked(claim_path) => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!(
                        "a previous retry left an unreadable claim at {}; it cannot \
                         be proven safe to remove automatically. If no retry is \
                         currently starting, delete that file and try again",
                        claim_path.display()
                    )
                })),
            )
                .into_response());
        }
        PreparedOutcome::Client(message) => return Ok(bad_request(&message)),
        PreparedOutcome::Ready(prepared) => prepared,
    };
    // From here on the claim must be released no matter which way control
    // leaves this handler; the guard pairs that with the launch slot. The
    // durable `running` state is published below BEFORE this guard drops, so
    // once the lock is released no second job can start behind this one.
    let _guard = RetryClaimLaunch {
        slot,
        claim: prepared.claim,
    };

    // Key handling mirrors launch_audiobook exactly: a supplied key replaces
    // the remembered session slot, remembered beats environment, loopback
    // openai-compatible endpoints stay exempt from a paid cloud key.
    let supplied = req
        .api_key
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let key_slot = format!("audio:{}", prepared.provider);
    if let Some(key) = supplied.clone() {
        lock_keys(&state)?.insert(key_slot.clone(), key);
    }
    let key = resolve_audio_provider_key(&state, &prepared.provider)?;
    if prepared.provider != "mock"
        && key.is_none()
        && !audio_provider_env_has_key(&prepared.provider)
        && !(prepared.provider == "openai"
            && prepared
                .base_url
                .as_deref()
                .is_some_and(audio_base_url_is_loopback))
    {
        return Ok(bad_request("TTS provider API key is required"));
    }

    let api_key_env = (prepared.provider != "mock" && key.is_some())
        .then(|| audio_provider_key_env(&prepared.provider))
        .flatten();
    let exe = std::env::current_exe()?;
    let mut command = tokio::process::Command::new(exe);
    command.args(audiobook_command_args(
        &prepared.input_path,
        &prepared.out_dir,
        &prepared.provider,
        prepared.model_opt.as_deref(),
        &prepared.voice,
        &prepared.format,
        prepared.speed,
        prepared.max_chars,
        prepared.concurrency,
        prepared.instructions.as_deref(),
        prepared.base_url.as_deref(),
        prepared.make_m4b,
        prepared.stitch_audio,
        api_key_env,
        &prepared.advanced,
    ));
    command.arg("--retry-failed");
    configure_dashboard_child_environment(&mut command, api_key_env.zip(key.as_deref()));

    // Test parity with the resume hook (state.resume_launches): installs a
    // deterministic spawn boundary so endpoint tests can drive the full
    // claim/slot/state machinery without exec'ing the test binary as an
    // audiobook child. The spawn-failure variant proves the claim and the
    // slot are released when `command.spawn()` would fail, and that durable
    // state is not left as a phantom "running" marker.
    #[cfg(test)]
    if let Some(should_fail) = &state.retry_fail_spawns
        && should_fail.load(std::sync::atomic::Ordering::SeqCst)
    {
        return Err(anyhow::anyhow!("simulated audiobook spawn failure").into());
    }
    #[cfg(test)]
    if let Some(launches) = &state.retry_launches {
        launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        write_audio_process_state(
            &prepared.out_dir,
            "running",
            None,
            None,
            prepared.auto_model,
            Some(&prepared.options_snapshot),
        )?;
        eprintln!(
            "[serve] audiobook retry-failed id={id} pid=none failed_chunks={}",
            prepared.failed_count
        );
        return Ok(Json(json!({
            "ok": true,
            "id": id,
            "mode": "spawned",
            "retry_failed": true,
            "failed_chunks": prepared.failed_count,
            "pid": serde_json::Value::Null,
        }))
        .into_response());
    }

    // Durability transition: publish `running` (pid unknown yet) BEFORE the
    // child exists, so a crash anywhere past this point can never leave the
    // operation looking idle while a concurrent retry starts a second job. If
    // the spawn then fails, the prior durable bytes are restored so the
    // operation stays truthful and retryable.
    write_audio_process_state(
        &prepared.out_dir,
        "running",
        None,
        None,
        prepared.auto_model,
        Some(&prepared.options_snapshot),
    )?;
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = restore_audio_process_state(&prepared.out_dir, &prepared.prior_process_json);
            return Err(anyhow::Error::from(error)
                .context("failed to spawn audiobook retry process")
                .into());
        }
    };
    let pid = child.id();
    write_audio_process_state(
        &prepared.out_dir,
        "running",
        pid,
        None,
        prepared.auto_model,
        Some(&prepared.options_snapshot),
    )?;
    register_audio_cancellation(
        &state,
        id.clone(),
        child,
        prepared.out_dir.clone(),
        pid,
        prepared.auto_model,
    );
    // F7 audit trail: relaunches spend provider credits on retry, so record
    // the operation, pid and chunk count on the serve console.
    eprintln!(
        "[serve] audiobook retry-failed id={id} pid={pid:?} failed_chunks={}",
        prepared.failed_count
    );

    Ok(Json(json!({
        "ok": true,
        "id": id,
        "mode": "spawned",
        "retry_failed": true,
        "failed_chunks": prepared.failed_count,
        "pid": pid,
    }))
    .into_response())
}

/// Restore `process.json` to exact prior bytes, durably and atomically, after
/// a failed spawn unwound the pre-spawn "running" marker.
fn restore_audio_process_state(out_dir: &std::path::Path, prior: &[u8]) -> Result<()> {
    let path = out_dir.join("process.json");
    let temp = out_dir.join("process.part.tmp");
    let staged = (|| -> Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        std::io::Write::write_all(&mut file, prior)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, &path)?;
        Ok(())
    })();
    if staged.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    staged
}
