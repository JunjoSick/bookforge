use super::*;
use std::io::Write;

/// Environment variable through which a launch/retry parent hands the output
/// lock to its audiobook child. The parent pre-addresses the lock record with
/// this nonce and the child adopts it instead of racing the parent's release.
const AUDIO_LOCK_HANDOFF_ENV: &str = "BOOKFORGE_AUDIO_OUT_LOCK_HANDOFF";

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
    // Keep the operation owned across the spawn handoff. The child acquires
    // the same lock before touching the cache; holding it here closes the
    // starting-to-running gap where prune or retry could otherwise intervene.
    // Taking the kernel lock involves file I/O, so keep it off the async
    // worker like the retry endpoint does.
    let output_lock = tokio::task::spawn_blocking({
        let out_dir = out_dir.clone();
        move || bookforge_audio::acquire_audiobook_output_lock(&out_dir)
    })
    .await?
    .map_err(anyhow::Error::from)?;
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
    if let Err(error) = write_audio_process_state(
        &out_dir,
        "starting",
        None,
        None,
        auto_model,
        Some(&launch_options),
    ) {
        drop(output_lock);
        let _ = std::fs::remove_file(&input_path);
        let _ = std::fs::remove_dir_all(&out_dir);
        return Err(error.into());
    }

    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            drop(output_lock);
            let _ = std::fs::remove_file(&input_path);
            let _ = std::fs::remove_dir_all(&out_dir);
            return Err(anyhow::Error::from(error)
                .context("failed to locate audiobook executable")
                .into());
        }
    };
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
    configure_audio_child_process_group(&mut command);
    // Hand the output lock to the child before it can exist: rewrite the
    // record with a fresh nonce the child will find in its environment, so
    // its acquisition waits on the kernel lock and adopts it instead of
    // failing on (or racing) this live parent. If the record rewrite fails
    // the launch is aborted before any child exists; the kernel lock is then
    // simply released on return.
    let handoff_nonce = bookforge_audio::new_lock_handoff_nonce();
    if let Err(error) = output_lock.handoff_nonce(&handoff_nonce) {
        let detail = format!("could not hand off the audiobook output lock: {error:#}");
        drop(output_lock);
        let _ = std::fs::remove_file(&input_path);
        let _ = std::fs::remove_dir_all(&out_dir);
        return Err(anyhow::anyhow!(detail).into());
    }
    command.env(AUDIO_LOCK_HANDOFF_ENV, &handoff_nonce);

    // On spawn failure the freshly written upload (and empty operation dir)
    // must not linger as orphans; remove them before surfacing the error.
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            drop(output_lock);
            let _ = std::fs::remove_file(&input_path);
            let _ = std::fs::remove_dir_all(&out_dir);
            return Err(anyhow::Error::from(error)
                .context("failed to spawn audiobook process")
                .into());
        }
    };
    let pid = child.id();
    if let Err(error) = write_audio_process_state(
        &out_dir,
        "running",
        pid,
        None,
        auto_model,
        Some(&launch_options),
    ) {
        let detail = error.to_string();
        settle_running_state_write_failure(
            &mut child,
            &out_dir,
            pid,
            &detail,
            auto_model,
            Some(&launch_options),
        )
        .await;
        // The parent's own output lock is dropped on return, releasing the
        // kernel lock; the child can never have adopted it because the parent
        // has not released it, so there is nothing to wait on or reclaim.
        return Err(error.into());
    }
    register_audio_cancellation(
        &state,
        id.clone(),
        child,
        out_dir.clone(),
        pid,
        auto_model,
        Some(handoff_nonce),
    );
    // The child now owns the operation's lifetime; the launch slot is free.
    // Release the handoff lock only after the child has been handed a running
    // state and registered for cancellation.
    drop(output_lock);
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
    let auto_model = process
        .get("auto_model")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // SERVE-3: this PID came from disk and a server restart means the live
    // child cannot be reaped or identified with certainty — pid + executable
    // name alone would let PID reuse kill a different BookForge process. The
    // only identity we trust is the owned `Child` handle, which is gone after
    // a restart, so we NEVER signal a post-restart process. Instead the kernel
    // lock decides: if a live run still holds it, exact identity cannot be
    // proven and cancel refuses; if it is free, the recorded worker is gone
    // and the durable state is reconciled to cancelled without signalling.
    let output_lock = match bookforge_audio::acquire_audiobook_output_lock_peek(&out_dir) {
        Ok(lock) => lock,
        Err(bookforge_audio::BuildError::OutputLocked(_)) => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({"error": "a live audiobook run still owns this operation and its exact identity cannot be verified after a server restart; nothing was signalled or changed"})),
            )
                .into_response());
        }
        Err(error) => return Err(error.into()),
    };
    let wrote = write_audio_process_state_if_owner(
        &out_dir,
        "cancelled",
        pid,
        None,
        auto_model,
        None,
        &output_lock,
    )?;
    drop(output_lock);
    if !wrote {
        return Ok((
            StatusCode::CONFLICT,
            Json(json!({"error": "audiobook operation was restarted by another request; nothing was changed"})),
        )
            .into_response());
    }
    Ok(Json(json!({"ok": true, "status": "cancelled"})).into_response())
}

