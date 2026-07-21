# OCR recovery for PDF conversion

`bookforge convert --ocr-endpoint <BASE_URL>` sends only low-confidence PDF
pages to an OpenAI-compatible vision endpoint. BookForge first renders each
page as PNG, then replaces that page's weak reconstructed blocks with the OCR
response. The conversion report keeps the page marked low-confidence but
records `action=ocr` and includes it in `OCR-recovered pages`. If OCR fails,
the configured `--low-confidence` behavior still applies.

OCR output is inserted as plain paragraphs split at blank lines. A model may
return Markdown; BookForge currently preserves it as raw text. Markdown-aware
post-processing is deferred.

Loopback endpoints (`localhost`, `127.0.0.1`, or `::1`) do not need an API key.
For remote endpoints, set `OCR_API_KEY`, or name another environment variable
with `--ocr-api-key-env`.

## Recommended SGLang setup for Unlimited-OCR

The model card's tested SGLang path uses its development wheel and the matching
kernels release. From the Unlimited-OCR checkout or release bundle:

```bash
uv venv --python 3.12
source .venv/bin/activate
uv pip install wheel/sglang-0.0.0.dev11416+g92e8bb79e-py3-none-any.whl
uv pip install kernels==0.11.7
```

Launch the server with custom logit processors enabled:

```bash
python -m sglang.launch_server \
  --model baidu/Unlimited-OCR \
  --served-model-name Unlimited-OCR \
  --attention-backend fa3 \
  --page-size 1 \
  --mem-fraction-static 0.8 \
  --context-length 32768 \
  --enable-custom-logit-processor \
  --disable-overlap-schedule \
  --skip-server-warmup \
  --host 0.0.0.0 \
  --port 10000
```

Generate the serialized Python-side processor value in the same environment:

```bash
python scripts/unlimited-ocr-logit-processor.py > unlimited-ocr-processor.txt
```

Then convert with the Unlimited-OCR dialect:

```bash
bookforge convert scan.pdf --out scan.epub \
  --ocr-endpoint http://127.0.0.1:10000/v1 \
  --ocr-dialect unlimited-ocr \
  --ocr-model Unlimited-OCR \
  --ocr-image-mode gundam \
  --ocr-logit-processor unlimited-ocr-processor.txt
```

The dialect always sends `images_config` and the default custom parameters
`ngram_size=35` and `window_size=90`. The serialized processor blob is passed
through verbatim. Omitting it is supported, but dense pages may enter
repetition loops.

## vLLM alternative

Unlimited-OCR also has a plain OpenAI-compatible vLLM recipe. Point BookForge
at its `/v1` base URL and keep the default `openai` dialect:

```bash
bookforge convert scan.pdf --out scan.epub \
  --ocr-endpoint http://127.0.0.1:8000/v1 \
  --ocr-dialect openai \
  --ocr-model baidu/Unlimited-OCR
```

This path works without SGLang's custom fields, but dense pages have a greater
risk of repetition loops because n-gram suppression is unavailable.

Use `bookforge doctor --ocr-endpoint http://127.0.0.1:10000/v1` to verify that
the endpoint is reachable and list its reported model IDs.
