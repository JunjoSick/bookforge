# Checkpointing

Checkpointing is segment-level and content-addressed by source hash, prompt version, provider, and model.

## Store

Jobs live in `.bookforge/jobs.sqlite`. The store tracks job metadata, segment records, translated segment text, translated block text, report paths, retry state, glossary/style/entity data, and a JSON run snapshot.

When a job is created, `translate` snapshots the input EPUB into the job directory and stores a resolved run configuration. `resume` reads that snapshot instead of trusting current CLI defaults, then rebuilds the source IR and verifies that pending segment IDs still exist.

## Segment States

Segments move through pending, running, succeeded, skipped-cached, failed, needs-review, and retry-pending states. A segment translation is saved as soon as it validates, before the whole job finishes. If the process is interrupted, terminal segments remain usable.

`retry` marks selected failed or needs-review segments retry-pending. `resume` translates only resumable segments. Retry-pending segments bypass cache so the user gets a fresh provider call instead of reusing a known-bad result.

## Cache Keys

Cache lookup requires matching:

- source hash;
- cache namespace;
- prompt version;
- provider and model;
- source and target language;
- terminal segment status.

The cache namespace is derived from segmentation settings, profile namespace, batch mode, prompt version, glossary fingerprint, style fingerprint, entity fingerprint, and the inline marker schema version.

## Shared Execution Engine

Fresh translation and resume share `commands/translate/engine.rs` for the checkpointed provider run. That engine starts the SQLite checkpoint writer, chooses batch or single-segment execution from the resolved settings, sends save commands as segments complete, shuts the writer down, and returns fresh translations to the caller.

The callers still own their surrounding policy: fresh `translate` creates jobs and snapshots settings; `resume` resolves stored settings, checks cache namespace compatibility, and rebuilds output from stored plus fresh translations.