fn configure_audio_child_process_group(command: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // The dashboard owns the child handle, but ffmpeg is launched by that
        // child. Isolating the child lets cancellation address the complete
        // audiobook process tree with one negative-PGID signal.
        command.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = command;
    }
}

async fn terminate_audio_child_tree(child: &mut tokio::process::Child) {
    let pid = child.id();
    #[cfg(windows)]
    if let Some(pid) = pid {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .await;
    }
    #[cfg(unix)]
    if let Some(pid) = pid {
        let group = format!("-{pid}");
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", &group])
            .status()
            .await;
    }
    // A direct kill is harmless when the tree signal already succeeded and
    // covers children spawned before process-group setup was introduced.
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Fold a launch/retry failure whose `running` state write failed: the child
/// is killed and reaped and the durable state is marked failed. The caller's
/// own output lock is dropped on return, which releases the kernel lock. This
/// helper never acquires or waits on the kernel lock: the child can never
/// have adopted it because the parent has not released it, so adopting it back
/// here would block for the 30-second handoff wait on the parent's own lock.
/// Returns promptly by construction.
async fn settle_running_state_write_failure(
    child: &mut tokio::process::Child,
    out_dir: &std::path::Path,
    pid: Option<u32>,
    detail: &str,
    auto_model: bool,
    options: Option<&serde_json::Value>,
) {
    terminate_audio_child_tree(child).await;
    let _ = write_audio_process_state(out_dir, "failed", pid, Some(detail), auto_model, options);
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
    handoff_nonce: Option<String>,
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
                terminate_audio_child_tree(&mut child).await;
                ("cancelled", None)
            }
        };
        let _ = tokio::task::spawn_blocking(move || -> Result<()> {
            if let Some(expected_pid) = pid {
                // The child releases its own output lock before wait() reports
                // exit. Reacquire the kernel lock non-blockingly — a newer run
                // (retry) holds it and defers this writer — then confirm the
                // record still carries the exact handoff nonce we gave the
                // child, so a late watcher can never overwrite or clean up a
                // replacement run.
                let lock = match bookforge_audio::acquire_audiobook_output_lock_peek(&out_dir) {
                    Ok(lock) => lock,
                    Err(error) => {
                        eprintln!("[serve] audiobook terminal state deferred: {error}");
                        return Ok(());
                    }
                };
                let addressed = match (handoff_nonce.as_deref(), lock.record().ok()) {
                    (Some(expected), Some(record)) => record.nonce.as_deref() == Some(expected),
                    (None, Some(record)) => record.pid == expected_pid,
                    _ => false,
                };
                if addressed {
                    let _ = write_audio_process_state_if_owner(
                        &out_dir,
                        operation_state.0,
                        expected_pid,
                        operation_state.1.as_deref(),
                        auto_model,
                        None,
                        &lock,
                    )?;
                }
                drop(lock);
            } else {
                write_audio_process_state(
                    &out_dir,
                    operation_state.0,
                    pid,
                    operation_state.1.as_deref(),
                    auto_model,
                    None,
                )?;
            }
            Ok(())
        })
        .await;
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
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = out_dir.join(format!(
        ".process-{}-{sequence}.part.tmp",
        std::process::id()
    ));
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
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    if let Err(error) = file.write_all(&bytes).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    drop(file);
    if let Err(error) = bookforge_audio::replace_file(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    #[cfg(unix)]
    std::fs::File::open(out_dir)?.sync_all()?;
    Ok(())
}

