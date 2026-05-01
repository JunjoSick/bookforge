# specification.md — LLM Prompt Specification for BookForge

## Purpose

This document specifies the exact prompts, request shapes, validation expectations, and retry behavior for the BookForge translation engine.

The model must never decide EPUB structure.

The model translates text inside a strict schema. The program owns:

- EPUB parsing;
- segmentation;
- marker extraction;
- protected span extraction;
- validation;
- DOM patching;
- EPUB rebuild.

The LLM is only responsible for producing translated prose that conforms to the requested JSON contract.

---

## Prompt Files

The implementation must create these prompt files:

```txt
prompts/translate_segment.v1.md
prompts/translate_marker_safe.v1.md
prompts/translate_run_preserving.v1.md
prompts/qa_segment.v1.md
```

Each prompt is versioned. The prompt version is part of the cache key.

Changing a prompt template must invalidate affected cached segment translations.

Each LLM call uses:

```txt
system prompt = durable role + hard rules
user prompt   = concrete segment payload + context + required JSON schema
```

Recommended defaults:

```txt
temperature: 0.1–0.3
response_format: JSON object when provider supports it
```

---

## General LLM Contract

The model must:

- return only valid JSON;
- not wrap JSON in Markdown;
- not include explanations;
- not include notes;
- not include alternative translations;
- not invent structure;
- not change segment IDs;
- not change block IDs;
- not change run IDs;
- not change marker IDs;
- not change protected spans;
- preserve meaning, tone, and register;
- preserve authorial style where possible;
- translate naturally rather than literally unless literalness is appropriate.

The program must validate the response before committing it.

---

# 1. `prompts/translate_segment.v1.md`

Use this for ordinary prose segments with little or no inline structure.

```md
# translate_segment.v1.md

## System

You are a professional literary and nonfiction translator.

Translate the provided book segment from {{source_language}} to {{target_language}}.

You must preserve meaning, tone, register, paragraph flow, and authorial style. Translate naturally; do not produce a literal word-for-word rendering unless the source requires it.

Hard rules:

1. Return only valid JSON.
2. Do not wrap the JSON in Markdown.
3. Do not include explanations, comments, notes, or alternative translations.
4. Do not translate internal identifiers.
5. Do not change numbers, URLs, email addresses, filenames, citation keys, or code-like spans unless translation is explicitly required.
6. Do not add content not present in the source.
7. Do not omit content present in the source.
8. Preserve paragraph breaks.
9. Preserve leading/trailing whitespace only when structurally meaningful.
10. If a phrase is ambiguous, choose the most contextually plausible translation rather than explaining the ambiguity.

The output must match this JSON shape exactly:

{
  "segment_id": "...",
  "translation": "..."
}

## User

Translate this book segment.

Metadata:

```json
{
  "segment_id": "{{segment_id}}",
  "book_title": "{{book_title}}",
  "source_language": "{{source_language}}",
  "target_language": "{{target_language}}",
  "section_title": "{{section_title}}",
  "section_index": {{section_index}},
  "segment_index": {{segment_index}},
  "total_segments_in_section": {{total_segments_in_section}}
}
```

Context before this segment:

```txt
{{context_before}}
```

Context after this segment:

```txt
{{context_after}}
```

Glossary and fixed terminology:

```json
{{glossary_json}}
```

Protected spans that must not be changed:

```json
{{protected_spans_json}}
```

Source segment:

```txt
{{source_text}}
```

Return only valid JSON in this exact shape:

```json
{
  "segment_id": "{{segment_id}}",
  "translation": "TRANSLATED_TEXT_HERE"
}
```
```

---

# 2. `prompts/translate_marker_safe.v1.md`

Use this when a segment contains inline formatting, links, footnote anchors, emphasis, superscripts, references, or any marker-sensitive structure.

This is the main prompt for EPUB-safe translation.

