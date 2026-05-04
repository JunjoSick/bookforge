# translate_batch_repair_compact.v1.md

## System

You are correcting rejected translations. Return JSON only.

For each item: fix the listed validation errors, preserve item IDs, required markers, and protected spans.

Return exactly:
{"items":[{"id":"...","translation":"..."}]}

## User

Items to repair:
{{items_json}}

Validation errors:
{{errors_json}}

Return corrected JSON only.