fn write_audio_process_state_if_owner(
    out_dir: &std::path::Path,
    status: &str,
    expected_pid: u32,
    error: Option<&str>,
    auto_model: bool,
    options: Option<&serde_json::Value>,
    _lock: &bookforge_audio::AudiobookOutputLock,
) -> Result<bool> {
    let path = out_dir.join("process.json");
    let process = match std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    {
        Some(process) => process,
        None => return Ok(false),
    };
    let current_pid = process
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok());
    if current_pid != Some(expected_pid) {
        return Ok(false);
    }
    write_audio_process_state(
        out_dir,
        status,
        Some(expected_pid),
        error,
        auto_model,
        options,
    )?;
    Ok(true)
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
    let scan = tokio::task::spawn_blocking(move || -> Result<DebrisScan> {
        let out_dir = audiobook_operation_out_dir(&upload_dir, &id);
        if !out_dir.is_dir() {
            return Ok(DebrisScan::NotFound);
        }
        let _lock = match bookforge_audio::acquire_audiobook_output_lock(&out_dir) {
            Ok(lock) => lock,
            Err(bookforge_audio::BuildError::OutputLocked(_)) => {
                return Ok(DebrisScan::Running);
            }
            Err(error) => return Err(error.into()),
        };
        scan_audiobook_debris(&upload_dir, &id)
    })
    .await??;
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
        let out_dir = audiobook_operation_out_dir(&upload_dir, &id);
        if !out_dir.is_dir() {
            return Ok(PruneOutcome::NotFound);
        }
        let _lock = match bookforge_audio::acquire_audiobook_output_lock(&out_dir) {
            Ok(lock) => lock,
            Err(bookforge_audio::BuildError::OutputLocked(_)) => {
                return Ok(PruneOutcome::Running);
            }
            Err(error) => return Err(error.into()),
        };
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
// (a) an atomic filesystem claim: after the status recheck the winner renames
//     `process.json` to [`RETRY_CLAIM_FILE_NAME`] -- a single rename admits
//     exactly one worker; every loser reports 409 "retry already starting".
//     Validation-refusals rename the original back, so durable state is never
//     destroyed;
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
}