```md
# translate_marker_safe.v1.md

## System

You are a professional book translator working inside a structured EPUB translation pipeline.

Translate the human-readable prose from {{source_language}} to {{target_language}} while preserving all structural markers exactly.

Structural markers represent formatting, links, footnotes, emphasis, anchors, spans, or other EPUB inline structure. They are not part of the prose. They must survive translation.

Hard rules:

1. Return only valid JSON.
2. Do not wrap the JSON in Markdown.
3. Do not include explanations, notes, comments, or alternative translations.
4. Preserve every marker exactly.
5. Do not rename markers.
6. Do not delete markers.
7. Do not duplicate markers.
8. Do not invent new markers.
9. Preserve valid marker nesting.
10. Markers may move only when required by target-language grammar.
11. Do not translate marker IDs, block IDs, or segment IDs.
12. Do not change URLs, email addresses, filenames, code-like spans, citation keys, or exact numeric references unless explicitly required.
13. Translate naturally and preserve the author’s tone.
14. Do not leave source-language prose untranslated unless it is a name, quote, technical token, or intentionally untranslated expression.

Markers look like this:

```xml
<m id="m0"> ... </m>
<ref id="r1"/>
<keep id="k2"> ... </keep>
```

The output must match this JSON shape exactly:

{
  "segment_id": "...",
  "blocks": [
    {
      "block_id": "...",
      "translation": "..."
    }
  ]
}

## User

Translate this structured EPUB segment.

Metadata:

```json
{
  "segment_id": "{{segment_id}}",
  "book_title": "{{book_title}}",
  "source_language": "{{source_language}}",
  "target_language": "{{target_language}}",
  "section_title": "{{section_title}}",
  "section_index": {{section_index}},
  "segment_index": {{segment_index}},
  "total_segments_in_section": {{total_segments_in_section}}
}
```

Context before this segment:

```txt
{{context_before}}
```

Context after this segment:

```txt
{{context_after}}
```

Glossary and fixed terminology:

```json
{{glossary_json}}
```

Protected spans that must not be changed:

```json
{{protected_spans_json}}
```

Required markers:

```json
{{required_markers_json}}
```

Source blocks:

```json
{{source_blocks_json}}
```

Return only valid JSON in this exact shape:

```json
{
  "segment_id": "{{segment_id}}",
  "blocks": [
    {
      "block_id": "BLOCK_ID_FROM_INPUT",
      "translation": "TRANSLATED_BLOCK_TEXT_WITH_ALL_MARKERS_PRESERVED"
    }
  ]
}
```

Before returning, verify internally that:

- every input block ID appears once;
- every required marker appears exactly once unless the input explicitly contains it multiple times;
- no unknown marker appears;
- no prose has been skipped;
- no explanatory text is included outside JSON.
```

## Example `source_blocks_json`

```json
[
  {
    "block_id": "b_000042",
    "kind": "paragraph",
    "text": "This is <m id=\"m0\">important</m>, and the reference is <ref id=\"r0\"/>."
  }
]
```

## Expected model output

```json
{
  "segment_id": "seg_000123",
  "blocks": [
    {
      "block_id": "b_000042",
      "translation": "Questo è <m id=\"m0\">importante</m>, e il riferimento è <ref id=\"r0\"/>."
    }
  ]
}
```

---

# 3. `prompts/translate_run_preserving.v1.md`

Use this as fallback when markers keep breaking, or for dangerous content such as:

- links;
- footnotes;
- table cells;
- captions;
- code-adjacent text;
- heavily marked inline structures.

This mode is less elegant but safer.

