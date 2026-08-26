use clap::Args;
use serde_json::Value;

use bookforge_llm::{
    CompletionRequest, LlmProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    RequestMetadata, ResponseFormat,
};
use bookforge_pdf::{HttpOcrClient, OcrConfig, PopplerTools};
use bookforge_store::run_doctor;

use crate::sanitize::{sanitize_terminal, sanitize_truncated};

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Check storage health
    #[arg(long)]
    pub storage: bool,

    /// Check PDF conversion tooling
    #[arg(long)]
    pub pdf: bool,

    /// Check provider health
    #[arg(long)]
    pub provider: Option<String>,

    /// Check an OpenAI-compatible OCR endpoint.
    #[arg(long)]
    pub ocr_endpoint: Option<String>,

    /// Model to test with
    #[arg(long)]
    pub model: Option<String>,

    /// API base URL
    #[arg(long)]
    pub base_url: Option<String>,

    /// API key environment variable name
    #[arg(long)]
    pub api_key_env: Option<String>,

    /// Request timeout in seconds
    #[arg(long, default_value_t = 30)]
    pub timeout_seconds: u64,

    /// Always exit 0, even when checks report failures (for scripts that
    /// parse the output and handle exit codes themselves).
    #[arg(long, default_value_t = false)]
    pub no_fail: bool,
}

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let mut ran = false;
    let mut failed_checks = Vec::<&'static str>::new();

    if args.storage {
        ran = true;
        if !run_storage_doctor().await? {
            failed_checks.push("storage");
        }
    }

    if args.pdf {
        ran = true;
        if !run_pdf_doctor()? {
            failed_checks.push("pdf");
        }
    }

    if let Some(provider) = &args.provider {
        ran = true;
        if !run_provider_doctor(
            provider,
            args.model.as_deref(),
            args.base_url.as_deref(),
            args.api_key_env.as_deref(),
            args.timeout_seconds,
        )
        .await?
        {
            failed_checks.push("provider");
        }
    }

    if let Some(endpoint) = &args.ocr_endpoint {
        ran = true;
        if !run_ocr_doctor(
            endpoint,
            args.model.as_deref(),
            args.api_key_env.as_deref(),
            args.timeout_seconds,
        )
        .await?
        {
            failed_checks.push("ocr");
        }
    }

    if !ran && !run_storage_doctor().await? {
        failed_checks.push("storage");
    }

    // Reporting a FAILED check and then exiting 0 lies to CI (CLI-17).
    // Explicitly pass --no-fail to keep the old always-green behavior.
    evaluate_doctor_exit(&failed_checks, args.no_fail)
}

/// Shared exit policy so the rule is testable independently of live checks.
fn evaluate_doctor_exit(failed_checks: &[&str], no_fail: bool) -> anyhow::Result<()> {
    if !failed_checks.is_empty() && !no_fail {
        anyhow::bail!(
            "doctor check(s) failed: {}. Use --no-fail to keep the exit code green.",
            failed_checks.join(", ")
        );
    }
    Ok(())
}

async fn run_ocr_doctor(
    endpoint: &str,
    model: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
) -> anyhow::Result<bool> {
    let mut config = OcrConfig::new(endpoint);
    if let Some(model) = model {
        config.model = model.to_string();
    }
    if let Some(api_key_env) = api_key_env {
        config.api_key_env = api_key_env.to_string();
    }
    config.timeout_seconds = timeout_seconds;
    let display_endpoint = config.base_url.clone();
    let display_model = config.model.clone();

    println!("OCR endpoint:");
    println!("  Base URL: {display_endpoint}");
    println!("  Model: {display_model}");

    let result = tokio::task::spawn_blocking(move || {
        HttpOcrClient::new(config).and_then(|client| client.health_check())
    })
    .await
    .map_err(|error| anyhow::anyhow!("OCR health-check worker failed: {error}"))?;

    match result {
        Ok(models) => {
            println!("  Reachable: yes");
            if models.is_empty() {
                println!("  Models: (none reported)");
            } else {
                // Model ids come from the remote endpoint; strip control
                // characters before the terminal sees them (UI-5).
                let models = models
                    .iter()
                    .map(|model| sanitize_terminal(model))
                    .collect::<Vec<_>>();
                println!("  Models: {}", models.join(", "));
            }
            Ok(true)
        }
        Err(error) => {
            println!("  Reachable: no");
            println!("  Error: {}", sanitize_terminal(&error.to_string()));
            println!(
                "  Hint: OCR_API_KEY is only needed for non-loopback endpoints (or set --api-key-env to another variable)."
            );
            Ok(false)
        }
    }
}

