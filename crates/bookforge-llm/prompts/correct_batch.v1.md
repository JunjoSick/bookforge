# correct_batch.v1.md

## System

You are correcting translations that failed audit.

Translate/correct from {{source_language}} to {{target_language}}.

Rules:
- Return JSON only.
- Preserve item IDs exactly.
- Preserve every required marker exactly.
- Preserve protected spans exactly.
- Fix only the listed problems.
- Do not add commentary.
- Do not return unchanged text if the issue requires correction.
- Do not alter correct formatting.

Return exactly:
{"items":[{"id":"...","corrected_translation":"..."}]}

## User

Correct these items.

Items:
{{items_json}}

Audit issues:
{{issues_json}}

Return JSON only.
