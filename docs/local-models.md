# Local OpenAI-compatible models

BookForge includes presets for Ollama and llama.cpp's `llama-server`. Both use
the same validated JSON translation contracts as hosted providers; only the
endpoint and conservative runtime defaults change.

## Ollama

```bash
ollama pull qwen2.5:14b
ollama serve

bookforge doctor --provider local-ollama --model qwen2.5:14b
bookforge translate book.epub \
  --target Italian \
  --provider-preset local-ollama \
  --model qwen2.5:14b \
  --validate-output \
  --out book.it.epub
```

The preset resolves to `http://localhost:11434/v1`. `OLLAMA_API_KEY` is
optional; if set, BookForge sends it as a bearer token.

## llama.cpp

Start an OpenAI-compatible server with a model and a context size appropriate
for the selected BookForge profile:

```bash
llama-server -m /models/model.gguf --port 8080 --ctx-size 32768

bookforge doctor --provider local-llamacpp --model <id-from-models-endpoint>
bookforge translate book.epub \
  --target Italian \
  --provider-preset local-llamacpp \
  --model <id-from-models-endpoint> \
  --model-context-tokens 32768 \
  --validate-output \
  --out book.it.epub
```

The preset resolves to `http://localhost:8080/v1`. `LLAMACPP_API_KEY` is
optional.

## Doctor behavior

For local presets, `doctor` requests `<base-url>/models` and fails unless the
requested model ID appears in the returned OpenAI-compatible `data` array.
This catches the common case where the daemon is running but the intended
model is not loaded.

Local models vary significantly in JSON reliability and marker preservation.
Start with a short book or chapter, keep concurrency at the preset default,
and inspect the generated review and validation reports before a long run.
