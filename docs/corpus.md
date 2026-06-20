# Standard Ebooks corpus

BookForge's structural regression corpus is a curated set of public-domain
EPUBs from [Standard Ebooks](https://standardebooks.org/). The manifest lives
at `tests/corpus/standard-ebooks/manifest.toml`; downloaded books and generated
outputs are intentionally ignored by git.

The corpus currently covers nine books across three tiers:

| Tier | Books | Primary structural coverage |
|---|---:|---|
| small | 3 | dialogue, italics, letters, epigraphs, nested quotations |
| medium | 3 | footnotes, music notation, geometry, tables, longer novels |
| large | 3 | illustrations, very large books, foreign-language passages, deep nesting |

The exact titles, source URLs, byte sizes, SHA-256 hashes, and feature labels
are recorded in the manifest. Hash changes are treated as reviewable corpus
updates; the fetch script never silently accepts a changed upstream file.

## Running the corpus

Requirements:

- Python 3.11 or newer
- Java 8 or newer
- EPUBCheck on `PATH`, or `BOOKFORGE_EPUBCHECK` pointing to the executable,
  its directory, or `epubcheck.jar`

```bash
bash scripts/corpus-fetch.sh small
bash scripts/corpus-smoke.sh small
```

Use `medium` to run the small and medium books, or `large` for all nine.
Set `BOOKFORGE_CORPUS_REQUIRE_EPUBCHECK=0` only for local debugging when
EPUBCheck is unavailable; CI always requires it.

## What the smoke test proves

For each selected book the harness:

1. verifies the pinned size and SHA-256;
2. validates the source with BookForge and EPUBCheck;
3. translates it with the deterministic `mock-identity` provider;
4. validates the rebuilt EPUB with BookForge and EPUBCheck;
5. compares XHTML spine, section, block, segment, archive-file, and image
   counts between source and output.

The small tier runs on every pull request. The full tier runs nightly and on
manual CI dispatch. A real-provider pre-release run can be enabled with
`BOOKFORGE_CORPUS_REAL_PROVIDER` and `BOOKFORGE_CORPUS_REAL_MODEL`; API keys
remain ordinary provider environment variables and are never written by the
harness.

This methodology checks structural preservation, not translation quality.
Reader experience still requires the review UI and human reading.
