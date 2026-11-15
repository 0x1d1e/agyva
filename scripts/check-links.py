#!/usr/bin/env python3
"""Check relative links and anchors in the repository's Markdown.

External http(s), mailto and other URI links are not fetched. For every local
link the target path must exist; a `#fragment` on a Markdown target must match
a heading slug (GitHub style) or an explicit HTML anchor id/name.

Usage: python3 -I scripts/check-links.py [path ...]
With no arguments, every *.md file outside .git/, target/ and dist/ is checked.
"""

from __future__ import annotations

import pathlib
import re
import sys
import urllib.parse

ROOT = pathlib.Path(__file__).resolve().parent.parent

SKIP_DIRS = {".git", "target", "dist"}
SKIP_SCHEMES = ("http://", "https://", "mailto:", "tel:", "data:", "ftp:", "//")

# [text](target) and ![alt](target); the target may be wrapped in <> and may be
# followed by a "title".
LINK_RE = re.compile(r'!?\[[^\]]*\]\(\s*(<[^>]*>|[^)\s]+)(?:\s+["\'][^"\']*["\'])?\s*\)')
HEADING_RE = re.compile(r"^(#{1,6})\s+(.*?)\s*$")
FENCE_RE = re.compile(r"^\s*(`{3,}|~{3,})")
EXPLICIT_ANCHOR_RE = re.compile(r"""\b(?:id|name)\s*=\s*["']([^"']+)["']""")
CODE_SPAN_RE = re.compile(r"`+[^`]*`+")


def markdown_files(paths: list[str]) -> list[pathlib.Path]:
    if paths:
        out = []
        for raw in paths:
            p = pathlib.Path(raw)
            if not p.is_absolute():
                p = ROOT / p
            if p.is_dir():
                out.extend(p.rglob("*.md"))
            else:
                out.append(p)
        return sorted(set(out))
    out = []
    for p in ROOT.rglob("*.md"):
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        out.append(p)
    return sorted(out)


def strip_inline_markdown(text: str) -> str:
    text = re.sub(r"!\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    text = CODE_SPAN_RE.sub("", text)
    text = re.sub(r"<[^>]+>", "", text)
    text = re.sub(r"[*_~]", "", text)
    return text.strip()


def slugify(text: str) -> str:
    text = strip_inline_markdown(text).lower()
    text = re.sub(r"[^\w\- ]", "", text, flags=re.UNICODE)
    return text.replace(" ", "-")


def text_lines(path: pathlib.Path):
    """Yield (line_number, line) outside fenced code blocks."""
    fenced = False
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if FENCE_RE.match(line):
            fenced = not fenced
            continue
        if not fenced:
            yield number, line


def anchors(path: pathlib.Path) -> set[str]:
    found: set[str] = set()
    counts: dict[str, int] = {}
    for _, line in text_lines(path):
        match = HEADING_RE.match(line)
        if match:
            base = slugify(match.group(2).rstrip("#").strip())
            if not base:
                continue
            count = counts.get(base, 0)
            counts[base] = count + 1
            found.add(base if count == 0 else f"{base}-{count}")
        for explicit in EXPLICIT_ANCHOR_RE.findall(line):
            found.add(explicit)
    return found


def target_of(raw: str) -> str:
    if raw.startswith("<") and raw.endswith(">"):
        raw = raw[1:-1]
    return raw.strip()


def check_file(path: pathlib.Path, cache: dict[pathlib.Path, set[str]]) -> list[str]:
    problems: list[str] = []
    if not path.is_file():
        return [f"{path}: file does not exist"]
    for number, line in text_lines(path):
        for raw in LINK_RE.findall(line):
            target = target_of(raw)
            if not target or target.startswith(SKIP_SCHEMES):
                continue
            parts = target.split("#", 1)
            path_part = urllib.parse.unquote(parts[0])
            fragment = urllib.parse.unquote(parts[1]) if len(parts) > 1 else ""
            if path_part.startswith("/"):
                dest = ROOT / path_part.lstrip("/")
            elif path_part:
                dest = path.parent / path_part
            else:
                dest = path
            try:
                dest = dest.resolve()
            except OSError:
                problems.append(f"{path}:{number}: bad link {target!r}")
                continue
            if not dest.exists():
                problems.append(f"{path}:{number}: missing target {target!r}")
                continue
            if not fragment:
                continue
            if dest.is_dir():
                continue
            if dest.suffix.lower() != ".md":
                continue
            if dest not in cache:
                cache[dest] = anchors(dest)
            if fragment not in cache[dest]:
                problems.append(f"{path}:{number}: missing anchor {target!r}")
    return problems


def main(argv: list[str]) -> int:
    files = markdown_files(argv)
    if not files:
        print("check-links: no Markdown files found", file=sys.stderr)
        return 1
    cache: dict[pathlib.Path, set[str]] = {}
    problems: list[str] = []
    for path in files:
        problems.extend(check_file(path, cache))
    if problems:
        for problem in problems:
            print(problem, file=sys.stderr)
        print(f"check-links: {len(problems)} broken link(s) in {len(files)} file(s)", file=sys.stderr)
        return 1
    print(f"check-links: {len(files)} file(s) OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
