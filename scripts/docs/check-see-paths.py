#!/usr/bin/env python3
"""Validate code-span paths in decision-doc See sections."""

from __future__ import annotations

import io
import re
import sys
import tempfile
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
DECISIONS = ROOT / "docs/design/decisions"
CODE_SPAN_RE = re.compile(r"`([^`]+)`")
LINE_SUFFIX_RE = re.compile(r":\d+(?:-\d+)?$")
DESC_SEPARATORS = (" \u2014 ", " -- ")


def see_section_lines(path: Path) -> list[tuple[int, str]]:
    lines = path.read_text(encoding="utf-8").splitlines()
    in_see = False
    section: list[tuple[int, str]] = []

    for line_no, line in enumerate(lines, start=1):
        if line == "## See":
            in_see = True
            continue
        if in_see and line.startswith("## "):
            break
        if in_see:
            section.append((line_no, line))

    return section


def target_cluster(line: str) -> str:
    split_at: int | None = None
    for separator in DESC_SEPARATORS:
        index = line.find(separator)
        if index != -1 and (split_at is None or index < split_at):
            split_at = index
    if split_at is None:
        return line
    return line[:split_at]


def clean_target(target: str) -> str:
    target = target.split("#", 1)[0]
    target = LINE_SUFFIX_RE.sub("", target)
    return target


def validate_bullet(
    path: Path,
    line_no: int,
    line: str,
    root: Path,
) -> list[str]:
    stripped = line.strip()
    if not stripped.startswith("- "):
        return []
    if "preserved in git history" in line:
        return []

    failures: list[str] = []
    for target in CODE_SPAN_RE.findall(target_cluster(line)):
        cleaned = clean_target(target)
        if not cleaned:
            continue
        if not (root / cleaned).exists():
            failures.append(
                f"{path.relative_to(root)}:{line_no}: unresolved See path `{target}`"
            )
    return failures


def _selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        decisions = root / "docs/design/decisions"
        decisions.mkdir(parents=True)
        (root / "existing.rs").write_text("", encoding="utf-8")
        (decisions / "fixture.md").write_text(
            "\n".join(
                [
                    "# Fixture",
                    "",
                    "## See",
                    "",
                    "- `existing.rs` -- resolves",
                    "- `missing.rs` -- must fail",
                    "",
                    "## Later section",
                    "",
                    "- `also-missing.rs` -- outside See",
                    "",
                ]
            ),
            encoding="utf-8",
        )

        stdout = io.StringIO()
        stderr = io.StringIO()
        with redirect_stdout(stdout), redirect_stderr(stderr):
            status = run_check(root, decisions)

    expected_stderr = (
        "See path check failed:\n"
        "  docs/design/decisions/fixture.md:6: "
        "unresolved See path `missing.rs`\n"
    )
    if status != 1 or stdout.getvalue() or stderr.getvalue() != expected_stderr:
        print("See path selftest FAILED:", file=sys.stderr)
        print(f"  expected status: 1; actual status: {status}", file=sys.stderr)
        print(
            f"  expected stdout: ''; actual stdout: {stdout.getvalue()!r}",
            file=sys.stderr,
        )
        print(
            f"  expected stderr: {expected_stderr!r}; actual stderr: {stderr.getvalue()!r}",
            file=sys.stderr,
        )
        return 1

    print("See path selftest ok")
    return 0


def run_check(root: Path, decisions: Path) -> int:
    failures: list[str] = []
    for path in sorted(decisions.glob("*.md")):
        for line_no, line in see_section_lines(path):
            failures.extend(validate_bullet(path, line_no, line, root))

    if failures:
        print("See path check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("See path check ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()
    return run_check(ROOT, DECISIONS)


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