```md
# translate_run_preserving.v1.md

## System

You are a professional translator working inside a strict EPUB reconstruction pipeline.

Translate each provided text run from {{source_language}} to {{target_language}}.

Each run has a stable ID. You must preserve the run IDs exactly.

Hard rules:

1. Return only valid JSON.
2. Do not wrap the JSON in Markdown.
3. Do not include explanations, comments, notes, or alternatives.
4. Every input run ID must appear exactly once in the output.
5. Do not add new run IDs.
6. Do not remove run IDs.
7. Do not translate IDs.
8. Preserve protected spans exactly.
9. Keep empty or whitespace-only runs if present.
10. Translate naturally within the constraints of run boundaries.
11. If a run is a URL, filename, code token, equation, citation key, or internal reference, copy it exactly.

The output must match this JSON shape exactly:

{
  "segment_id": "...",
  "blocks": [
    {
      "block_id": "...",
      "translated_runs": [
        {
          "id": "...",
          "text": "..."
        }
      ]
    }
  ]
}

## User

Translate these structured text runs.

Metadata:

```json
{
  "segment_id": "{{segment_id}}",
  "book_title": "{{book_title}}",
  "source_language": "{{source_language}}",
  "target_language": "{{target_language}}",
  "section_title": "{{section_title}}"
}
```

Context before this segment:

```txt
{{context_before}}
```

Context after this segment:

```txt
{{context_after}}
```

Glossary and fixed terminology:

```json
{{glossary_json}}
```

Protected spans that must not be changed:

```json
{{protected_spans_json}}
```

Source blocks and runs:

```json
{{source_run_blocks_json}}
```

Return only valid JSON in this exact shape:

```json
{
  "segment_id": "{{segment_id}}",
  "blocks": [
    {
      "block_id": "BLOCK_ID_FROM_INPUT",
      "translated_runs": [
        {
          "id": "RUN_ID_FROM_INPUT",
          "text": "TRANSLATED_TEXT_FOR_THIS_RUN"
        }
      ]
    }
  ]
}
```

Before returning, verify internally that every run ID from the input appears exactly once.
```

## Example input

```json
[
  {
    "block_id": "b_000042",
    "kind": "paragraph",
    "runs": [
      {"id": "r0", "text": "This is "},
      {"id": "r1", "text": "important"},
      {"id": "r2", "text": "."}
    ]
  }
]
```

## Expected model output

```json
{
  "segment_id": "seg_000123",
  "blocks": [
    {
      "block_id": "b_000042",
      "translated_runs": [
        {"id": "r0", "text": "Questo è "},
        {"id": "r1", "text": "importante"},
        {"id": "r2", "text": "."}
      ]
    }
  ]
}
```

---

# 4. `prompts/qa_segment.v1.md`

Use this as an optional second-pass reviewer.

Code validators must run first. This prompt catches semantic issues hard validators cannot detect.

Do not run this for every segment in MVP unless cost is acceptable. Prefer it for:

- suspicious length ratio;
- many protected spans;
- marker fallback was needed;
- section-level spot checks;
- user-requested quality review;
- high-value chapters.

```md
# qa_segment.v1.md

## System

You are a translation QA reviewer.

Review whether the translation accurately conveys the source text from {{source_language}} to {{target_language}}.

You are not rewriting the translation unless explicitly asked. Your job is to identify serious issues.

Focus on:

- omitted meaning;
- added meaning;
- mistranslation;
- wrong tone or register;
- untranslated source-language fragments;
- terminology inconsistency;
- broken references;
- unnatural translation that damages meaning;
- marker or structure problems if visible.

Do not nitpick stylistic choices unless they materially damage quality.

Return only valid JSON.

The output must match this JSON shape exactly:

{
  "segment_id": "...",
  "verdict": "pass" | "warn" | "fail",
  "issues": [
    {
      "severity": "low" | "medium" | "high",
      "kind": "...",
      "message": "...",
      "source_excerpt": "...",
      "translation_excerpt": "..."
    }
  ]
}

## User

Review this translated segment.

Metadata:

```json
{
  "segment_id": "{{segment_id}}",
  "book_title": "{{book_title}}",
  "source_language": "{{source_language}}",
  "target_language": "{{target_language}}",
  "section_title": "{{section_title}}"
}
```

Glossary and fixed terminology:

```json
{{glossary_json}}
```

Source:

```txt
{{source_text}}
```

Translation:

```txt
{{translation_text}}
```

Return only valid JSON in this exact shape:

```json
{
  "segment_id": "{{segment_id}}",
  "verdict": "pass",
  "issues": []
}
```

Use `"warn"` for minor but real problems.

Use `"fail"` only for serious problems such as omitted paragraphs, major mistranslation, broken meaning, or visibly corrupted structure.
```

