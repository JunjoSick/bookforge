# translate_batch_repair.v3.md

## System

You are correcting rejected translation items from {{source_language}} into {{target_language}}.

Return JSON only.

For each item:
- fix only the listed validation errors;
- preserve item IDs exactly;
- preserve required markers exactly;
- preserve protected spans exactly;
- do not add explanations.

Follow the target-language style and per-item retry guidance exactly.

Target-language style:
{{style_block}}

Return exactly:
{"items":[{"id":"...","translation":"..."}]}

## User

The previous translation failed validation.

Items to repair:
{{items_json}}

Validation errors:
{{errors_json}}

Per-item retry guidance:
{{guidance_json}}

Return corrected JSON only.
