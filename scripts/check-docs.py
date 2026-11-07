#!/usr/bin/env python3
"""Check local Markdown links and Chinese/English link consistency.

Fails if:
- a relative file link points at a missing path;
- a Chinese document links to a known English canonical document without
  linking its Chinese counterpart (unless the link text says English/英文).

English canonical documents may always be linked from the English docs. The
Chinese docs must either link the translated file or make the English target
explicit.
"""

from __future__ import annotations

import os
import re
import sys

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
    result = []
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
        or rel in {"README.zh-CN.md", "SECURITY.zh-CN.md", "CONTRIBUTING.zh-CN.md", "SUPPORT.zh-CN.md"}
    )


def main() -> int:
    errors: list[str] = []
    for path in markdown_files():
        rel = os.path.relpath(path, ROOT)
        text = open(path, encoding="utf-8").read()
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
                if os.path.exists(chinese_path):
                    if ("English" not in label and "英文" not in label):
                        errors.append(
                            f"{rel}: links to English {target_rel}; link {chinese} "
                            f"or label the link English/英文"
                        )

    # Every paired document must have a language switcher in both directions.
    for english, chinese in PAIRS.items():
        english_path = os.path.join(ROOT, english)
        chinese_path = os.path.join(ROOT, chinese)
        if not os.path.exists(english_path) or not os.path.exists(chinese_path):
            continue
        english_head = "".join(open(english_path, encoding="utf-8").read().splitlines(True)[:8])
        chinese_head = "".join(open(chinese_path, encoding="utf-8").read().splitlines(True)[:8])
        if "简体中文" not in english_head:
            errors.append(f"{english}: missing 简体中文 language switcher")
        if "English" not in chinese_head:
            errors.append(f"{chinese}: missing English language switcher")

    if errors:
        print("Documentation check failed:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(f"Documentation links and Chinese/English pairing OK ({len(markdown_files())} files)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
