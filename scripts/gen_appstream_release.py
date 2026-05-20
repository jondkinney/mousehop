#!/usr/bin/env python3
"""Generate an AppStream <release> XML block from a CHANGELOG.md entry.

Usage:
    gen_appstream_release.py CHANGELOG.md VERSION DATE

Writes the resulting XML to stdout. The AppStream description
subset accepts only <p>, <ul>, and <li>, so the script converts
Keep-a-Changelog subsections into that shape.

If the CHANGELOG file doesn't exist, the version section isn't
found, or the section has no bullet items, a minimal
<release version="X" date="Y"/> is emitted (still valid AppStream).
The post-release workflow uses idempotent metainfo patching, so a
stub entry will simply be left alone on subsequent runs.

CHANGELOG format expected (Keep a Changelog):

    ## [VERSION] - DATE              (with or without a compare-URL)
    ### Added
    - feature one
    - *(scope)* feature two          (conventional-commit scope is stripped)
    ### Fixed
    - bug
"""
from __future__ import annotations

import html
import os.path
import re
import sys


def emit_minimal(version: str, date: str) -> None:
    print(f'<release version="{version}" date="{date}"/>')


def main() -> int:
    if len(sys.argv) != 4:
        print(
            "usage: gen_appstream_release.py CHANGELOG.md VERSION DATE",
            file=sys.stderr,
        )
        return 2
    changelog_path, version, date = sys.argv[1], sys.argv[2], sys.argv[3]

    if not os.path.isfile(changelog_path):
        emit_minimal(version, date)
        return 0

    with open(changelog_path, encoding="utf-8") as f:
        content = f.read()

    # Match `## [VERSION]` with or without a compare-URL link.
    pattern = re.compile(
        r"^## \[" + re.escape(version) + r"\][^\n]*\n(.*?)(?=\n## \[|\Z)",
        re.MULTILINE | re.DOTALL,
    )
    match = pattern.search(content)
    if not match:
        emit_minimal(version, date)
        return 0

    section = match.group(1).strip()
    # Split on `### Header` lines into ['', 'Header1', body1, 'Header2', body2, ...].
    subsections = re.split(r"^### (\w+)\s*\n", section, flags=re.MULTILINE)

    parts: list[str] = []
    for i in range(1, len(subsections), 2):
        header = subsections[i]
        body = subsections[i + 1] if i + 1 < len(subsections) else ""
        items: list[str] = []
        for line in body.split("\n"):
            stripped = line.strip()
            if not stripped.startswith("- "):
                continue
            item = stripped[2:].strip()
            # Strip conventional-commit scope: `*(desktop)* foo` -> `foo`.
            item = re.sub(r"^\*\([^)]+\)\*\s*", "", item)
            # Collapse internal whitespace from line wraps.
            item = re.sub(r"\s+", " ", item)
            if item:
                items.append(html.escape(item))
        if items:
            parts.append(f"<p>{header}:</p>")
            parts.append("<ul>")
            for item in items:
                parts.append(f"  <li>{item}</li>")
            parts.append("</ul>")

    if not parts:
        emit_minimal(version, date)
        return 0

    print(f'<release version="{version}" date="{date}">')
    print("  <description>")
    for part in parts:
        print(f"    {part}")
    print("  </description>")
    print("</release>")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
