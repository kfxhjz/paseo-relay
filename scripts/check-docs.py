#!/usr/bin/env python3
"""Check local Markdown links and Chinese/English link consistency.

Fails if:
- a relative file link points at a missing path;
- a Chinese document links to a known English canonical document without
  linking its Chinese counterpart (unless the link text says English/英文);
- a framed ASCII box diagram in a fenced code block is misaligned (CJK
  characters count as two columns, so mixed English/Chinese diagrams must be
  checked by display width, not byte or character count).

Deliberately open diagrams (no `┌ ... ┐` frame) are ignored.

English canonical documents may always be linked from the English docs. The
Chinese docs must either link the translated file or make the English target
explicit.
"""

from __future__ import annotations

import os
import re
import sys
import unicodedata
from collections.abc import Iterator

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LINK = re.compile(r"\[([^\]]+)\]\(([^)]+)\)")
SKIP_PREFIXES = ("http://", "https://", "mailto:", "#")

# English canonical path (relative to repo root) -> Chinese counterpart.
PAIRS = {
    "README.md": "README.zh-CN.md",
    "DESIGN.md": "DESIGN.zh-CN.md",
    "docs/quickstart.md": "docs/zh-CN/quickstart.md",
    "docs/comparison.md": "docs/zh-CN/comparison.md",
    "docs/operator-policy.md": "docs/zh-CN/operator-policy.md",
    "docs/translation.md": "docs/zh-CN/translation.md",
    "docs/protocol.md": "docs/zh-CN/protocol.md",
    "docs/configuration.md": "docs/zh-CN/configuration.md",
    "docs/deployment.md": "docs/zh-CN/deployment.md",
    "docs/compatibility.md": "docs/zh-CN/compatibility.md",
    "docs/operations.md": "docs/zh-CN/operations.md",
    "docs/migration.md": "docs/zh-CN/migration.md",
    "docs/benchmarks.md": "docs/zh-CN/benchmarks.md",
    "docs/faq.md": "docs/zh-CN/faq.md",
    "SECURITY.md": "SECURITY.zh-CN.md",
    "CONTRIBUTING.md": "CONTRIBUTING.zh-CN.md",
    "SUPPORT.md": "SUPPORT.zh-CN.md",
    "CODE_OF_CONDUCT.md": "CODE_OF_CONDUCT.zh-CN.md",
    "docs/evidence/paseo-e2e-2026-11-07.md": "docs/zh-CN/evidence/paseo-e2e-2026-11-07.md",
}


def markdown_files() -> list[str]:
    result: list[str] = []
    for dirpath, dirnames, filenames in os.walk(ROOT):
        dirnames[:] = [
            d
            for d in dirnames
            if d not in {".git", "target", "node_modules", "fuzz", "local"}
        ]
        for name in filenames:
            if name.endswith(".md"):
                result.append(os.path.join(dirpath, name))
    return sorted(result)


def is_chinese_doc(rel: str) -> bool:
    return (
        rel.endswith(".zh-CN.md")
        or rel.startswith("docs/zh-CN/")
        or rel
        in {
            "README.zh-CN.md",
            "SECURITY.zh-CN.md",
            "CONTRIBUTING.zh-CN.md",
            "SUPPORT.zh-CN.md",
        }
    )


def read_text(path: str) -> str:
    with open(path, encoding="utf-8") as handle:
        return handle.read()


def read_leading_lines(path: str, limit: int) -> list[str]:
    with open(path, encoding="utf-8") as handle:
        return handle.read().splitlines(True)[:limit]


BOX_CHARS = set("┌┐└┘┬┴┼│├┤─►◄▼▲")
BOX_LEFT = set("┌├└│")
BOX_RIGHT = set("┐┤┘│")


def display_width(text: str) -> int:
    """Terminal display width: East Asian wide/fullwidth characters are 2."""
    return sum(
        2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1 for ch in text
    )


def box_columns(line: str) -> list[tuple[str, int]]:
    columns: list[tuple[str, int]] = []
    column = 0
    for ch in line:
        if ch in BOX_CHARS:
            columns.append((ch, column))
        column += 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1
    return columns


