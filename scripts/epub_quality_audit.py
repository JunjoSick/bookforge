#!/usr/bin/env python3
"""Compare a source EPUB with a replacement-mode translation, without a provider.

This is a review aid, not a language or semantic-quality judge. Unchanged prose
can be intentional quotations; token/character ratios differ between languages;
legitimate rhetoric can repeat. CSS, image text and scripted content are not
rendered. Use the locations and excerpts to adjudicate findings against the book.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from dataclasses import dataclass
from difflib import SequenceMatcher
import html.entities
import json
from pathlib import Path
import posixpath
import re
import sys
import unicodedata
from urllib.parse import unquote, urlsplit
import xml.etree.ElementTree as ET
import zipfile


BLOCKS = frozenset(
    "address article aside blockquote body caption dd div dl dt figcaption figure "
    "footer h1 h2 h3 h4 h5 h6 header hr li main nav ol p pre section table tbody "
    "td tfoot th thead tr ul".split()
)
LEAF_BLOCKS = frozenset("caption dd dt figcaption h1 h2 h3 h4 h5 h6 li p pre td th".split())
EXCLUDED = frozenset({"head", "script", "style", "template"})
WORD = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]|[^\W_\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]+(?:['’][^\W_\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]+)*", re.UNICODE)
MAX_RESOURCE_BYTES = 64 * 1024 * 1024


def local_name(tag: str) -> str:
    return tag.rsplit("}", 1)[-1].split(":")[-1].lower()


def normalized(text: str) -> str:
    return " ".join(unicodedata.normalize("NFC", text).split())


def tokens(text: str) -> list[str]:
    # CJK ideographs count individually, rather than treating a whole paragraph
    # as one word. These are comparable lexical units, not linguistic word counts.
    return WORD.findall(text.casefold())


def parse_xml(data: bytes) -> ET.Element:
    try:
        return ET.fromstring(data)
    except ET.ParseError:
        # XHTML often relies on named HTML entities declared in an external DTD.
        # Resolve only those names; unescaping &lt; would introduce fake markup.
        text = data.decode("utf-8-sig")

        def entity(match: re.Match) -> str:
            name = match[1]
            value = html.entities.html5.get(name + ";")
            if name in {"amp", "lt", "gt", "apos", "quot"} or value is None:
                return match[0]
            return "".join(f"&#{ord(char)};" for char in value)

        return ET.fromstring(re.sub(r"&([A-Za-z][A-Za-z0-9]+);", entity, text))


@dataclass(frozen=True)
class Paragraph:
    resource: str
    location: str
    text: str
    role: str = "spine"

    @property
    def key(self) -> tuple[str, str]:
        return self.resource, self.location


def extract_paragraphs(root: ET.Element, resource: str, role: str = "spine") -> list[Paragraph]:
    """Collect text once, preserving inline adjacency and all element tails."""
    body = next((node for node in root.iter() if local_name(node.tag) == "body"), root)
    paragraphs: list[Paragraph] = []

    def hidden(node: ET.Element) -> bool:
        style = re.sub(r"\s+", "", node.get("style", "").lower())
        return local_name(node.tag) in EXCLUDED or "hidden" in node.attrib or bool(
            re.search(r"(?:^|;)(?:display:none|visibility:hidden)(?:!important)?(?:;|$)", style)
        )

    def walk(node: ET.Element, path: str) -> None:
        if hidden(node):
            return
        pieces: list[str] = []
        fragment = 0
        had_boundary = False

        def flush(*, keep_empty: bool = False) -> None:
            nonlocal fragment
            text = normalized("".join(pieces))
            if text or keep_empty:
                paragraphs.append(Paragraph(resource, f"{path}#{fragment}", text, role))
            # Advance even for empty fragments so deleting text cannot realign
            # subsequent prose in a container onto an unrelated source fragment.
            fragment += 1
            pieces.clear()

        def content(element: ET.Element, element_path: str) -> None:
            nonlocal had_boundary
            if element.text:
                pieces.append(element.text)
            for index, child in enumerate(element):
                tag = local_name(child.tag)
                child_path = f"{element_path}/{tag}[{index}]"
                if not hidden(child):
                    if tag in BLOCKS:
                        had_boundary = True
                        flush()
                        walk(child, child_path)
                    elif tag == "br":
                        pieces.append(" ")
                    else:
                        content(child, child_path)
                if child.tail:
                    pieces.append(child.tail)

        content(node, path)
        flush(keep_empty=not had_boundary and local_name(node.tag) in LEAF_BLOCKS)

    walk(body, "body")
    return paragraphs


def archive_path(base: str, reference: str) -> str:
    parsed = urlsplit(reference)
    if parsed.scheme or parsed.netloc:
        raise ValueError(f"external EPUB resource: {reference!r}")
    path = posixpath.normpath(posixpath.join(base, unquote(parsed.path)))
    if path.startswith(("../", "/")) or path == "..":
        raise ValueError(f"EPUB resource escapes archive root: {reference!r}")
    return path


def read_epub(path: Path) -> list[Paragraph]:
    with zipfile.ZipFile(path) as archive:
        def read(name: str) -> ET.Element:
            info = archive.getinfo(name)
            if info.file_size > MAX_RESOURCE_BYTES:
                raise ValueError(f"resource too large to audit: {name}")
            with archive.open(info) as stream:
                data = stream.read(MAX_RESOURCE_BYTES + 1)
            if len(data) > MAX_RESOURCE_BYTES:
                raise ValueError(f"resource too large to audit: {name}")
            return parse_xml(data)

        container = read("META-INF/container.xml")
        rootfile = next(node for node in container.iter() if local_name(node.tag) == "rootfile")
        opf_path = archive_path("", rootfile.attrib["full-path"])
        opf = read(opf_path)
        manifest = {node.attrib["id"]: node for node in opf.iter() if local_name(node.tag) == "item"}
        ordered = [(node.attrib["idref"], "spine") for node in opf.iter() if local_name(node.tag) == "itemref"]
        ordered.extend((key, "navigation") for key, node in manifest.items() if "nav" in node.get("properties", "").split())
        # Linked notes and appendices can be readable without a spine entry.
        ordered.extend((key, "auxiliary") for key in manifest)
        seen: set[str] = set()
        result: list[Paragraph] = []
        for item_id, role in ordered:
            item = manifest[item_id]
            if item.get("media-type") not in {"application/xhtml+xml", "text/html"}:
                continue
            name = archive_path(posixpath.dirname(opf_path), item.attrib["href"])
            if name in seen:
                continue
            seen.add(name)
            result.extend(extract_paragraphs(read(name), name, role))
        if not result:
            raise ValueError("EPUB has no auditable body paragraphs")
        return result


def strongest_repeat(words: list[str]) -> dict | None:
    best = None
    for width in range(1, min(12, len(words) // 2) + 1):
        index = 0
        while index + width * 2 <= len(words):
            phrase = words[index:index + width]
            repeats = 1
            while words[index + repeats * width:index + (repeats + 1) * width] == phrase:
                repeats += 1
            excess = width * (repeats - 1)
            if (repeats >= 3 or width >= 4 and repeats >= 2) and width * repeats >= 6:
                if best is None or excess > best["excess_units"]:
                    best = {"phrase": " ".join(phrase), "repeats": repeats, "excess_units": excess}
            index += max(1, excess)
    return best


def excerpt(text: str, limit: int = 260) -> str:
    return text if len(text) <= limit else text[:limit].rstrip() + "…"


def audit(source: list[Paragraph], target: list[Paragraph], min_units: int = 20) -> dict:
    targets = {paragraph.key: paragraph for paragraph in target}
    sources = {paragraph.key: paragraph for paragraph in source}
    findings = []
    totals = Counter()
    duplicate_targets: dict[str, list[tuple[Paragraph, Paragraph]]] = defaultdict(list)

    def flag(kind: str, original: Paragraph, translated: Paragraph | None, **details) -> None:
        findings.append({"kind": kind, "resource": original.resource, "location": original.location,
                         "role": original.role, "source": excerpt(original.text),
                         "target": excerpt(translated.text) if translated else None, **details})

    for original in source:
        if not original.text:
            continue
        words = tokens(original.text)
        totals["source_paragraphs"] += 1
        totals["source_units"] += len(words)
        totals["source_characters"] += len(original.text)
        if len(words) >= min_units:
            totals["long_source_paragraphs"] += 1
        translated = targets.get(original.key)
        if translated is None:
            totals["unmatched_source_paragraphs"] += 1
            flag("unmatched_source_location", original, None)
            continue
        totals["matched_source_paragraphs"] += 1
        target_words = tokens(translated.text)
        if not translated.text:
            flag("empty_target", original, translated)
        if len(words) >= min_units:
            if original.text == translated.text:
                totals["unchanged_long_paragraphs"] += 1
                totals["unchanged_long_source_units"] += len(words)
                flag("unchanged_long_prose", original, translated, source_units=len(words))
            else:
                # A language label, punctuation change or a translated opening
                # sentence must not conceal the rest of a copied paragraph.
                match = SequenceMatcher(None, words, target_words, autojunk=False).find_longest_match()
                if match.size >= min_units:
                    totals["paragraphs_with_long_source_copy"] += 1
                    flag("copied_long_source_span", original, translated,
                         copied_units=match.size, source_fraction=round(match.size / len(words), 3),
                         copied_span=excerpt(" ".join(words[match.a:match.a + match.size])))
                if len(target_words) >= min_units:
                    duplicate_targets[translated.text].append((original, translated))
            ratio = len(translated.text) / max(1, len(original.text))
            if ratio < 0.35 or ratio > 2.5:
                flag("extreme_length_ratio", original, translated, character_ratio=round(ratio, 3))
        repetition = strongest_repeat(target_words)
        source_repetition = strongest_repeat(words)
        if repetition and repetition["excess_units"] > (source_repetition or {}).get("excess_units", 0) + 2:
            flag("introduced_tandem_repetition", original, translated, **repetition)

    for copies in duplicate_targets.values():
        if len(copies) > 1 and len({original.text for original, _ in copies}) > 1:
            for original, translated in copies:
                flag("duplicate_target_for_different_sources", original, translated, occurrences=len(copies))

    totals["target_paragraphs"] = sum(bool(paragraph.text) for paragraph in target)
    totals["target_units"] = sum(len(tokens(paragraph.text)) for paragraph in target)
    totals["target_characters"] = sum(len(paragraph.text) for paragraph in target)
    totals["unmatched_target_paragraphs"] = sum(bool(paragraph.text) and paragraph.key not in sources for paragraph in target)
    for paragraph in target:
        if paragraph.text and paragraph.key not in sources:
            findings.append({"kind": "unmatched_target_location", "resource": paragraph.resource,
                             "location": paragraph.location, "role": paragraph.role,
                             "source": None, "target": excerpt(paragraph.text)})
    # Explicit zeros keep result schemas stable across successful and failed runs.
    for key in ("unmatched_source_paragraphs", "matched_source_paragraphs", "long_source_paragraphs",
                "unchanged_long_paragraphs", "unchanged_long_source_units", "paragraphs_with_long_source_copy"):
        totals.setdefault(key, 0)
    return {"schema_version": 1, "metrics": dict(totals),
            "finding_counts": dict(Counter(finding["kind"] for finding in findings)), "findings": findings,
            "limitations": [
                "Heuristic review candidates, not a verdict on translation fidelity or language.",
                "Locations assume replacement mode with preserved document structure; unmatched locations require structural review.",
                "Unchanged quotations, bibliographies and target-language source passages can be legitimate.",
                "Length ratios vary by language; lexical units count each CJK ideograph separately.",
                "Inline text and tails are included; external CSS, image text and scripts are not rendered.",
                "Package and HTML head metadata are outside this prose audit.",
                "Repetition checks catch local tandem loops and identical long outputs, not all repeated or omitted meaning.",
            ]}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("target", type=Path)
    parser.add_argument("--json", type=Path, help="write the complete machine-readable audit")
    parser.add_argument("--min-units", type=int, default=20, help="minimum lexical units for long-prose checks (default: 20)")
    parser.add_argument("--examples", type=int, default=8, help="maximum findings printed (default: 8)")
    args = parser.parse_args()
    if args.min_units < 1 or args.examples < 0:
        parser.error("--min-units must be positive and --examples must be nonnegative")
    try:
        report = audit(read_epub(args.source), read_epub(args.target), args.min_units)
    except (OSError, ValueError, KeyError, StopIteration, ET.ParseError, zipfile.BadZipFile) as exc:
        parser.exit(2, f"Cannot audit EPUB: {exc}\n")
    report.update(source=str(args.source), target=str(args.target))
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: report[key] for key in ("source", "target", "metrics", "finding_counts")}, ensure_ascii=False, indent=2))
    for finding in report["findings"][:args.examples]:
        print(f"\n{finding['kind']}: {finding['resource']} {finding['location']}")
        print(f"  source: {finding['source']}")
        print(f"  target: {finding['target']}")
    print("\nReview candidates only; unchanged text and repetition can be intentional.")


if __name__ == "__main__":
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")
    main()
