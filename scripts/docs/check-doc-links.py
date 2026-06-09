#!/usr/bin/env python3
"""Validate `](...)` markdown links in the repo-root, agent-facing docs.

AGENTS.md and README.md sit outside `docs/`, so mdbook-linkcheck2 (which only
sees the book src) never validates their links, and the sibling guards cover
different forms (`check-code-doc-anchors.py`: docs/*.md#anchor cites in textual
form; `check-see-paths.py`: backticked paths in ADR See sections). This closes
the remaining gap: every inline `](target)` link in those root files is checked
for path existence and, when the target is a `.md#fragment`, for anchor
validity -- catching the silent rot when a linked heading is renamed.

Non-goals:
- Not content accuracy -- a link can resolve to the wrong-but-existing file with
  prose that misdescribes it; that stays a human-audit concern.
- Not `:line` suffixes -- `](file.md:12)` style is not used by these files and a
  line number is not validated.
- No overlap with mdbook-linkcheck2 (never sees root files) or check-see-paths.py
  (backticked code spans in ADR See sections, not `](...)` links).

stdlib-only and nix-free; runs in the all-PR `checks.yml` lane, the only one that
fires on AGENTS.md/README.md edits. `--selftest` drives the real `lint_file()`
over a temp fixture tree, so the test exercises the live scanner end-to-end.
"""

from __future__ import annotations

import re
import sys
import tempfile
from pathlib import Path

from _mdslug import anchors_of


ROOT = Path(__file__).resolve().parents[2]

# Repo-root, agent-facing markdown outside docs/ (so mdbook-linkcheck2 misses
# its links). Extend as new root docs appear.
TARGETS = ["AGENTS.md", "README.md"]

LINK = re.compile(r"\]\(([^)]+)\)")  # matches [text](t) and ![alt](t)
FENCE_PREFIX = "```"


def classify(target: str) -> tuple[str, str | None, str | None]:
    """Sort a raw link target into `('skip', _, _)` or `('check', path, frag)`.

    External schemes and same-page anchors need no path check; everything else is
    a repo-relative reference whose path (and optional `#fragment`) we validate.
    """
    parts = target.strip().split()  # drop an optional `"title"` after the URL
    if not parts:
        return ("skip", None, None)
    url = parts[0]
    if url.startswith(("http://", "https://", "mailto:", "tel:")):
        return ("skip", None, None)
    path, _, frag = url.partition("#")
    return ("check", path, frag or None)


def lint_file(md_path: Path) -> list[str]:
    """Path + anchor failures for every `](...)` link in one markdown file.

    Each link path is resolved relative to the file's own parent dir (so the
    check stays correct if a non-root target is ever added to TARGETS), and links
    inside fenced code blocks are skipped so an example `](...)` is not checked.
    This is the single entry point shared by the live scan and `--selftest`.
    """
    failures: list[str] = []
    try:
        rel = md_path.relative_to(ROOT).as_posix()
    except ValueError:
        rel = md_path.name  # selftest fixtures live outside ROOT
    fenced = False
    for line_no, line in enumerate(md_path.read_text(encoding="utf-8").splitlines(), start=1):
        if line.lstrip().startswith(FENCE_PREFIX):
            fenced = not fenced
            continue
        if fenced:
            continue
        for target in LINK.findall(line):
            kind, path, frag = classify(target)
            if kind == "skip":
                continue
            if path:
                resolved = md_path.parent / path
                if not resolved.exists():
                    failures.append(f"{rel}:{line_no}: unresolved link path `{target}`")
                    continue
            else:
                resolved = md_path  # empty path => same-page anchor
            if frag and resolved.suffix == ".md" and frag not in anchors_of(resolved):
                failures.append(f"{rel}:{line_no}: unresolved link anchor `{target}`")
    return failures


# ---------------------------------------------------------------------------
# Selftest -- validator-level regression coverage. Runs the same lint_file() the
# live scan uses over a temp fixture tree, so it catches the scanner silently
# ceasing to enforce path existence, parent-relative resolution, fenced-code
# skipping, or anchor lookup -- regressions the live scan cannot surface while
# the real tree is clean. Runs first in both the recipe and CI.
# ---------------------------------------------------------------------------

_TARGET_MD = "\n".join(
    [
        "# Title",
        "",
        "## Top section",
        "",
        "[a](dep.md)",  # pass: path ok, no anchor
        "[b](dep.md#known-heading)",  # pass: path + anchor ok
        "[c](sub/child.md)",  # pass: parent-relative path resolves
        "[d](#top-section)",  # pass: same-page anchor (own heading)
        "[e](https://example.com)",  # pass: URL skipped
        "",
        FENCE_PREFIX,
        "[f](dep.md)",  # pass: inside fenced code -> skipped entirely
        FENCE_PREFIX,
        "",
        "[i](dep.md#known-heading-1)",  # pass: dedup second occurrence
        "[g](gone.md)",  # FAIL: unresolved path
        "[h](dep.md#no-such-heading)",  # FAIL: unresolved anchor
        "",
    ]
)

_DEP_MD = "\n".join(
    ["# Dep", "", "## Known heading", "", "first", "", "## Known heading", "", "second", ""]
)


def _selftest() -> int:
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        (root / "sub").mkdir()
        (root / "sub" / "child.md").write_text("# Child\n", encoding="utf-8")
        (root / "dep.md").write_text(_DEP_MD, encoding="utf-8")
        (root / "target.md").write_text(_TARGET_MD, encoding="utf-8")
        failures = lint_file(root / "target.md")

    offending = {re.search(r"`([^`]+)`", f).group(1) for f in failures}
    expected = {"gone.md", "dep.md#no-such-heading"}
    if offending != expected:
        print("doc link selftest FAILED:", file=sys.stderr)
        print(f"  expected failing targets: {sorted(expected)}", file=sys.stderr)
        print(f"  actual failing targets:   {sorted(offending)}", file=sys.stderr)
        for failure in failures:
            print(f"  - {failure}", file=sys.stderr)
        return 1
    print("doc link selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures: list[str] = []
    for name in TARGETS:
        failures.extend(lint_file(ROOT / name))

    if failures:
        print("doc link check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("doc link check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