def fenced_blocks(text: str) -> Iterator[tuple[int, list[str]]]:
    """Yield (first content line number, lines) for each fenced code block."""
    fence = ""
    block: list[str] = []
    start = 0
    for lineno, line in enumerate(text.splitlines(), 1):
        stripped = line.lstrip()
        if not fence:
            if stripped.startswith(("```", "~~~")):
                fence = stripped[:3]
                block = []
                start = lineno + 1
            continue
        if stripped.startswith(fence):
            yield start, block
            fence = ""
            continue
        block.append(line)


def diagram_errors(rel: str, text: str) -> list[str]:
    """Check that framed box diagrams close on one display column."""
    errors: list[str] = []
    for first_line, block in fenced_blocks(text):
        index = 0
        while index < len(block):
            columns = box_columns(block[index])
            lefts = [column for ch, column in columns if ch == "┌"]
            rights = [column for ch, column in columns if ch == "┐"]
            if not lefts or not rights:
                index += 1
                continue
            left, right = lefts[0], rights[-1]
            closed = index + 1
            while closed < len(block) and not any(
                ch == "└" and column == left
                for ch, column in box_columns(block[closed])
            ):
                closed += 1
            if closed == len(block):
                errors.append(
                    f"{rel}:{first_line + index}: box starting at column {left} has no closing border"
                )
                index += 1
                continue
            for offset in range(index, closed + 1):
                line_columns = box_columns(block[offset])
                if not line_columns:
                    continue
                line_lefts = [c for ch, c in line_columns if ch in BOX_LEFT]
                line_rights = [c for ch, c in line_columns if ch in BOX_RIGHT]
                if line_lefts and line_lefts[0] != left:
                    errors.append(
                        f"{rel}:{first_line + offset}: left border at column {line_lefts[0]}, expected {left}"
                    )
                if line_rights and line_rights[-1] != right:
                    errors.append(
                        f"{rel}:{first_line + offset}: right border at column {line_rights[-1]}, "
                        + f"expected {right} (display width {display_width(block[offset])})"
                    )
            # A ┴/▼ on the top border must line up with the connector above it.
            for column in [c for ch, c in columns if ch in "┴▼"]:
                above = index - 1
                while above >= 0 and not box_columns(block[above]):
                    above -= 1
                connector = (
                    [c for _, c in box_columns(block[above])] if above >= 0 else []
                )
                if column not in connector:
                    errors.append(
                        f"{rel}:{first_line + index}: top border connector at column {column} has no vertical line above it"
                    )
            index = closed + 1
    return errors


def main() -> int:
    errors: list[str] = []
    for path in markdown_files():
        rel = os.path.relpath(path, ROOT)
        text = read_text(path)
        errors.extend(diagram_errors(rel, text))
        for match in LINK.finditer(text):
            label, target = match.group(1), match.group(2)
            target = target.split("#", 1)[0]
            if not target or target.startswith(SKIP_PREFIXES):
                continue
            if target.endswith(".md"):
                resolved = os.path.normpath(os.path.join(os.path.dirname(path), target))
                if not os.path.exists(resolved):
                    errors.append(f"{rel}: missing link target {target}")
                    continue
                if not is_chinese_doc(rel):
                    continue
                target_rel = os.path.relpath(resolved, ROOT)
                chinese = PAIRS.get(target_rel)
                if chinese is None:
                    continue
                chinese_path = os.path.join(ROOT, chinese)
                if os.path.exists(chinese_path) and (
                    "English" not in label and "英文" not in label
                ):
                    errors.append(
                        f"{rel}: links to English {target_rel}; link {chinese} or label the link English/英文"
                    )

    # Every paired document must have a language switcher in both directions.
    for english, chinese in PAIRS.items():
        english_path = os.path.join(ROOT, english)
        chinese_path = os.path.join(ROOT, chinese)
        if not os.path.exists(english_path) or not os.path.exists(chinese_path):
            continue
        english_head = "".join(read_leading_lines(english_path, 8))
        chinese_head = "".join(read_leading_lines(chinese_path, 8))
        if "简体中文" not in english_head:
            errors.append(f"{english}: missing 简体中文 language switcher")
        if "English" not in chinese_head:
            errors.append(f"{chinese}: missing English language switcher")

    if errors:
        print("Documentation check failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(
        f"Documentation links, Chinese/English pairing, and framed-diagram alignment OK ({len(markdown_files())} files)"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
