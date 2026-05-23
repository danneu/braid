#!/usr/bin/env python3
"""Validate YAML frontmatter for agent-facing docs."""

from __future__ import annotations

import re
import sys
from pathlib import Path

import yaml


ROOT = Path(__file__).resolve().parents[2]
VALID_STATUSES = {"Active", "Superseded", "Draft", "Deprecated"}


def frontmatter(path: Path) -> tuple[dict[str, object] | None, str | None]:
    text = path.read_text(encoding="utf-8")
    match = re.match(r"\A---\n(.*?)\n---\n", text, re.DOTALL)
    if not match:
        return None, "missing leading YAML frontmatter block"
    try:
        data = yaml.safe_load(match.group(1)) or {}
    except yaml.YAMLError as exc:
        return None, f"invalid YAML: {exc}"
    if not isinstance(data, dict):
        return None, "frontmatter must be a YAML mapping"
    return data, None


def validate(path: Path, require_status: bool) -> list[str]:
    data, error = frontmatter(path)
    if error:
        return [error]

    errors: list[str] = []
    intent = data.get("intent")
    if not isinstance(intent, str) or not intent.strip():
        errors.append("missing non-empty intent")

    status = data.get("status")
    if require_status:
        if status not in VALID_STATUSES:
            errors.append(
                "status must be exactly one of: "
                + ", ".join(sorted(VALID_STATUSES))
            )
    elif status is not None and status not in VALID_STATUSES:
        errors.append(
            "optional status must be exactly one of: "
            + ", ".join(sorted(VALID_STATUSES))
        )
    return errors


def main() -> int:
    checks: list[tuple[Path, bool]] = []
    checks.append((ROOT / "docs/design/principles.md", False))
    checks.extend((path, True) for path in sorted((ROOT / "docs/design/decisions").glob("*.md")))
    checks.extend((path, True) for path in sorted((ROOT / "docs/internals").rglob("*.md")))

    failures: list[str] = []
    for path, require_status in checks:
        for error in validate(path, require_status):
            failures.append(f"{path.relative_to(ROOT)}: {error}")

    if failures:
        print("frontmatter check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("frontmatter check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
