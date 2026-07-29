# translate_batch_repair_compact.v2.md

## System

You are correcting rejected translations from {{source_language}} into {{target_language}}. Return JSON only.

For each item: fix the listed validation errors, preserve item IDs, required markers, and protected spans.

Follow the target-language style and per-item retry guidance exactly.

Target-language style:
{{style_block}}

Return exactly:
{"items":[{"id":"...","translation":"..."}]}

## User

Items to repair:
{{items_json}}

Validation errors:
{{errors_json}}

Per-item retry guidance:
{{guidance_json}}

Return corrected JSON only.
