"""Regression checks for the independent EPUB quality audit.

Run with: python -m unittest discover -s scripts -p test_epub_quality_audit.py
"""

import unittest
from io import BytesIO
import zipfile

from epub_quality_audit import archive_path, audit, extract_paragraphs, parse_xml, read_epub, tokens


def paragraphs(markup):
    return extract_paragraphs(parse_xml(markup.encode()), "chapter.xhtml")


class EpubQualityAuditTests(unittest.TestCase):
    def test_inline_text_and_tails_keep_adjacency(self):
        result = paragraphs("<body><p>A super<em>man</em> &amp; a <b>bold</b> tail.</p></body>")
        self.assertEqual([p.text for p in result], ["A superman & a bold tail."])

    def test_nested_blocks_are_not_double_counted(self):
        result = paragraphs("<body><div>Before <em>inline</em><p>Inner.</p> After.</div><ul><li>First<p>Second</p></li></ul></body>")
        self.assertEqual([p.text for p in result], ["Before inline", "Inner.", "After.", "First", "Second"])

    def test_hidden_content_does_not_hide_its_tail(self):
        result = paragraphs('<body><p>A <span hidden="hidden">secret</span> tail<br/>end<style>x</style>.</p><p style="display: none;">hidden</p></body>')
        self.assertEqual([p.text for p in result], ["A tail end."])

    def test_html_entities_do_not_turn_escaped_text_into_markup(self):
        result = paragraphs("<body><p>&nbsp;A &mdash; &lt;em&gt;text&lt;/em&gt;</p></body>")
        self.assertEqual(result[0].text, "A — <em>text</em>")

    def test_empty_paragraph_preserves_alignment(self):
        source = paragraphs("<body><p>First.</p><p>Second.</p></body>")
        target = paragraphs("<body><p></p><p>Secondo.</p></body>")
        report = audit(source, target, min_units=1)
        self.assertEqual(report["metrics"]["matched_source_paragraphs"], 2)
        self.assertEqual(report["finding_counts"]["empty_target"], 1)
        self.assertNotIn("unmatched_source_location", report["finding_counts"])

    def test_source_repetition_is_not_a_new_error(self):
        source = paragraphs("<body><p>very very very very very very</p></body>")
        target = paragraphs("<body><p>molto molto molto molto molto molto</p></body>")
        self.assertNotIn("introduced_tandem_repetition", audit(source, target)["finding_counts"])

    def test_prefix_and_punctuation_cannot_conceal_source_copy(self):
        source = paragraphs("<body><p>The original sentence should have been translated in full.</p></body>")
        target = paragraphs("<body><p>[Italian] The original sentence — should have been translated in full!</p></body>")
        report = audit(source, target, min_units=5)
        self.assertEqual(report["finding_counts"]["copied_long_source_span"], 1)
        self.assertEqual(report["findings"][0]["source_fraction"], 1.0)

    def test_new_repetition_and_collapsed_distinct_paragraphs(self):
        source = paragraphs("<body><p>The first original sentence has many different words.</p><p>The second paragraph describes another thing entirely.</p></body>")
        target = paragraphs("<body><p>molto molto molto molto molto molto</p><p>molto molto molto molto molto molto</p></body>")
        report = audit(source, target, min_units=5)
        self.assertEqual(report["finding_counts"]["introduced_tandem_repetition"], 2)
        self.assertEqual(report["finding_counts"]["duplicate_target_for_different_sources"], 2)

    def test_cjk_prose_and_safe_archive_paths(self):
        self.assertEqual(tokens("中文 test"), ["中", "文", "test"])
        self.assertEqual(archive_path("OPS", "text/one%20two.xhtml#part"), "OPS/text/one two.xhtml")
        with self.assertRaises(ValueError):
            archive_path("OPS", "../../outside.xhtml")

    def test_missing_long_source_is_still_counted(self):
        source = paragraphs("<body><p>Some original prose is missing from the output.</p></body>")
        report = audit(source, [], min_units=5)
        self.assertEqual(report["metrics"]["long_source_paragraphs"], 1)
        self.assertEqual(report["finding_counts"]["unmatched_source_location"], 1)

    def test_epub_spine_order_url_paths_and_navigation(self):
        stream = BytesIO()
        with zipfile.ZipFile(stream, "w") as archive:
            archive.writestr("META-INF/container.xml", '<container><rootfiles><rootfile full-path="OPS/package.opf"/></rootfiles></container>')
            archive.writestr("OPS/package.opf", '<package><manifest><item id="one" href="one%20chapter.xhtml" media-type="application/xhtml+xml"/><item id="two" href="two.xhtml" media-type="application/xhtml+xml"/><item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/><item id="note" href="notes.xhtml" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="two"/><itemref idref="one"/></spine></package>')
            archive.writestr("OPS/one chapter.xhtml", "<html><body><p>First chapter.</p></body></html>")
            archive.writestr("OPS/two.xhtml", "<html><body><p>Second chapter.</p></body></html>")
            archive.writestr("OPS/nav.xhtml", "<html><body><nav><p>Contents.</p></nav></body></html>")
            archive.writestr("OPS/notes.xhtml", "<html><body><p>Linked notes.</p></body></html>")
        stream.seek(0)
        result = read_epub(stream)
        self.assertEqual([p.text for p in result], ["Second chapter.", "First chapter.", "Contents.", "Linked notes."])
        self.assertEqual([p.role for p in result], ["spine", "spine", "navigation", "auxiliary"])


if __name__ == "__main__":
    unittest.main()
