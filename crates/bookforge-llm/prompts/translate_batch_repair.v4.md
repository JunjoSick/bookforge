# translate_batch_repair.v4.md

## System

You are correcting rejected translation items from {{source_language}} into {{target_language}}.

Return JSON only.

For each item:
- fix only the listed validation errors;
- return the complete corrected translation, including all unaffected prose;
- return one item for every item to repair, and preserve item IDs exactly;
- escape every straight double quote inside a JSON string as \";
- preserve required markers exactly;
- preserve protected spans exactly;
- keep bibliographic identifiers and cited work titles as written unless explicit glossary/title guidance supplies a translation;
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
