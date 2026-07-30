# Providers

The initial provider target is OpenAI-compatible chat completions, with DeepSeek as a preset.

## Supported Providers

- `mock` is used by tests and local roundtrip checks. It can echo, prefix, uppercase, or deliberately return malformed output.
- `deepseek` defaults to `https://api.deepseek.com/v1`, `DEEPSEEK_API_KEY`, and `deepseek-v4-flash`.
- `openrouter` defaults to `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`, and `openrouter/auto`.
- `openai-compatible` requires an explicit `--base-url` unless a preset resolves one.

All non-mock providers go through `OpenAiCompatibleProvider`. Provider settings include timeout, provider-level attempts, retry-after handling, max backoff, idle connection pool size, JSON mode, model context tokens, and output-token limits.

## Thinking Suppression

`--no-thinking` is dispatched from the configured base URL (or the known
OpenRouter/DeepSeek preset credential identity when a proxy overrides that URL)
because
OpenAI-compatible chat-completion APIs do not share one reasoning-control
parameter:

| Endpoint | Request field |
| --- | --- |
| OpenRouter (`openrouter.ai`) | `"reasoning": {"enabled": false}` |
| OpenAI (`api.openai.com`) | `"reasoning_effort": "none"` |
| DeepSeek (`api.deepseek.com`) | `"thinking": {"type": "disabled"}` |

DeepSeek V4's OpenAI-format API documents the `thinking` toggle directly.
This is a DeepSeek extension, not an Anthropic Messages API request: BookForge
still sends `/chat/completions`.

For any other `openai-compatible` base URL, BookForge cannot safely guess a
vendor-specific parameter. It omits all suppression fields and warns instead
of sending an unknown field that the server may silently ignore. The local
Ollama and llama.cpp presets currently take this warning path. A model may
also require reasoning even when its gateway supports a suppression field;
the provider's model-specific error remains authoritative.

## Reasoning Token Accounting

OpenAI-format usage reports define
`completion_tokens_details.reasoning_tokens` as a breakdown of
`completion_tokens`, not an additional token count. BookForge therefore stores
the billable completion aggregate in `segments.tokens_output`; adding the
reasoning detail again would double-count standards-compliant responses.

As a defensive compatibility measure, BookForge compares `completion_tokens`
with `total_tokens - prompt_tokens` and keeps the larger output aggregate. This
also handles gateways that report visible completion tokens separately while
keeping `total_tokens` correct. If only a reasoning-token detail is present,
that count is used as the output fallback. Cost estimates price the resulting
`tokens_output` aggregate once. There is no separate reasoning column or
schema migration.

## Presets And Profiles

Provider presets can change both endpoint/model defaults and runtime knobs such as concurrency, provider attempts, batch target size, adaptive batch sizing, and retry policy. Explicit CLI flags win over preset values.

Translation profiles control segmentation, scheduler, batching, compact prompts, and provider defaults. The CLI default is `v1-fast`, which enables batching by default. Batch translation uses the batch prompt version and apportions request-level token usage back to segments for reporting.

## JSON Mode

`JsonMode::Auto` tries provider-native JSON response format where supported and falls back when the provider rejects it. `PromptOnly` relies on prompt instructions only. `Strict` requires native JSON support to work.

## Retry Boundaries

Provider attempts handle network and provider errors. Scheduler attempts handle translation validation failures. Batch repair can retry invalid batch items, and resume/retry commands use the SQLite checkpoint store to avoid paying again for already terminal segments.

