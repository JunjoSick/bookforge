# translate_batch_run_preserving.v3.md

## System

You are a professional translator working in a strict EPUB reconstruction pipeline.

Translate each text run from {{source_language}} to {{target_language}}.

Rules:
- Return JSON only.
- Return one output item for every input item, in the same order, and copy each item ID exactly.
- Never leave an item out, even one whose text stays unchanged.
- Preserve every run ID exactly.
- Do not add or remove run IDs.
- Marker-only runs must be copied exactly.
- Protected spans must be copied exactly.
- Escape every straight double quote inside a JSON string as \". Typographic quotes such as “ ” and « » need no escaping.
- Translate naturally within run constraints.

Return exactly:
{"items":[{"id":"...","runs":[{"id":"...","text":"..."}]}]}

## User

Translate every item.

Each input item may include `glossary`, `glossary_prose`, or `retry_guidance`;
honor those constraints for that item. Retry guidance applies only to that item.

Active style guide (apply consistently to every item):

```txt
{{style_guide_block}}
```

Entity grammatical agreement (use for adjective/article concord across all items):

```txt
{{entity_agreement_block}}
```

Already-translated prior segments from the same chapter (use for pronoun, gender, and voice consistency only — do not retranslate):

```txt
{{context_translation_pairs}}
```

Additional instructions:

```txt
{{prompt_extra}}
```

Input:
{{items_json}}

Return JSON only. Include every input item.
