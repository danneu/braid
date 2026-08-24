#!/usr/bin/env python3
"""Guard: no typographic Unicode in user-facing CLI output.

Enforces the CLI-output ASCII rule from AGENTS.md going forward: the
operator-facing surfaces of the Rust CLI and the NixOS module must use plain
ASCII (`--`, straight quotes, `...`, `x`) rather than typographic substitutes
that render badly over SSH and in non-UTF-8 locales.

Approach (a lexical code/comment scan, not a sink list). Rather than enumerate
output-producing calls (`println!`, `bail!`, ...) -- a list that silently
misses `thiserror` `#[error(...)]` strings, intermediate message variables, and
preview/TUI renderers -- the checker classifies each character of a production,
non-test Rust file as *code context* vs *comment context* and flags any
denylist char that lands in code context. Because Rust identifiers and
operators cannot contain a denylist char, "denylist char in code context" means
it sits inside a string or char literal -- i.e. user-facing text: output
macros, `#[error]` / `#[command(about=...)]` / `#[arg(help=...)]` attribute
strings, `format!` args, `.context(...)`, preview/TUI text, and message
variables alike. Comments (`//`, `/* */`, plain `///`) are exempt, which keeps
the deliberate no-comment-sweep decision intact. The one exception is clap help:
a `///` doc comment that documents a `#[derive(Parser|Subcommand|Args)]` item
becomes `--help` text, so those doc lines are scanned.

The denylist (`DENY`) is the set of plain-ASCII substitutes from the global
style rule. Rendering Unicode -- arrows, box-drawing, braille spinner glyphs,
the degree sign, the bullet -- is deliberately NOT in `DENY`, so a denylist
(not an ASCII-allowlist) needs no per-file carve-outs for the TUI.

Scanned: tracked `cli/src/**/*.rs` (minus `cli/src/test_fixtures/**`) and
`modules/**/*.nix`. Test code is skipped before any check: every
`#[cfg(test)]`-gated item or block -- `mod tests`, `#[test]` fns, test-only
helper items, and `#[cfg(test)]` blocks nested inside production fns -- is
skipped by its gated span, so dev-facing test strings never fail CI while the
production code surrounding an inline gated block stays scanned.

Escape hatch: a legitimate non-display production string that genuinely needs a
denylist char (none exist today) can carry a trailing `// ascii-guard: allow`
comment on the offending line to suppress the check for that line. Prefer this
narrow marker over broadening the structural exemptions.

Residual gaps (line-oriented scanner; the rule stays partly human-reviewed):
- Multi-line `#[cfg(test)]` / `#[derive(...)]` attributes, and cfg/derive
  attributes not at the start of a line, are classified from their first line
  only; the tree uses the single-line idiomatic form exclusively.
- `cfg` predicates other than a bare `#[cfg(test)]` (e.g. `cfg(all(test, ...))`)
  are not recognized as test gates.
- Byte raw strings (`br"..."`) and `#[doc = "..."]` doc attributes are scanned
  as ordinary code/strings rather than special-cased.
- Clap `///` help is scanned only for `Parser`/`Subcommand`/`Args` derives.
  `ValueEnum` variant docs also render into `--help`, but no `ValueEnum` variant
  in the tree carries doc help today; revisit if one gains a `///` line.
- The Nix scan is a literal `echo` line scan; a denylist char in a Nix `#`
  comment on an `echo` line could false-positive (generated module echo lines
  do not hit this today).
"""

from __future__ import annotations

import re
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]

# Plain-ASCII substitutes banned in operator-facing output. Rendering Unicode
# (arrows, box-drawing, degree sign, bullet, spinner glyphs) is intentionally
# absent so the TUI needs no carve-outs.
DENY = {
    "—": "em dash",
    "–": "en dash",
    "‘": "left single quote",
    "’": "right single quote",
    "“": "left double quote",
    "”": "right double quote",
    "…": "ellipsis",
    "×": "multiplication sign",
}

ALLOW_MARKER = "// ascii-guard: allow"

# A derive whose trait list names any of these renders the item's doc comments
# into `--help` text, so clap `///` help is scanned as user-facing.
CLAP_DERIVE_RE = re.compile(r"\b(Parser|Subcommand|Args)\b")
CFG_TEST_RE = re.compile(r"#!?\[\s*cfg\s*\(\s*test\s*\)\s*\]")
DERIVE_RE = re.compile(r"#!?\[\s*derive\s*\(")


def _is_ident(ch: str) -> bool:
    return ch.isalnum() or ch == "_"


