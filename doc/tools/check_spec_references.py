#!/usr/bin/env python3
"""Mechanically checks the cross references in the Rust design spec.

Design spec §6 requires, at the end of every phase, that the §2 mapping table
and every in-text reference (including numbered items) resolve to an existing
heading, and that every BUG/PERF/SPEC id has its own definition.

Usage:

    python3 doc/tools/check_spec_references.py [spec.md]

Exit code 0 means every reference resolved.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

DEFAULT_SPEC = Path(__file__).resolve().parents[1] / "img2webp-rust-design-spec.md"

HEADING_NUMBER = re.compile(r"^(#{2,4})\s+(\d+(?:\.\d+)*)\.?\s")
ISSUE_DEFINITION = re.compile(r"^####\s+((?:BUG|PERF|SPEC)-\d{2})\b", re.MULTILINE)
SECTION_REFERENCE = re.compile(r"§\s*(\d+(?:\.\d+)*)")
ISSUE_REFERENCE = re.compile(r"\b((?:BUG|PERF|SPEC)-\d{2})\b")
DECISION_POINT = re.compile(r"\b(D-[A-Z]+)\b")
NUMBERED_ITEM = re.compile(r"§\s*7\s*項目\s*(\d)")


def main() -> int:
    spec_path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_SPEC
    text = spec_path.read_text(encoding="utf-8")

    headings = {}
    for line in text.splitlines():
        match = HEADING_NUMBER.match(line)
        if match:
            headings[match.group(2)] = line.strip()
    defined_issues = set(ISSUE_DEFINITION.findall(text))
    defined_decisions = set(DECISION_POINT.findall(text))

    # References inside the numbered sections (skip the bibliography). The
    # decision-point list in §6 counts as a definition, so D-* ids only need to
    # appear at least twice.
    problems = []
    for number in sorted(set(SECTION_REFERENCE.findall(text))):
        if number not in headings:
            problems.append(f"§{number} does not resolve to a heading")
    for issue in sorted(set(ISSUE_REFERENCE.findall(text))):
        if issue not in defined_issues:
            problems.append(f"{issue} has no '#### {issue}' definition")
    for decision in sorted(defined_decisions):
        occurrences = len(re.findall(rf"\b{decision}\b", text))
        if occurrences < 2:
            problems.append(f"{decision} is referenced but never defined")
    for item in sorted(set(NUMBERED_ITEM.findall(text))):
        if not re.search(rf"^{item}\.\s", text, re.MULTILINE):
            problems.append(f"§7 項目{item} has no numbered definition")

    print(f"headings: {len(headings)}")
    print(f"issue definitions: {len(defined_issues)} -> {', '.join(sorted(defined_issues))}")
    print(f"decision points: {len(defined_decisions)} -> {', '.join(sorted(defined_decisions))}")
    if problems:
        print(f"\n{len(problems)} unresolved reference(s):")
        for problem in problems:
            print(f"  - {problem}")
        return 1
    print("\nall cross references resolve")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