fn run_pdf_doctor() -> anyhow::Result<bool> {
    println!("PDF conversion tooling:");
    match PopplerTools::discover() {
        Ok(tools) => {
            println!("  pdftohtml (required): {}", tools.pdftohtml.display());
            println!("  pdftotext (required): {}", tools.pdftotext.display());
            match &tools.pdfimages {
                Some(path) => println!(
                    "  pdfimages (recommended, figure preservation): {}",
                    path.display()
                ),
                None => println!("  pdfimages (recommended, figure preservation): missing"),
            }
            match &tools.pdftoppm {
                Some(path) => println!(
                    "  pdftoppm (recommended, figure/table/equation crops): {}",
                    path.display()
                ),
                None => println!("  pdftoppm (recommended, figure/table/equation crops): missing"),
            }
            if let Some(version) = tools.version() {
                println!("  version: {version}");
            }
            Ok(true)
        }
        Err(err) => {
            println!("  MISSING: {err}");
            println!();
            println!(
                "  Install poppler and add at least pdftohtml and pdftotext to PATH. pdfimages and pdftoppm are recommended for figure preservation."
            );
            Ok(false)
        }
    }
}

async fn run_storage_doctor() -> anyhow::Result<bool> {
    let doctor = run_doctor(None)?;
    let mut healthy = true;

    println!("SQLite storage:");
    if doctor.database_exists {
        println!("  database: {}", doctor.database_path.display());
        println!("  journal mode: {}", doctor.journal_mode);
        if doctor.wal_present || doctor.shm_present {
            println!(
                "  sidecars: {}{} present",
                if doctor.wal_present {
                    "jobs.sqlite-wal "
                } else {
                    ""
                },
                if doctor.shm_present {
                    "jobs.sqlite-shm"
                } else {
                    ""
                },
            );
        } else {
            println!("  sidecars: none");
        }
        println!("  integrity_check: {}", doctor.integrity_check);
        if !doctor.wal_sidecars_normal {
            println!("  WARNING: WAL sidecars are not normal");
            healthy = false;
        }
        if !doctor.note.is_empty() {
            println!();
            println!("Note:");
            println!("  {}", doctor.note);
        }
        if doctor.integrity_check != "ok" {
            println!();
            println!(
                "  WARNING: integrity check failed — consider running PRAGMA integrity_check manually"
            );
            healthy = false;
        }
    } else {
        println!("  database: {} (not found)", doctor.database_path.display());
        println!("  No storage issues to report.");
    }

    Ok(healthy)
}

async fn run_provider_doctor(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
) -> anyhow::Result<bool> {
    use bookforge_core::RetryAfterPolicy;

    println!("Provider doctor: {provider}");
    println!();

    if matches!(provider, "local-ollama" | "local-llamacpp") {
        return run_local_provider_doctor(provider, model, base_url, api_key_env, timeout_seconds)
            .await;
    }

    // 1. Determine config
    let (default_url, default_key_env, default_model) = match provider {
        "deepseek" => (
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
            "deepseek-v4-flash",
        ),
        "openrouter" => (
            "https://openrouter.ai/api/v1",
            "OPENROUTER_API_KEY",
            "openrouter/auto",
        ),
        "openai-compatible" if base_url.is_some() => (
            base_url.expect("checked above"),
            api_key_env.unwrap_or("OPENAI_API_KEY"),
            model.unwrap_or("local-model"),
        ),
        _ => {
            anyhow::bail!(
                "Provider '{provider}' is not supported for doctor checks. Use deepseek, openrouter, local-ollama, local-llamacpp, or openai-compatible with --base-url."
            );
        }
    };

    let provider_name = provider;
    let _ = provider_name; // used below in recommended preset

    let effective_url = base_url.unwrap_or(default_url);
    let effective_key_env = api_key_env.unwrap_or(default_key_env);
    let effective_model = model.unwrap_or(default_model);

    println!("  Base URL: {effective_url}");
    println!("  Model: {effective_model}");
    println!();

    // 2. Check API key
    let _api_key = match std::env::var(effective_key_env) {
        Ok(key) => {
            println!("  API key ({effective_key_env}): present");
            key
        }
        Err(_) => {
            println!("  API key ({effective_key_env}): MISSING");
            println!();
            println!(
                "  Set the environment variable {effective_key_env} before using this provider."
            );
            return Ok(false);
        }
    };

    // 3. Build provider
    let config = OpenAiCompatibleConfig {
        base_url: effective_url.to_string(),
        api_key_env: effective_key_env.to_string(),
        model: effective_model.to_string(),
        timeout_seconds,
        provider_max_attempts: 1,
        thinking_disabled: true,
        retry_after_policy: RetryAfterPolicy::None,
        max_backoff_seconds: 5,
        max_idle_per_host: 1,
        json_mode: bookforge_core::JsonMode::Auto,
    };

    let provider = match OpenAiCompatibleProvider::new_with_cancel(
        config.clone(),
        tokio_util::sync::CancellationToken::new(),
    ) {
        Ok(p) => p,
        Err(e) => {
            println!("  Provider init: FAILED ({e})");
            return Ok(false);
        }
    };

    // 4. Test tiny completion
    println!("  Sending test completion...");
    let started = std::time::Instant::now();
    let result = provider
        .complete(CompletionRequest {
            system: "Reply with exactly the word 'ok' in JSON format.".to_string(),
            user: "Reply with {\"status\": \"ok\"}".to_string(),
            response_format: ResponseFormat::Json,
            temperature: 0.0,
            max_output_tokens: Some(50),
            metadata: RequestMetadata::default(),
        })
        .await;

    let latency_ms = started.elapsed().as_millis() as u64;
    println!("  Latency: {latency_ms}ms");

    let mut healthy = true;
    match result {
        Ok(response) => {
            println!("  Finish reason: {:?}", response.finish_reason);
            println!(
                "  Tokens: in={} out={}",
                response.input_tokens.unwrap_or(0),
                response.output_tokens.unwrap_or(0),
            );
            // The response body is fully provider-controlled: sanitize AND
            // bound the preview so a crafted EPUB/provider cannot inject
            // escape sequences into the terminal (UI-5).
            println!(
                "  Content preview: {}",
                sanitize_truncated(&response.content, 200)
            );

            // JSON response_format support
            if response.content.trim().starts_with('{') || response.content.trim().starts_with('[')
            {
                println!("  JSON response_format: supported");
            } else {
                println!("  JSON response_format: may not be supported (got non-JSON response)");
            }

            // Usage tokens
            if response.input_tokens.is_some() && response.output_tokens.is_some() {
                println!("  Usage tokens: supported");
            } else {
                println!("  Usage tokens: not reported");
            }

            // Reasoning detection
            if response.is_reasoning_response() {
                println!("  Reasoning model: yes (reasoning_content detected)");
                println!("  Note: reasoning models use part of max_tokens for chain-of-thought.");
            }
        }
        Err(e) => {
            println!("  Completion: FAILED");
            println!("  Error: {}", sanitize_terminal(&e.to_string()));
            healthy = false;
        }
    }

    println!();
    println!(
        "  Recommended preset: --profile v1-fast --provider {provider_name} --model {effective_model}"
    );

    Ok(healthy)
}

