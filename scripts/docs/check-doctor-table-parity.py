#!/usr/bin/env python3
"""Guard: every braid doctor check has a row in docs/commands/doctor.md.

`braid doctor` runs an orchestrated set of checks (`cli/src/doctor.rs#run_doctor`);
each emits a named row in `--json`. The "What it checks" table in
`docs/commands/doctor.md` is the canonical operator reference for those names.
Nothing structural tied the two together, so a check could be added in code and
silently never documented -- which is exactly how `mountpoint_immutable` drifted.
This converts "remember to document the new check" into "CI fails if you don't",
matching braid's other docs<->code guards (`check-output-ascii.py`,
`check-see-paths.py`, `check-code-doc-anchors.py`, ...).

Invariant (bidirectional): the set of check names emitted by `run_doctor` equals
the set of names documented as table rows. A code-only name is an undocumented
check; a docs-only name is a stale row.

Code-side source of truth: the `expected_names` inventory in
`cli/src/doctor.rs#valid_config_parses_ok_declared_disks_skips`. That unit test
asserts `expected_names == ` the names `run_doctor` actually emits
(`assert_eq!(actual_names, expected_names)`), so it is the complete check
inventory *by construction* -- but only while that test runs. The just recipe and
CI job run the binding test in the same lane *before* this guard, so the inventory
this script reads cannot silently go stale. (The human-label match arms in
`format_doctor_human_with` were rejected as the source: their `other => other`
catch-all renders a row for any unlabeled check, so a check missing both its label
arm and its doc row would be invisible to a label-arm scan -- the very drift this
guard catches.)

Docs-side source of truth: the first-column backtick token of each row under the
`## What it checks` heading, up to the next `## ` heading.

Fail closed: if the `expected_names` block cannot be located (test renamed or
restructured) the guard exits non-zero with a clear message rather than comparing
against an empty set -- a silent empty-set pass would defeat the guard. The
`--selftest` exercises this path explicitly.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]

DOCTOR_RS = ROOT / "cli" / "src" / "doctor.rs"
DOCTOR_MD = ROOT / "docs" / "commands" / "doctor.md"

# The `let expected_names: Vec<&str> = vec![ ... ];` block. Non-greedy up to the
# first `];`; the inventory contains only string literals, never `];`.
EXPECTED_NAMES_RE = re.compile(
    r"let\s+expected_names\s*:\s*Vec<&str>\s*=\s*vec!\[(.*?)\];",
    re.DOTALL,
)
# A check name is a lowercase snake_case string literal inside that block.
NAME_LITERAL_RE = re.compile(r'"([a-z0-9_]+)"')

# A table row's first column: a backtick-wrapped snake_case check name. The
# header (`| Check |`) and separator (`| --- |`) rows have no leading backtick,
# so they are skipped.
DOC_ROW_RE = re.compile(r"^\|\s*`([a-z0-9_]+)`")
DOC_SECTION_HEADING = "## What it checks"


class ParityError(Exception):
    """Raised when the code-side inventory cannot be parsed -- fail closed."""


def parse_expected_names(source: str) -> set[str]:
    """Extract the doctor check inventory from `expected_names` source text.

    Takes source text (not a path) so `--selftest` can feed it fixtures. Raises
    `ParityError` rather than returning an empty set when the block is absent or
    holds no literals, so a parser miss fails the guard instead of passing it.
    """
    m = EXPECTED_NAMES_RE.search(source)
    if not m:
        raise ParityError(
            "could not locate the `let expected_names: Vec<&str> = vec![...]` block "
            "in cli/src/doctor.rs#valid_config_parses_ok_declared_disks_skips -- the "
            "test may have been renamed or restructured. Refusing to compare against "
            "an empty set (fail closed)."
        )
    names = set(NAME_LITERAL_RE.findall(m.group(1)))
    if not names:
        raise ParityError(
            "located the expected_names block but it held no check-name literals "
            "(fail closed)."
        )
    return names


def parse_doc_names(text: str) -> set[str]:
    """Collect the check names documented as rows under `## What it checks`."""
    names: set[str] = set()
    in_section = False
    for line in text.splitlines():
        if line.startswith("## "):
            in_section = line.strip() == DOC_SECTION_HEADING
            continue
        if not in_section:
            continue
        m = DOC_ROW_RE.match(line)
        if m:
            names.add(m.group(1))
    return names


def evaluate(code_source: str, docs_text: str) -> tuple[int, list[str]]:
    """Compare the code and docs inventories; return (exit_code, messages).

    Fails closed: a `ParityError` from the code-side parser yields exit 1, never
    a silent pass.
    """
    try:
        code_names = parse_expected_names(code_source)
    except ParityError as e:
        return 1, [str(e)]

    doc_names = parse_doc_names(docs_text)
    code_only = sorted(code_names - doc_names)
    docs_only = sorted(doc_names - code_names)

    msgs: list[str] = []
    if code_only:
        msgs.append("checks emitted by run_doctor but missing a docs row (undocumented):")
        msgs.extend(f"  - {n}" for n in code_only)
    if docs_only:
        msgs.append("doctor.md rows with no matching doctor check (stale):")
        msgs.extend(f"  - {n}" for n in docs_only)
    return (1 if msgs else 0), msgs


# ---------------------------------------------------------------------------
# Selftest -- proves the parse + compare logic (including the fail-closed path)
# over in-memory fixtures before the guard runs against the tree.
# ---------------------------------------------------------------------------


def _code_fixture(names: list[str]) -> str:
    body = "\n".join(f'            "{n}",' for n in names)
    return (
        "    #[test]\n"
        "    fn valid_config_parses_ok_declared_disks_skips() {\n"
        "        let expected_names: Vec<&str> = vec![\n"
        f"{body}\n"
        "        ];\n"
        "        assert_eq!(actual_names, expected_names);\n"
        "    }\n"
    )


def _docs_fixture(names: list[str]) -> str:
    rows = "\n".join(f"| `{n}` | does {n} |" for n in names)
    return (
        "## What it checks\n\n"
        "| Check | What it does |\n"
        "| --- | --- |\n"
        f"{rows}\n\n"
        "## Flags\n"
    )


def _selftest() -> int:
    failures: list[str] = []

    def check(name: str, ok: bool) -> None:
        if not ok:
            failures.append(name)

    base = ["config_file", "declared_disks", "wake_on_lan"]

    # (a) a matching code/doc pair passes.
    code, _ = evaluate(_code_fixture(base), _docs_fixture(base))
    check("matching pair passes", code == 0)

    # (b) an injected code-only name fails, naming the undocumented check.
    code, msgs = evaluate(
        _code_fixture(base + ["mountpoint_immutable"]), _docs_fixture(base)
    )
    check(
        "code-only name fails",
        code == 1 and any("mountpoint_immutable" in m for m in msgs),
    )

    # (c) an injected docs-only name fails, naming the stale row.
    code, msgs = evaluate(
        _code_fixture(base), _docs_fixture(base + ["stale_check"])
    )
    check(
        "docs-only name fails",
        code == 1 and any("stale_check" in m for m in msgs),
    )

    # (d) fail-closed: an absent expected_names block fails with the clear
    #     "could not locate" message, never a silent empty-set pass.
    code, msgs = evaluate("fn unrelated() {}\n", _docs_fixture(base))
    check(
        "fail-closed on absent block",
        code == 1 and any("could not locate" in m for m in msgs),
    )

    # (d') fail-closed: a present-but-empty block also refuses to pass.
    empty_block = "        let expected_names: Vec<&str> = vec![\n        ];\n"
    code, _ = evaluate(empty_block, _docs_fixture(base))
    check("fail-closed on empty block", code == 1)

    if failures:
        print("doctor table parity selftest FAILED:", file=sys.stderr)
        for name in failures:
            print(f"  - {name}", file=sys.stderr)
        return 1
    print("doctor table parity selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    code_source = DOCTOR_RS.read_text(encoding="utf-8")
    docs_text = DOCTOR_MD.read_text(encoding="utf-8")
    exit_code, msgs = evaluate(code_source, docs_text)
    if exit_code != 0:
        print("doctor table parity check failed:", file=sys.stderr)
        for m in msgs:
            print(m, file=sys.stderr)
        return exit_code
    print("doctor table parity check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