enum PreparedOutcome {
    NotFound,
    Running,
    /// A concurrent retry won the atomic claim rename; this caller loses.
    RetryStarting,
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

fn options_string(options: &serde_json::Value, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn prepare_retry_failed(upload_dir: &Path, id: &str) -> Result<PreparedOutcome> {
    let out_dir = audiobook_operation_out_dir(upload_dir, id);
    if !out_dir.is_dir() {
        return Ok(PreparedOutcome::NotFound);
    }
    let process_path = out_dir.join("process.json");
    let claim_path = out_dir.join(RETRY_CLAIM_FILE_NAME);
    let process = match std::fs::read(&process_path)
        .ok()
        .map(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or(json!({})))
    {
        Some(process) => process,
        // A missing/unreadable state file alongside a live claim temp means a
        // concurrent retry has already won the rename race.
        None if claim_path.exists() => return Ok(PreparedOutcome::RetryStarting),
        None => json!({}),
    };
    if audiobook_process_is_running(&process) {
        return Ok(PreparedOutcome::Running);
    }

    // F3(a): atomic single-winner claim AFTER the status recheck. Exactly one
    // concurrent caller's rename can succeed; every loser sees NotFound and
    // reports "already starting" without touching anything.
    if let Err(error) = std::fs::rename(&process_path, &claim_path) {
        return Ok(match error.kind() {
            std::io::ErrorKind::NotFound => PreparedOutcome::RetryStarting,
            _ => PreparedOutcome::Client(format!("retry could not be started ({error})")),
        });
    }

    match finish_prepare_retry(upload_dir, id, &process)? {
        PrepareStep::Ready(prepared) => Ok(PreparedOutcome::Ready(prepared)),
        PrepareStep::Refused(message) => {
            // Refusals must not destroy the durable state file (or strand the
            // operation in claimed limbo): put the original bytes back.
            settle_retry_claim(&out_dir);
            Ok(PreparedOutcome::Client(message))
        }
    }
}

/// Validation tail of [`prepare_retry_failed`], run only once the atomic
/// claim is held. Every refusal becomes [`PrepareStep::Refused`] so the
/// caller can restore the claim uniformly.
fn finish_prepare_retry(
    upload_dir: &Path,
    id: &str,
    process: &serde_json::Value,
) -> Result<PrepareStep> {
    let refused = |message: &str| Ok(PrepareStep::Refused(message.to_string()));
    let out_dir = audiobook_operation_out_dir(upload_dir, id);
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
        out_dir,
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
    })))
}

