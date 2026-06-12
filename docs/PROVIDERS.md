# Providers

The initial provider target is OpenAI-compatible chat completions, with DeepSeek as a preset.

## Supported Providers

- `mock` is used by tests and local roundtrip checks. It can echo, prefix, uppercase, or deliberately return malformed output.
- `deepseek` defaults to `https://api.deepseek.com/v1`, `DEEPSEEK_API_KEY`, and `deepseek-v4-flash`.
- `openrouter` defaults to `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`, and `openrouter/auto`.
- `openai-compatible` requires an explicit `--base-url` unless a preset resolves one.

All non-mock providers go through `OpenAiCompatibleProvider`. Provider settings include timeout, provider-level attempts, retry-after handling, max backoff, idle connection pool size, JSON mode, model context tokens, and output-token limits.

## Presets And Profiles

Provider presets can change both endpoint/model defaults and runtime knobs such as concurrency, provider attempts, batch target size, adaptive batch sizing, and retry policy. Explicit CLI flags win over preset values.

Translation profiles control segmentation, scheduler, batching, compact prompts, and provider defaults. The CLI default is `v1-fast`, which enables batching by default. Batch translation uses the batch prompt version and apportions request-level token usage back to segments for reporting.

## JSON Mode

`JsonMode::Auto` tries provider-native JSON response format where supported and falls back when the provider rejects it. `PromptOnly` relies on prompt instructions only. `Strict` requires native JSON support to work.

## Retry Boundaries

Provider attempts handle network and provider errors. Scheduler attempts handle translation validation failures. Batch repair can retry invalid batch items, and resume/retry commands use the SQLite checkpoint store to avoid paying again for already terminal segments.

