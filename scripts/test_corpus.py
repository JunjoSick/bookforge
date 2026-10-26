"""Unit checks for the corpus smoke gate.

Nothing here downloads a book, builds the binary or calls a provider: the
fetch, the commands and the reports are all stubbed.

Run with `python3 scripts/test_corpus.py`.
"""

from contextlib import ExitStack
from copy import deepcopy
from pathlib import Path
import sys
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parent))

import corpus  # noqa: E402


def validator_report(blocks: int, segments: int) -> dict:
    return {
        "epubcheck": {"status": "passed"},
        "bookforge_validators": {
            "xhtml_spine_count": 2,
            "section_count": 4,
            "block_count": blocks,
            "default_segmentation_count": segments,
        },
    }


class SmokeGateTests(unittest.TestCase):
    def run_smoke(self, *, real_provider=None, output_blocks=10, output_segments=6):
        source = validator_report(blocks=10, segments=6)
        translated = deepcopy(source)
        translated["bookforge_validators"].update(
            block_count=output_blocks, default_segmentation_count=output_segments
        )
        environment = {}
        if real_provider is not None:
            environment = {
                "BOOKFORGE_CORPUS_REAL_PROVIDER": real_provider,
                "BOOKFORGE_CORPUS_REAL_MODEL": "stub-model",
            }
        with ExitStack() as stack:
            stack.enter_context(patch.dict(corpus.os.environ, environment, clear=True))
            stack.enter_context(patch.object(corpus, "fetch"))
            stack.enter_context(
                patch.object(corpus, "command_path", return_value=Path("bookforge"))
            )
            stack.enter_context(
                patch.object(corpus, "load_books", return_value=[{"id": "stub-book"}])
            )
            stack.enter_context(patch.object(corpus.Path, "mkdir"))
            stack.enter_context(patch.object(corpus, "run"))
            stack.enter_context(
                patch.object(corpus, "load_report", side_effect=[source, translated])
            )
            stack.enter_context(
                patch.object(corpus, "zip_metrics", return_value={"files": 7, "images": 2})
            )
            stack.enter_context(patch("builtins.print"))
            corpus.smoke("small")

    def test_identity_mock_passes_when_every_count_matches(self):
        self.run_smoke()

    def test_identity_mock_must_keep_the_segmentation_count(self):
        with self.assertRaisesRegex(
            SystemExit, "default_segmentation_count changed 6 -> 7"
        ):
            self.run_smoke(output_segments=7)

    def test_real_translation_may_move_segment_boundaries(self):
        self.run_smoke(real_provider="stub-provider", output_segments=7)

    def test_real_translation_must_still_keep_every_block(self):
        with self.assertRaisesRegex(SystemExit, "block_count changed 10 -> 9"):
            self.run_smoke(real_provider="stub-provider", output_blocks=9)


if __name__ == "__main__":
    unittest.main()