class _RustScan:
    """Per-file lexical scanner carrying cross-line string/comment state.

    Encapsulated as a class because the scan threads brace depth, test-skip
    spans, clap-region tracking, and a pending-doc buffer across every line; a
    flat function would have to surface all of that as nonlocals.
    """

    def __init__(self, rel: str, lines: list[str]):
        self.rel = rel
        self.lines = lines
        self.errors: list[str] = []

        # Cross-line lexical mode.
        self.lex = "code"  # code | str | rawstr | block
        self.raw_hashes = 0
        self.block_depth = 0

        # Region state.
        self.brace_depth = 0
        self.skip = None  # None or {"phase": "await"|"body", "start_depth": int}
        self.clap_pending = False
        self.clap_region_start = None  # inside clap body while brace_depth > this
        self.doc_buffer: list[tuple[int, str]] = []  # tentative (line_no, ch) hits

        self.line_no = 0
        self.line_allow = False  # current line carries the escape marker

    # -- hit recording --------------------------------------------------------

    def _emit(self, line_no: int, ch: str) -> None:
        name = DENY[ch]
        self.errors.append(
            f"{self.rel}:{line_no}: {name} (U+{ord(ch):04X}) in user-facing output"
        )

    def _record(self, ch: str) -> None:
        """Flag a denylist char found in code/string context on the current line."""
        if self.skip is None and not self.line_allow:
            self._emit(self.line_no, ch)

    def _clap_active(self) -> bool:
        return self.clap_region_start is not None and self.brace_depth > self.clap_region_start

    # -- brace / semicolon events ---------------------------------------------

    def _on_open_brace(self) -> None:
        if self.skip is not None:
            if self.skip["phase"] == "await":
                self.skip["phase"] = "body"
            return
        if self.clap_pending:
            self.clap_region_start = self.brace_depth - 1
            self.clap_pending = False

    def _on_close_brace(self) -> None:
        if (
            self.skip is not None
            and self.skip["phase"] == "body"
            and self.brace_depth == self.skip["start_depth"]
        ):
            self.skip = None
            return
        if self.clap_region_start is not None and self.brace_depth == self.clap_region_start:
            self.clap_region_start = None

    def _on_semicolon(self) -> None:
        if (
            self.skip is not None
            and self.skip["phase"] == "await"
            and self.brace_depth == self.skip["start_depth"]
        ):
            self.skip = None

    # -- per-line region transitions (only when the line starts in code mode) -

    def _region_line_start(self, line: str) -> None:
        stripped = line.strip()
        if stripped == "":
            kind = "blank"
        elif stripped.startswith("//"):
            kind = "comment"  # the char loop scans `///` doc lines
        elif stripped.startswith("#[") or stripped.startswith("#!["):
            kind = "attr"
        elif stripped.startswith("/*"):
            kind = "blockcomment"
        else:
            kind = "code"

        if kind == "attr":
            if self.skip is None:
                if CFG_TEST_RE.match(stripped):
                    self.skip = {"phase": "await", "start_depth": self.brace_depth}
                elif DERIVE_RE.match(stripped) and CLAP_DERIVE_RE.search(stripped):
                    # Doc block immediately preceding a clap item is its about-text.
                    for ln, ch in self.doc_buffer:
                        self._emit(ln, ch)
                    self.doc_buffer.clear()
                    self.clap_pending = True
        elif kind == "blank":
            if self.skip is None:
                self.doc_buffer.clear()
        elif kind == "code":
            if self.skip is None and not self.clap_pending:
                self.doc_buffer.clear()
        # "comment" / "blockcomment": neutral (doc lines accumulate in the loop)

    # -- doc-comment scanning -------------------------------------------------

    def _scan_doc(self, line: str, text_start: int) -> None:
        if self.skip is not None or self.line_allow:
            return
        if self._clap_active():
            for k in range(text_start, len(line)):
                if line[k] in DENY:
                    self._emit(self.line_no, line[k])
        else:
            for k in range(text_start, len(line)):
                if line[k] in DENY:
                    self.doc_buffer.append((self.line_no, line[k]))

    # -- main loop ------------------------------------------------------------

    def run(self) -> list[str]:
        for line_no, line in enumerate(self.lines, start=1):
            self.line_no = line_no
            self.line_allow = ALLOW_MARKER in line
            if self.lex == "code":
                self._region_line_start(line)
            self._scan_line(line)
        return self.errors

    def _scan_line(self, line: str) -> None:
        i = 0
        n = len(line)
        while i < n:
            c = line[i]

            if self.lex == "block":
                if c == "*" and i + 1 < n and line[i + 1] == "/":
                    self.block_depth -= 1
                    i += 2
                    if self.block_depth == 0:
                        self.lex = "code"
                    continue
                if c == "/" and i + 1 < n and line[i + 1] == "*":
                    self.block_depth += 1
                    i += 2
                    continue
                i += 1
                continue

            if self.lex == "rawstr":
                if c == '"':
                    if line[i + 1 : i + 1 + self.raw_hashes] == "#" * self.raw_hashes:
                        self.lex = "code"
                        i += 1 + self.raw_hashes
                        continue
                    i += 1
                    continue
                if c in DENY:
                    self._record(c)
                i += 1
                continue

            if self.lex == "str":
                if c == "\\":
                    i += 2  # skip escaped char (or the line-continuation newline)
                    continue
                if c == '"':
                    self.lex = "code"
                    i += 1
                    continue
                if c in DENY:
                    self._record(c)
                i += 1
                continue

            # lex == "code"
            if c == "/" and i + 1 < n and line[i + 1] == "/":
                j = i
                while j < n and line[j] == "/":
                    j += 1
                slashes = j - i
                is_doc = slashes == 3  # `///` outer doc; `////`+ is a plain comment
                if is_doc:
                    self._scan_doc(line, j)
                return  # rest of line is a comment; line comments end at EOL

            if c == "/" and i + 1 < n and line[i + 1] == "*":
                self.lex = "block"
                self.block_depth = 1
                i += 2
                continue

            if c == '"':
                self.lex = "str"
                i += 1
                continue

            if c == "r" and (i == 0 or not _is_ident(line[i - 1])):
                j = i + 1
                h = 0
                while j < n and line[j] == "#":
                    h += 1
                    j += 1
                if j < n and line[j] == '"':
                    self.lex = "rawstr"
                    self.raw_hashes = h
                    i = j + 1
                    continue
                # not a raw string: `r` is an ordinary identifier char

            if c == "'":
                if i + 1 < n and line[i + 1] == "\\":
                    j = i + 2
                    while j < n and line[j] != "'":
                        if line[j] in DENY:
                            self._record(line[j])
                        j += 1
                    i = j + 1 if j < n else n
                    continue
                if i + 2 < n and line[i + 2] == "'":
                    if line[i + 1] in DENY:
                        self._record(line[i + 1])
                    i += 3
                    continue
                # otherwise a lifetime/label tick: ordinary punctuation
                i += 1
                continue

            if c == "{":
                self.brace_depth += 1
                self._on_open_brace()
                i += 1
                continue
            if c == "}":
                self.brace_depth -= 1
                self._on_close_brace()
                i += 1
                continue
            if c == ";":
                self._on_semicolon()
                i += 1
                continue

            if c in DENY:
                # A denylist char in bare code is not valid Rust, but if one
                # appears it is code context -- flag it.
                self._record(c)
            i += 1