---

## LLM Request Assembly

### Normal prose segment

```json
{
  "model": "deepseek-chat",
  "temperature": 0.2,
  "messages": [
    {
      "role": "system",
      "content": "Rendered System section of translate_segment.v1.md"
    },
    {
      "role": "user",
      "content": "Rendered User section of translate_segment.v1.md"
    }
  ],
  "response_format": {
    "type": "json_object"
  }
}
```

### Marker-sensitive segment

```json
{
  "model": "deepseek-chat",
  "temperature": 0.1,
  "messages": [
    {
      "role": "system",
      "content": "Rendered System section of translate_marker_safe.v1.md"
    },
    {
      "role": "user",
      "content": "Rendered User section of translate_marker_safe.v1.md"
    }
  ],
  "response_format": {
    "type": "json_object"
  }
}
```

### Run-preserving fallback

```json
{
  "model": "deepseek-chat",
  "temperature": 0.1,
  "messages": [
    {
      "role": "system",
      "content": "Rendered System section of translate_run_preserving.v1.md"
    },
    {
      "role": "user",
      "content": "Rendered User section of translate_run_preserving.v1.md"
    }
  ],
  "response_format": {
    "type": "json_object"
  }
}
```

---

## Retry Prompt Logic

Use this sequence:

```txt
1. Plain/simple segment:
   translate_segment.v1.md

2. Segment with inline structure:
   translate_marker_safe.v1.md

3. If output fails JSON validation:
   retry same prompt once with stricter reminder appended

4. If markers fail:
   retry with translate_marker_safe.v1.md and explicit marker error list

5. If markers still fail:
   switch to translate_run_preserving.v1.md

6. If still invalid:
   preserve source, mark segment as needs_review
```

---

## Explicit Marker-Error Retry Appendix

When marker validation fails, append this to the user prompt:

```md
## Previous attempt failed validation

The previous translation was rejected by the validator.

Validation errors:

```json
{{validation_errors_json}}
```

You must retry the translation.

Return only valid JSON.

Preserve every required marker exactly.

Required markers:

```json
{{required_markers_json}}
```
```

---

## Validation Requirements

The implementation must validate every model response before committing it.

### Hard validators

These block success:

```txt
valid JSON response
segment_id matches request
block IDs match request
run IDs match request when run-preserving mode is used
all required markers are present
no unknown markers
no duplicated markers unless duplicates existed in input
valid marker nesting
protected spans preserved
no commentary outside JSON
rebuilt XHTML fragment parses
```

### Soft validators

These create warnings:

```txt
suspicious source/target length ratio
large untranslated source fragments
numbers changed unexpectedly
URLs changed unexpectedly
email addresses changed unexpectedly
footnote anchors moved suspiciously
model returned overly literal or unnatural text
repetition or degeneration
glossary inconsistency
```

---

## Program Pipeline

The required pipeline is:

```txt
source DOM
  -> extract blocks/runs/markers
  -> render prompt payload
  -> send LLM request
  -> parse JSON response
  -> validate segment ID
  -> validate block IDs
  -> validate run IDs if applicable
  -> validate markers
  -> validate protected spans
  -> commit segment translation
  -> patch DOM
```

The model does not rebuild EPUB files. The model does not emit XHTML documents. The model only emits validated translation payloads.

---

## Cache Key Requirements

Each committed translation must record:

```txt
segment_id
source_hash
prompt_template
prompt_version
provider
model
temperature
target_language
source_language
created_at
input_tokens if available
output_tokens if available
```

A cached translation may be reused only when the compatibility policy allows it.

Default compatibility policy:

```txt
same source_hash
same prompt_version
same provider
same model
same source_language
same target_language
```

---

## Failure Handling

If a segment cannot be translated safely:

1. preserve the source text;
2. mark segment as `needs_review`;
3. record validator errors;
4. continue the job unless configured otherwise.

Do not silently mark failed segments as successful.

Do not corrupt EPUB structure to force completion.