fn u32_checked(value: Option<&serde_json::Value>) -> Option<u32> {
    value
        .and_then(serde_json::Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

/// Resolve the atomic retry claim one way or the other:
/// - when the fresh `running` state already exists, the claim file is debris
///   and is removed;
/// - otherwise the original `process.json` bytes are renamed back, so a
///   refusal or a crash below the claim can never strand the operation in
///   claimed limbo or destroy its durable state.
fn settle_retry_claim(out_dir: &Path) {
    let claim_path = out_dir.join(RETRY_CLAIM_FILE_NAME);
    if !claim_path.exists() {
        return;
    }
    if out_dir.join("process.json").exists() {
        let _ = std::fs::remove_file(&claim_path);
    } else {
        let _ = std::fs::rename(&claim_path, out_dir.join("process.json"));
    }
}

/// Guard pair held for the whole spawn handoff of [`retry_failed_chunks`].
/// Dropping it releases the SERVE-6 launch slot *and* settles the atomic
/// claim per [`settle_retry_claim`], so every failure path — early `?`, panic
/// unwind, refused key, failed spawn — unwinds to a consistent operation
/// directory.
struct RetryClaimLaunch {
    /// Held (never read) purely so its Drop releases the launch slot.
    #[allow(dead_code)]
    slot: LaunchSlotGuard,
    out_dir: PathBuf,
}

impl Drop for RetryClaimLaunch {
    fn drop(&mut self) {
        // LaunchSlotGuard's own Drop frees the slot first (field order); the
        // claim settlement follows before this struct is fully gone.
        settle_retry_claim(&self.out_dir);
    }
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
    let retry_out_dir = audiobook_operation_out_dir(&upload_dir, &id);
    if !retry_out_dir.is_dir() {
        return Ok(debris_not_found_response());
    }
    // Serialize claim validation and the spawn handoff with the old child's
    // terminal writer. The newly spawned child takes this same lock after the
    // parent publishes its running state.
    let output_lock_result = tokio::task::spawn_blocking({
        let retry_out_dir = retry_out_dir.clone();
        move || bookforge_audio::acquire_audiobook_output_lock(&retry_out_dir)
    })
    .await?;
    let output_lock = match output_lock_result {
        Ok(lock) => lock,
        Err(bookforge_audio::BuildError::OutputLocked(_)) => {
            return Ok(debris_running_response());
        }
        Err(error) => return Err(error.into()),
    };
    let prepared =
        tokio::task::spawn_blocking(move || prepare_retry_failed(&upload_dir, &retry_id)).await??;
    let prepared = match prepared {
        PreparedOutcome::NotFound => return Ok(debris_not_found_response()),
        PreparedOutcome::Running => return Ok(debris_running_response()),
        // F3(a): a concurrent double-click lost the atomic rename race.
        PreparedOutcome::RetryStarting => {
            return Ok((
                StatusCode::CONFLICT,
                Json(json!({ "error": "retry already starting" })),
            )
                .into_response());
        }
        PreparedOutcome::Client(message) => return Ok(bad_request(&message)),
        PreparedOutcome::Ready(prepared) => prepared,
    };
    // From here on the claim must be settled no matter which way control
    // leaves this handler; the guard pairs that with the launch slot.
    let _guard = RetryClaimLaunch {
        slot,
        out_dir: prepared.out_dir.clone(),
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
    configure_audio_child_process_group(&mut command);

    // Test parity with the resume hook (state.resume_launches): installs a
    // deterministic spawn boundary so endpoint tests can drive the full
    // claim/slot/state machinery without exec'ing the test binary as an
    // audiobook child.
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

    // Hand the output lock to the retry child before it can exist, so its
    // acquisition adopts the lock instead of failing on this live parent. If
    // the record rewrite fails the launch is aborted before any child exists;
    // the parent's kernel lock is then simply released on return.
    let handoff_nonce = bookforge_audio::new_lock_handoff_nonce();
    if let Err(error) = output_lock.handoff_nonce(&handoff_nonce) {
        let detail = format!("could not hand off the audiobook output lock: {error:#}");
        let _ = write_audio_process_state(
            &prepared.out_dir,
            "failed",
            None,
            Some(&detail),
            prepared.auto_model,
            Some(&prepared.options_snapshot),
        );
        return Err(anyhow::anyhow!(detail).into());
    }
    command.env(AUDIO_LOCK_HANDOFF_ENV, &handoff_nonce);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Err(anyhow::Error::from(error)
                .context("failed to spawn audiobook retry process")
                .into());
        }
    };
    let pid = child.id();
    if let Err(error) = write_audio_process_state(
        &prepared.out_dir,
        "running",
        pid,
        None,
        prepared.auto_model,
        Some(&prepared.options_snapshot),
    ) {
        let detail = error.to_string();
        settle_running_state_write_failure(
            &mut child,
            &prepared.out_dir,
            pid,
            &detail,
            prepared.auto_model,
            Some(&prepared.options_snapshot),
        )
        .await;
        // The parent's own output lock is dropped on return, releasing the
        // kernel lock; the child can never have adopted it because the parent
        // has not released it, so there is nothing to wait on or reclaim.
        return Err(error.into());
    }
    register_audio_cancellation(
        &state,
        id.clone(),
        child,
        prepared.out_dir.clone(),
        pid,
        prepared.auto_model,
        Some(handoff_nonce),
    );
    // The child now owns the operation's lifetime and will acquire this same
    // lock before touching the cache.
    drop(output_lock);
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

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_dashboard_child_kills_and_reaps_its_process_group() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 2 & wait"]);
        configure_audio_child_process_group(&mut command);
        let mut child = command.spawn().expect("shell should spawn");

        let started = Instant::now();
        terminate_audio_child_tree(&mut child).await;

        assert!(
            started.elapsed() < Duration::from_secs(1),
            "descendant survived tree termination: {:?}",
            started.elapsed()
        );
        assert!(
            child
                .try_wait()
                .expect("child status should be readable")
                .is_some(),
            "dashboard child must be reaped"
        );
    }

    /// A launch/retry whose `running` state write failed must fold the failure
    /// promptly, never waiting on the parent's own kernel lock (an adopt-back
    /// there would block for the 30-second handoff wait). The parent's lock is
    /// then released on return and the operation is immediately re-acquirable.
    #[cfg(unix)]
    #[tokio::test]
    async fn running_state_write_failure_returns_promptly_and_frees_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let out_dir = dir.path().join("out");
        std::fs::create_dir_all(&out_dir).unwrap();

        // The failed launch's parent still holds the kernel lock.
        let parent = bookforge_audio::acquire_audiobook_output_lock(&out_dir).unwrap();
        let handoff = bookforge_audio::new_lock_handoff_nonce();
        parent.handoff_nonce(&handoff).unwrap();
        write_audio_process_state(&out_dir, "starting", None, None, false, None).unwrap();

        let mut child = tokio::process::Command::new("sh")
            .args(["-c", "sleep 2"])
            .spawn()
            .expect("child spawns");
        let pid = child.id();

        let started = Instant::now();
        settle_running_state_write_failure(
            &mut child,
            &out_dir,
            pid,
            "injected running-state write failure",
            false,
            None,
        )
        .await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the failure path must return promptly, never waiting on the parent's own kernel lock: {:?}",
            started.elapsed()
        );

        let process: serde_json::Value = serde_json::from_slice(
            &std::fs::read(out_dir.join("process.json")).expect("failed state written"),
        )
        .expect("failed state is JSON");
        assert_eq!(process["status"], "failed");

        // The parent releases the kernel lock on return; a fresh acquire
        // succeeds immediately — no orphan, no 30-second wait.
        drop(parent);
        let reacquired =
            bookforge_audio::acquire_audiobook_output_lock(&out_dir).expect("re-acquirable");
        drop(reacquired);
    }

    #[test]
    fn terminal_writer_does_not_overwrite_a_newer_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock = bookforge_audio::acquire_audiobook_output_lock(dir.path()).unwrap();
        write_audio_process_state(dir.path(), "running", Some(111), None, false, None).unwrap();
        let before = std::fs::read(dir.path().join("process.json")).unwrap();

        assert!(
            !write_audio_process_state_if_owner(
                dir.path(),
                "succeeded",
                222,
                None,
                false,
                None,
                &lock,
            )
            .unwrap()
        );
        assert_eq!(
            std::fs::read(dir.path().join("process.json")).unwrap(),
            before
        );
    }

    /// A late watcher must defer to a replacement run twice over: while the
    /// replacement child holds the kernel lock the watcher's non-blocking
    /// acquisition fails, and even after the replacement finishes, the record
    /// carries a fresh nonce so the stale watcher's nonce check refuses to
    /// touch the state.
    #[test]
    fn terminal_watcher_defers_to_a_replacement_run() {
        let dir = tempfile::tempdir().unwrap();
        let out_dir = dir.path();
        let stale_nonce = "stale-handoff-nonce-of-the-previous-child";
        write_audio_process_state(out_dir, "running", Some(111), None, false, None).unwrap();

        // A replacement run (retry) now owns the operation: the parent hands
        // a fresh nonce, releases the kernel lock, and the child adopts.
        let retry_parent = bookforge_audio::acquire_audiobook_output_lock(out_dir).unwrap();
        let retry_nonce = bookforge_audio::new_lock_handoff_nonce();
        retry_parent.handoff_nonce(&retry_nonce).unwrap();
        drop(retry_parent);
        let retry_child =
            bookforge_audio::acquire_audiobook_output_lock_with_handoff(out_dir, &retry_nonce)
                .unwrap();

        // The old watcher's non-blocking acquire must defer (held).
        assert!(
            matches!(
                bookforge_audio::acquire_audiobook_output_lock_peek(out_dir),
                Err(bookforge_audio::BuildError::OutputLocked(_))
            ),
            "a late watcher must defer while a replacement run holds the lock"
        );

        // The replacement finishes; now the watcher's peek succeeds but the
        // record carries the replacement's nonce, so it must not write.
        drop(retry_child);
        let peeked = bookforge_audio::acquire_audiobook_output_lock_peek(out_dir)
            .expect("peek succeeds once the replacement is gone");
        let addressed = peeked
            .record()
            .ok()
            .and_then(|record| record.nonce)
            .as_deref()
            == Some(stale_nonce);
        assert!(
            !addressed,
            "a stale nonce must never be considered addressed"
        );
        let before = std::fs::read(out_dir.join("process.json")).unwrap();
        assert_eq!(
            std::fs::read(out_dir.join("process.json")).unwrap(),
            before,
            "the terminal state must stay untouched by a stale watcher"
        );
        drop(peeked);
    }
}