def scan_rust_file(rel: str, lines: list[str]) -> list[str]:
    return _RustScan(rel, lines).run()


ECHO_RE = re.compile(r"\becho\b")


def scan_nix_file(rel: str, lines: list[str]) -> list[str]:
    """Flag denylist chars in `echo` lines of generated shell.

    Generated module shell has no comment/string ambiguity worth a tokenizer,
    so a literal `echo` line scan suffices for the operator-facing surface.
    """
    errors: list[str] = []
    for line_no, line in enumerate(lines, start=1):
        if ALLOW_MARKER in line:
            continue
        m = ECHO_RE.search(line)
        if not m:
            continue
        for ch in line[m.end():]:
            if ch in DENY:
                errors.append(
                    f"{rel}:{line_no}: {DENY[ch]} (U+{ord(ch):04X}) in echo output"
                )
                break
    return errors


def _rust_files():
    base = ROOT / "cli" / "src"
    for path in sorted(base.rglob("*.rs")):
        rel = path.relative_to(ROOT).as_posix()
        if "/test_fixtures/" in "/" + rel:
            continue
        yield path, rel


def _nix_files():
    base = ROOT / "modules"
    for path in sorted(base.rglob("*.nix")):
        yield path, path.relative_to(ROOT).as_posix()


def _read_lines(path: Path) -> list[str]:
    return path.read_text(encoding="utf-8").splitlines()


# ---------------------------------------------------------------------------
# Selftest -- durable regression coverage for the lexical logic. Runs first in
# both the just recipe and CI, so a selftest failure fails before the tree scan.
# ---------------------------------------------------------------------------

EM = "—"  # em dash
ELL = "…"  # ellipsis


