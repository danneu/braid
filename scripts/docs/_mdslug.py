"""Canonical mdBook heading-slug logic, shared by the doc-reference guards.

`check-code-doc-anchors.py` (docs/*.md#anchor cites) and `check-doc-links.py`
(AGENTS.md/README.md `](...)` links) both need to know which `#anchor`s a
Markdown file exposes. Keeping one implementation here means the two guards
cannot drift on what a valid anchor is. stdlib-only; siblings import it because
Python puts a script's own directory on `sys.path` when run as
`python3 scripts/docs/<script>.py`.
"""

from __future__ import annotations

import re
from pathlib import Path


HEADING_RE = re.compile(r"#{1,6}\s+(.*)")
FENCE_PREFIX = "```"


def normalize_id(heading: str) -> str:
    """Slug a heading the way mdBook does: lowercase, keep alnum/`_`/`-`, space -> `-`.

    A deliberate approximation of mdBook's slugger (not vendored in `reference/`),
    already trusted for code-side doc cites and root-doc links.
    Explicit-id headings (`### Foo {#bar}`) are not modeled; no target doc uses
    them today (add `{#id}` handling only if one appears).
    """
    heading = heading.strip()
    out: list[str] = []
    for ch in heading:
        if ch.isalnum() or ch in ("_", "-"):
            out.append(ch.lower())
        elif ch.isspace():
            out.append("-")
    return "".join(out)


def anchors_of(md_path: Path) -> set[str]:
    """Every anchor id a Markdown file exposes, across all heading levels.

    Code-fence aware (a `## ...` inside a ``` block is example text, not a
    heading) and replicates mdBook's duplicate-slug suffixing (the second `foo`
    becomes `foo-1`), so a cite to a de-duped heading still resolves. The result
    is a superset of an H2-only scan, so swapping this in for the old
    earlier principles-only extraction cannot make a previously-valid cite fail.
    """
    seen: dict[str, int] = {}
    anchors: set[str] = set()
    fenced = False
    for line in md_path.read_text(encoding="utf-8").splitlines():
        if line.lstrip().startswith(FENCE_PREFIX):
            fenced = not fenced
            continue
        if fenced:
            continue
        m = HEADING_RE.match(line)
        if not m:
            continue
        base = normalize_id(m.group(1))
        n = seen.get(base, 0)
        anchors.add(base if n == 0 else f"{base}-{n}")  # mdbook: 2nd dup -> -1
        seen[base] = n + 1
    return anchors
