#!/usr/bin/env python3
"""Fetch and smoke-test the pinned Standard Ebooks regression corpus."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import urllib.request
import zipfile

try:
    import tomllib
except ModuleNotFoundError as exc:  # pragma: no cover - Python < 3.11
    raise SystemExit("Python 3.11 or newer is required for corpus scripts") from exc


ROOT = Path(__file__).resolve().parents[1]
CORPUS_DIR = ROOT / "tests" / "corpus" / "standard-ebooks"
MANIFEST_PATH = CORPUS_DIR / "manifest.toml"
CACHE_DIR = CORPUS_DIR / "cache"
OUTPUT_DIR = CORPUS_DIR / "output"
TIER_RANK = {"small": 0, "medium": 1, "large": 2}


def load_books(tier: str) -> list[dict]:
    with MANIFEST_PATH.open("rb") as stream:
        manifest = tomllib.load(stream)
    if manifest.get("schema_version") != 1:
        raise SystemExit("unsupported corpus manifest schema")
    rank = TIER_RANK[tier]
    return [
        book
        for book in manifest.get("book", [])
        if TIER_RANK.get(book["ci_tier"], 999) <= rank
    ]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify(path: Path, book: dict) -> bool:
    return (
        path.is_file()
        and path.stat().st_size == book["size_bytes"]
        and sha256(path) == book["sha256"]
    )


def fetch(tier: str) -> None:
    CACHE_DIR.mkdir(parents=True, exist_ok=True)
    for book in load_books(tier):
        destination = CACHE_DIR / f"{book['id']}.epub"
        if verify(destination, book):
            print(f"cached {book['id']}")
            continue
        temporary = destination.with_suffix(".epub.tmp")
        request = urllib.request.Request(
            book["url"], headers={"User-Agent": "BookForge corpus regression/1.8"}
        )
        print(f"fetching {book['id']}")
        try:
            with urllib.request.urlopen(request, timeout=120) as response:
                with temporary.open("wb") as stream:
                    shutil.copyfileobj(response, stream)
            if not verify(temporary, book):
                actual_size = temporary.stat().st_size if temporary.exists() else 0
                actual_hash = sha256(temporary) if temporary.exists() else "(missing)"
                raise SystemExit(
                    f"{book['id']} checksum mismatch: size={actual_size}, sha256={actual_hash}"
                )
            temporary.replace(destination)
        finally:
            temporary.unlink(missing_ok=True)


def command_path() -> Path:
    configured = os.environ.get("BOOKFORGE_BIN")
    if configured:
        return Path(configured).resolve()
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "bookforge-cli"],
        cwd=ROOT,
        check=True,
    )
    executable = "bookforge.exe" if os.name == "nt" else "bookforge"
    return ROOT / "target" / "debug" / executable


def run(command: list[str], *, cwd: Path) -> None:
    print("+", " ".join(command))
    subprocess.run(command, cwd=cwd, check=True)


def zip_metrics(path: Path) -> dict[str, int]:
    image_extensions = {".avif", ".gif", ".jpeg", ".jpg", ".png", ".svg", ".webp"}
    with zipfile.ZipFile(path) as archive:
        names = [name for name in archive.namelist() if not name.endswith("/")]
    return {
        "files": len(names),
        "images": sum(Path(name).suffix.lower() in image_extensions for name in names),
    }


def load_report(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as stream:
        return json.load(stream)


def default_segmentation_count(internal: dict, book_id: str) -> int:
    """Read the diagnostic default-config segment count from a validator report.

    Report schema 3 renamed this field from `segment_count`; both names are
    accepted so a corpus checkout holding reports written by an older binary
    still passes. Note this is a re-segmentation with `SegmentationConfig`
    defaults, not the scheduler-segment count a translation job records.
    """
    for field in ("default_segmentation_count", "segment_count"):
        if field in internal:
            return internal[field]
    raise SystemExit(
        f"{book_id}: validator report has neither default_segmentation_count "
        "nor segment_count"
    )


def assert_epubcheck(report: dict, book_id: str) -> None:
    status = report["epubcheck"]["status"]
    if status == "errors":
        raise SystemExit(f"{book_id}: EPUBCheck reported errors")
    require = os.environ.get("BOOKFORGE_CORPUS_REQUIRE_EPUBCHECK", "1") != "0"
    if require and status == "unavailable":
        raise SystemExit(
            f"{book_id}: EPUBCheck unavailable; set BOOKFORGE_EPUBCHECK or "
            "BOOKFORGE_CORPUS_REQUIRE_EPUBCHECK=0"
        )


def smoke(tier: str) -> None:
    fetch(tier)
    binary = command_path()
    output_root = OUTPUT_DIR / tier
    output_root.mkdir(parents=True, exist_ok=True)
    real_provider = os.environ.get("BOOKFORGE_CORPUS_REAL_PROVIDER")
    real_model = os.environ.get("BOOKFORGE_CORPUS_REAL_MODEL")

    for book in load_books(tier):
        book_id = book["id"]
        input_path = CACHE_DIR / f"{book_id}.epub"
        output_path = output_root / f"{book_id}.translated.epub"
        input_report_path = output_root / f"{book_id}.input.validation.json"
        output_report_path = output_root / f"{book_id}.translated.validation.json"

        run(
            [
                str(binary),
                "validate",
                str(input_path),
                "--report",
                str(input_report_path),
            ],
            cwd=output_root,
        )

        provider_args = ["--provider", "mock", "--model", "mock-identity"]
        if real_provider:
            if not real_model:
                raise SystemExit(
                    "BOOKFORGE_CORPUS_REAL_MODEL is required with "
                    "BOOKFORGE_CORPUS_REAL_PROVIDER"
                )
            provider_args = ["--provider", real_provider, "--model", real_model]

        run(
            [
                str(binary),
                "translate",
                str(input_path),
                "--source",
                "English",
                "--target",
                "Italian",
                *provider_args,
                "--profile",
                "v1-fast",
                "--context-window",
                "0",
                "--ui",
                "quiet",
                "--validate-output",
                "--out",
                str(output_path),
            ],
            cwd=output_root,
        )

        generated_report = output_path.with_name(
            f"{output_path.stem}.validation.json"
        )
        if generated_report != output_report_path:
            shutil.copyfile(generated_report, output_report_path)

        input_report = load_report(input_report_path)
        output_report = load_report(output_report_path)
        assert_epubcheck(input_report, book_id)
        assert_epubcheck(output_report, book_id)

        input_internal = input_report["bookforge_validators"]
        output_internal = output_report["bookforge_validators"]
        for metric in ("xhtml_spine_count", "section_count", "block_count"):
            if input_internal[metric] != output_internal[metric]:
                raise SystemExit(
                    f"{book_id}: {metric} changed "
                    f"{input_internal[metric]} -> {output_internal[metric]}"
                )

        # Report schema 3 renamed this field from `segment_count`, so accept both:
        # a corpus checkout may still hold reports generated by an older binary.
        input_segments = default_segmentation_count(input_internal, book_id)
        output_segments = default_segmentation_count(output_internal, book_id)
        if input_segments != output_segments:
            raise SystemExit(
                f"{book_id}: default_segmentation_count changed "
                f"{input_segments} -> {output_segments}"
            )

        input_zip = zip_metrics(input_path)
        output_zip = zip_metrics(output_path)
        if input_zip != output_zip:
            raise SystemExit(
                f"{book_id}: ZIP structural metrics changed {input_zip} -> {output_zip}"
            )
        print(f"PASS {book_id}: {output_segments} default-segmentation segments")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("operation", choices=("fetch", "smoke"))
    parser.add_argument("tier", choices=tuple(TIER_RANK), nargs="?", default="small")
    args = parser.parse_args()
    if args.operation == "fetch":
        fetch(args.tier)
    else:
        smoke(args.tier)


if __name__ == "__main__":
    main()