def _selftest() -> int:
    failures: list[str] = []

    def check(name: str, ok: bool) -> None:
        if not ok:
            failures.append(name)

    def rs(src: str) -> list[str]:
        return scan_rust_file("cli/src/sample.rs", src.splitlines())

    # -- Positive (must flag) --------------------------------------------------

    check(
        "eprintln string",
        len(rs(f'fn f() {{\n    eprintln!("oops {EM} bad");\n}}\n')) >= 1,
    )

    check(
        "multi-line #[error] continuation",
        len(
            rs(
                "#[derive(Debug)]\n"
                "enum E {\n"
                "    #[error(\n"
                '        "line one \\\n'
                f'         line two {EM} bad"\n'
                "    )]\n"
                "    V,\n"
                "}\n"
            )
        )
        >= 1,
    )

    check(
        "clap-rendered /// help",
        len(
            rs(
                "#[derive(Subcommand)]\n"
                "enum Cmd {\n"
                f"    /// Do the {EM} thing\n"
                "    Run,\n"
                "}\n"
            )
        )
        >= 1,
    )

    check(
        "intermediate format! variable",
        len(rs(f'fn g() {{\n    let msg = format!("done {ELL} now");\n    eprintln!("{{msg}}");\n}}\n')) >= 1,
    )

    check(
        "production code after inline #[cfg(test)] block",
        len(
            rs(
                "pub fn emit(line: &str) {\n"
                "    #[cfg(test)]\n"
                "    {\n"
                '        capture(line);\n'
                "    }\n"
                f'    eprintln!("note {EM}");\n'
                "}\n"
            )
        )
        >= 1,
    )

    check(
        "nix echo",
        len(scan_nix_file("modules/sample.nix", f'      echo "stopping {EM} now"\n'.splitlines())) >= 1,
    )

    # -- Negative (must pass clean) -------------------------------------------

    check(
        "plain // comment",
        rs(f"fn f() {{\n    // note {EM} here\n}}\n") == [],
    )

    check(
        "non-clap /// doc",
        rs(f"/// Command-level {EM} discipline\nenum LockPolicy {{\n    None,\n}}\n") == [],
    )

    check(
        "non-clap /// doc on fn",
        rs(f"/// frobnicate the {EM} widget\nfn frob() {{}}\n") == [],
    )

    check(
        "#[test] assertion",
        rs(
            "#[cfg(test)]\n"
            "mod tests {\n"
            "    #[test]\n"
            "    fn t() {\n"
            f'        assert_eq!(a, b, "boom {EM}");\n'
            "    }\n"
            "}\n"
        )
        == [],
    )

    check(
        "#[cfg(test)] helper enum",
        rs(
            "#[cfg(test)]\n"
            "#[derive(Default)]\n"
            "enum Verdict {\n"
            f"    /// test {EM} doc\n"
            "    Unexpected,\n"
            "}\n"
        )
        == [],
    )

    check(
        "#[cfg(test)] helper fn",
        rs(
            "#[cfg(test)]\n"
            "fn helper() -> &'static str {\n"
            f'    "boom {EM}"\n'
            "}\n"
        )
        == [],
    )

    check(
        "#[cfg(test)] helper use (braceless)",
        rs(f'#[cfg(test)]\nuse std::cell::RefCell;\nfn prod() {{\n    eprintln!("{EM}");\n}}\n')
        != []
        and len(
            rs(f'#[cfg(test)]\nuse std::cell::RefCell;\nfn prod() {{\n    let _ = "{EM}";\n}}\n')
        )
        == 1,
    )

    check(
        "#[cfg(test)] block nested in production fn (gated span suppressed)",
        rs(
            "pub fn f() {\n"
            "    #[cfg(test)]\n"
            "    {\n"
            f'        assert!(cond, "x {EM}");\n'
            "    }\n"
            "}\n"
        )
        == [],
    )

    check(
        "rendering Unicode in TUI Span stays clean",
        rs('fn render() {\n    let s = Span::raw("12°C → ─ ok");\n}\n') == [],
    )

    check(
        "escape marker suppresses",
        rs(f'fn f() {{\n    eprintln!("legacy {EM}"); {ALLOW_MARKER}\n}}\n') == [],
    )

    check(
        "escape marker suppresses buffered clap doc on its own line",
        rs(
            f"/// Do the {EM} thing {ALLOW_MARKER}\n"
            "#[derive(Subcommand)]\n"
            "enum Cmd { Run }\n"
        )
        == [],
    )

    check(
        "escape marker on derive does not suppress buffered clap doc",
        len(
            rs(
                f"/// Do the {EM} thing\n"
                f"#[derive(Subcommand)] {ALLOW_MARKER}\n"
                "enum Cmd { Run }\n"
            )
        )
        >= 1,
    )

    check(
        "test_fixtures path excluded from enumeration",
        all("/test_fixtures/" not in "/" + rel for _, rel in _rust_files()),
    )

    if failures:
        print("output ascii selftest FAILED:", file=sys.stderr)
        for name in failures:
            print(f"  - {name}", file=sys.stderr)
        return 1
    print("output ascii selftest ok")
    return 0


def main(argv: list[str]) -> int:
    if "--selftest" in argv:
        return _selftest()

    failures: list[str] = []
    for path, rel in _rust_files():
        failures.extend(scan_rust_file(rel, _read_lines(path)))
    for path, rel in _nix_files():
        failures.extend(scan_nix_file(rel, _read_lines(path)))

    if failures:
        print("output ascii check failed:", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1

    print("output ascii check ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