async fn run_local_provider_doctor(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
) -> anyhow::Result<bool> {
    let (default_url, default_key_env, default_model) = match provider {
        "local-ollama" => ("http://localhost:11434/v1", "OLLAMA_API_KEY", "qwen2.5:14b"),
        "local-llamacpp" => (
            "http://localhost:8080/v1",
            "LLAMACPP_API_KEY",
            "local-model",
        ),
        _ => unreachable!("caller filters local providers"),
    };
    let effective_url = base_url.unwrap_or(default_url).trim_end_matches('/');
    let effective_key_env = api_key_env.unwrap_or(default_key_env);
    let effective_model = model.unwrap_or(default_model);
    let models_url = format!("{effective_url}/models");

    println!("  Base URL: {effective_url}");
    println!("  Model: {effective_model}");
    println!("  Models endpoint: {models_url}");

    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(timeout_seconds))
        .build()?;
    let mut request = client.get(&models_url);
    match std::env::var(effective_key_env) {
        Ok(key) if !key.is_empty() => {
            println!("  API key ({effective_key_env}): present");
            request = request.bearer_auth(key);
        }
        _ => println!("  API key ({effective_key_env}): not set (optional for local endpoints)"),
    }

    let started = std::time::Instant::now();
    let response = match request.send().await {
        Ok(response) => response,
        Err(error) => {
            println!("  Models endpoint: UNREACHABLE ({error})");
            return Ok(false);
        }
    };
    let status = response.status();
    let body = response.text().await?;
    println!("  Latency: {}ms", started.elapsed().as_millis());
    if !status.is_success() {
        println!(
            "  Models endpoint returned HTTP {}: {}",
            status.as_u16(),
            sanitize_truncated(&body, 300)
        );
        return Ok(false);
    }

    let parsed: Value = serde_json::from_str(&body)
        .map_err(|error| anyhow::anyhow!("models endpoint returned invalid JSON: {error}"))?;
    let models = parsed
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("models response is missing the 'data' array"))?
        .iter()
        .filter_map(|entry| entry.get("id").and_then(Value::as_str))
        .collect::<Vec<_>>();

    println!("  Loaded models: {}", models.len());
    if !models.contains(&effective_model) {
        // Remote-supplied ids: sanitize before printing (UI-5).
        let available = if models.is_empty() {
            "(none)".to_string()
        } else {
            models
                .iter()
                .map(|model| sanitize_terminal(model))
                .collect::<Vec<_>>()
                .join(", ")
        };
        println!(
            "  Model loaded: no — model '{effective_model}' is not available; available models: {available}"
        );
        return Ok(false);
    }
    println!("  Model loaded: yes");
    println!();
    println!("  Recommended preset: --provider-preset {provider} --model {effective_model}");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_exit_policy_fails_loudly_unless_no_fail_is_set() {
        assert!(evaluate_doctor_exit(&[], false).is_ok());
        assert!(evaluate_doctor_exit(&[], true).is_ok());

        let error = evaluate_doctor_exit(&["storage", "provider"], false)
            .expect_err("failed checks must flip the exit code");
        let message = error.to_string();
        assert!(message.contains("storage"));
        assert!(message.contains("provider"));
        assert!(message.contains("--no-fail"));

        // Legacy scripts that explicitly pass --no-fail keep the green exit.
        assert!(evaluate_doctor_exit(&["pdf"], true).is_ok());
    }
}
