# Checking PDF conversion fidelity

`bookforge convert` turns a PDF into a reflowable EPUB. It rebuilds reading
order, paragraphs, emphasis, headings, notes and figure placement from
Poppler's layout output. It does not keep the page geometry: the result is a
reflowable book, not a PDF to print in the original layout.

Convert and check the result before translating:

```sh
bookforge convert source.pdf --out source.epub
bookforge inspect source.epub
bookforge validate source.epub
```

## What the conversion report measures

The report is written to `<out>.convert.json` (or `--report <path>`) and a
summary is printed. Its coverage figure compares non-whitespace character
counts with Poppler's `pdftotext` output, the baseline:

- Reconstructed text counts in full.
- Text preserved inside table, equation and figure crops is credited as
  `media_preserved_chars`; ordinary translation does not translate those
  pixels.
- Running heads and feet, page numbers and contents-page numbers are removed
  on purpose. They are totalled as `running_header_chars` and credited back,
  both in the document figure and in each page's 95% confidence check. Each
  page's share is in `page_stats[].running_header_chars`.

High coverage shows that the text was captured. It does not show that the
reading order is right, that every passage is translatable text, or that the
meaning survived. If the PDF has no selectable text at all,
`text_coverage_available` is false and the summary prints the coverage as
unavailable rather than 100%.

## Low-confidence pages and OCR

A page whose credited coverage falls below 95% is low-confidence. So is a
page with no selectable text whose only content is one positioned image
covering at least half of it: a scanned page (`image_only_candidate` in its
page stats, confirmed by an empty `pdftotext` page). Blank pages, small
images and images behind selectable text do not qualify. A scan assembled
from several image tiles is not recognized yet.

`--ocr-endpoint` sends low-confidence pages to OCR (see
[pdf-ocr.md](pdf-ocr.md)). OCR replaces the page's text but keeps its figures
and captions. Its plain-text answer cannot place individual lines around a
figure, so recovered text goes where the page's text began. A response with
no text leaves the page's content in place, and the page remains eligible for
`--low-confidence preserve`. The report warns for each scanned page, saying
whether OCR supplied its text.

## Page renders

Figure and table crops, preserved pages and OCR input are cut from page
renders at 150 DPI. A page too large for a 32-million-pixel render at 150
DPI is rendered at a lower DPI that fits, and crops from it are scaled to
match. A page that does not fit even at 8 DPI is not rendered: its crops,
preservation or OCR are skipped with a warning and its text is kept. Poppler's
intermediate images go to private temporary directories that are removed
on success and on error.

## What to review by eye

Compare a sample of source pages with the EPUB, especially:

- italic and bold runs inside sentences, note reference marks, and columns;
- numbered notes and bibliographies, where font changes can look like table
  cells;
- running heads, page transitions, indented and right-aligned passages;
- figure and table crops, every edge, and the caption around each one.

Detected headings become linked navigation entries, and a book with at least
two chapters of body text is split into one file per chapter at level-1
headings (see
[CLI_REFERENCE.md](CLI_REFERENCE.md#pdf-ingestion-and-epub-reflow)). The
publication identifier is derived from the source PDF's bytes, so two
different PDFs never share an identity in a reading app, and converting the
same PDF again gives the same one.

A clean report and a valid EPUB still need a reading of the translation for
technical terms, dates, negation and paragraph-level meaning.
