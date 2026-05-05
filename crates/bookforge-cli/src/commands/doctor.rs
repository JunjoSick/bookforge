use clap::Args;

use bookforge_llm::{
    CompletionRequest, LlmProvider, OpenAiCompatibleConfig, OpenAiCompatibleProvider,
    RequestMetadata, ResponseFormat,
};
use bookforge_store::run_doctor;

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Check storage health
    #[arg(long)]
    pub storage: bool,

    /// Check provider health
    #[arg(long)]
    pub provider: Option<String>,

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
}

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let mut ran = false;

    if args.storage {
        ran = true;
        run_storage_doctor().await?;
    }

    if let Some(provider) = &args.provider {
        ran = true;
        run_provider_doctor(
            provider,
            args.model.as_deref(),
            args.base_url.as_deref(),
            args.api_key_env.as_deref(),
            args.timeout_seconds,
        )
        .await?;
    }

    if !ran {
        run_storage_doctor().await?;
    }

    Ok(())
}

async fn run_storage_doctor() -> anyhow::Result<()> {
    let doctor = run_doctor(None)?;

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
        }
    } else {
        println!("  database: {} (not found)", doctor.database_path.display());
        println!("  No storage issues to report.");
    }

    Ok(())
}

async fn run_provider_doctor(
    provider: &str,
    model: Option<&str>,
    base_url: Option<&str>,
    api_key_env: Option<&str>,
    timeout_seconds: u64,
) -> anyhow::Result<()> {
    use bookforge_core::RetryAfterPolicy;

    println!("Provider doctor: {provider}");
    println!();

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
        _ => {
            anyhow::bail!(
                "Provider '{provider}' is not supported for doctor checks. Use --provider deepseek or --provider openrouter, or pass --base-url and --api-key-env for custom endpoints."
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
            let masked = if key.len() > 8 {
                format!("{}...{}", &key[..4], &key[key.len() - 4..])
            } else {
                "***".to_string()
            };
            println!("  API key ({effective_key_env}): present ({masked})");
            key
        }
        Err(_) => {
            println!("  API key ({effective_key_env}): MISSING");
            println!();
            println!(
                "  Set the environment variable {effective_key_env} before using this provider."
            );
            return Ok(());
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
            return Ok(());
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

    match result {
        Ok(response) => {
            println!("  Finish reason: {:?}", response.finish_reason);
            println!(
                "  Tokens: in={} out={}",
                response.input_tokens.unwrap_or(0),
                response.output_tokens.unwrap_or(0),
            );
            println!("  Content preview: {}", {
                let truncated: String = response.content.chars().take(200).collect();
                truncated
            });

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
            println!("  Error: {e}");
        }
    }

    println!();
    println!(
        "  Recommended preset: --profile v1-fast --provider {provider_name} --model {effective_model}"
    );

    Ok(())
}
