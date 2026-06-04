# Plan: codify the "CLI Output Style" rule as a CI guard

## Context

A review finding flagged five em-dashes in `cli/src/enroll_key_file.rs` test
comments as violating the project's ASCII style rule, and proposed replacing
them with ` -- `. Investigation showed the finding was misframed and
under-scoped:

- **The cited rule does not govern comments.** `AGENTS.md` "CLI Output Style"
  bans em-dash specifically in *user-facing CLI output* ("error messages, help
  text, TUI strings, shell `echo` lines"), with a stated rationale (renders
  badly over SSH / non-UTF-8 locales). Test comments never reach an operator.
- **The cited line numbers were stale** (drifted ~50-260 lines); the actual
  em-dashes are at other lines, all in comments.
- **Em-dash is pervasive, not localized:** ~172 lines across 34 `cli/src`
  files, ~318 in `tests/`, ~300 in `docs/` + `AGENTS.md`, ~1,696 in frozen
  `plans/impl/` records -- essentially all in comments, test-assertion
  strings, dev rustdoc, and markdown prose.
- **The rule that actually matters is already fully satisfied.** Verified
  end-to-end: zero typographic Unicode reaches operators via stdout/stderr
  macros, clap `--help` (builder *and* the lone derive file `main.rs`), TUI
  display strings, or `modules/*.nix` generated `echo` lines. The only
  non-comment em-dashes in `cli/src` are six test-assertion messages.
- **No ASCII guard exists.** The only related tool, `scripts/docs/check-see-paths.py`,
  explicitly *accepts* em-dash (`U+2014`) as a valid `## See` separator.

**Decision (user-selected):** Do not sweep comments/test-strings/prose. Instead,
**add a CI guard that enforces the existing checked-in output rule** going
forward, broadened to the full set of typographic Unicode substitutes. This
dissolves the real failure mode (an operator-facing typographic char regressing
in) with near-zero churn, without inventing a new comment-formatting policy the
project never adopted. The guard starts green because output is already clean,
so it doubles as the regression test for that claim.

## Scope

- **Charset (denylist `D`):** em-dash `U+2014`, en-dash `U+2013`, curly quotes
  `U+2018 U+2019 U+201C U+201D`, ellipsis `U+2026`, multiplication sign
  `U+00D7`. These are the "plain ASCII" substitutes from the global style rule
  (`--`, straight quotes, `...`, `4x`). **Rendering Unicode is deliberately
  excluded** and must never be flagged: arrows (`→`), box-drawing
  (`─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼`), braille spinner glyphs, `°`, `▶`, `·`. None of
  these are in `D`, so a denylist (not an ASCII-allowlist) needs no per-file
  carve-outs.
- **Files scanned:** tracked `cli/src/**/*.rs` and `modules/**/*.nix` -- the
  surfaces that emit operator-facing output. **Excluded:** `tests/` and
  `cli/tests/` (dev-facing), `cli/src/test_fixtures/**` (dev fixtures, confirmed
  present), `cli/tests/fixtures/` and `reference/` (authoritative captures).
  Within scanned `.rs` files, **every `#[cfg(test)]`-gated item or block** is
  skipped before any check -- not just `mod tests` and `#[test]` fns, but also
  test-only helper items (`#[cfg(test)] enum`/`fn`/`impl`/`use`, e.g.
  `cli/src/confirm.rs#Verdict` and `cli/src/monitor.rs`'s test-only `use`) and
  `#[cfg(test)]` blocks *inside* production fns (e.g.
  `cli/src/status_tag.rs#emit_status`). Only the gated span is skipped, so the
  production code surrounding an inline gated block -- such as `emit_status`'s real
  `eprint!` -- is still scanned. Test code is out of scope even when it lives
  beside production code.
- **Out of scope (intentionally untouched):** comment em-dashes, test-assertion
  strings, `docs/` + `AGENTS.md` prose, frozen `plans/impl/` records,
  `flake.nix` (per the existing decision, pure-script checks live in CI, not
  `nix flake check`).

## Deliverables

### 1. New checker: `scripts/docs/check-output-ascii.py` (create)

Model it structurally on `scripts/docs/check-see-paths.py#main`: stdlib-only,
`ROOT = Path(__file__).resolve().parents[2]`, `main()` returns `0`/`1`, prints
`"output ascii check ok"` to stdout or `"output ascii check failed:"` + one
`path:line: message` per error to stderr, ends with `raise SystemExit(main())`.

**Approach (pivoted from a sink list to a lexical scan).** Rather than enumerate
output-producing calls (`println!`, `bail!`, ...) -- a list that silently misses
`thiserror` `#[error(...)]` display strings, intermediate message variables, and
preview/TUI renderers -- the checker classifies each character as **code
context** vs **comment context** and flags any char from `D` in the code context
of production, non-test Rust. Because Rust identifiers and operators cannot
contain a `D` char, "`D` char in code context" means it sits inside a string or
char literal -- i.e. user-facing text: output macros, `#[error]` /
`#[command(about=...)]` / `#[arg(help=...)]` attribute strings, `format!` args,
error `.context(...)`, preview renderers, TUI `Span`/`Line` text, and message
variables alike. This is both broader (no output surface left to forget) and
tighter (it never touches comments) than a sink list.

A minimal per-file lexical scanner with cross-line state:

- Tracks string/char literals (`"..."`, `r"..."` / `r#"..."#`, and `\`-continued
  multi-line strings) and block comments (`/* ... */`) across lines, so a `D`
  char on a *continuation* line of a multi-line `#[error(...)]` body **is**
  flagged (see `cli/src/luks.rs#LuksError` -- multi-line `#[error]` is the norm),
  and `//` inside a string (e.g. `"http://"`) does not start a comment.
- **Comment context is exempt** (`//`, `/* */`, and `///`) -- this is what keeps
  the no-comment-sweep decision intact.
- **Exception -- clap-rendered help.** `///` doc comments become `--help` text, so
  scan `///` lines *only* when attached to a clap-rendered declaration: a
  field/variant inside, or the doc block immediately preceding, an item whose
  attributes include `#[derive(... Parser | Subcommand | Args ...)]`. Track these
  regions by brace depth from the derive marker. `main.rs` uses `///` for
  `Commands` variant help (so it must be scanned), while ordinary internal `///`
  -- e.g. on `main.rs`'s `LockPolicy` enum and `lock_policy` fn -- stays exempt.
  This replaces the earlier "any `///` in a derive file" rule, which would have
  wrongly flagged those internal docs.
- **Skip test code entirely** before any check: `cli/src/test_fixtures/**` files,
  and **every `#[cfg(test)]`-gated item or block** -- `mod tests`, `#[test]` fns,
  test-only helper items (`#[cfg(test)] enum`/`fn`/`impl`/`use`), and
  `#[cfg(test)]` blocks nested inside production fns. Track the gated span from the
  attribute by brace depth, ending at the matching `}` or (for braceless items
  like `use`) the `;`, and skip **only** that span -- production code after an
  inline gated block (e.g. the `eprint!` in `cli/src/status_tag.rs#emit_status`)
  is still scanned. Dev-facing test `println!`/asserts must never fail CI.
- **Nix (`modules/**/*.nix`):** keep the simpler `echo ` line scan -- generated
  shell has no comment/string ambiguity worth a tokenizer.

**Acceptance: must start green on the current tree.** Verified clean today: all
195 `cli/src` `#[error(...)]` bodies are denylist-free, and the only non-comment
`D` chars in `cli/src` are six test-assertion messages -- all inside `#[cfg(test)]`
modules, hence skipped. If the scan flags anything else, it is a real violation
to fix before committing.

**Residual gaps (document in the script header; far smaller than the sink-list
version).** A line-oriented scanner may still mishandle exotic Rust lexing
(deeply nested raw-string hashes; `#[doc = "..."]` used instead of `///` -- though
that is an attribute string, hence scanned as code anyway). If green-start ever
surfaces a legitimate non-display production string needing a `D` char (none
exist today), add a narrow trailing opt-out marker (e.g. `// ascii-guard: allow`)
rather than broadening the exemptions. The rule stays partly human-reviewed.

**Mandatory `--selftest`.** The lexical logic needs durable regression coverage,
so `--selftest` is **required** and runs **first** in both the `just` recipe and
the CI job; a selftest failure fails before the tree scan. It must assert:
- *Positive (flagged):* a `D` char in `eprintln!(...)`, in a multi-line
  `#[error("...")]` body, in a clap-rendered `///` help line, in an intermediate
  `let msg = format!("...")`, in production code immediately *after* an inline
  `#[cfg(test)]` block in the same fn (proving only the gated span is skipped),
  and in a `modules/*.nix` `echo`.
- *Negative (passes):* a `D` char in a plain `//` comment, in a non-clap `///`
  (e.g. on `LockPolicy`), in a `#[test]` assertion, in a `#[cfg(test)]` helper
  item (a test-only `enum`/`fn`), in a `#[cfg(test)]` block nested in a production
  fn, and in a `cli/src/test_fixtures/` file; plus allowed rendering chars (`°`,
  `→`, box-drawing) in a TUI `Span`.

### 2. Wiring (mirror `check-see-paths.py`)

- **`justfile`** (edit): add a recipe immediately after `check-docs-see-paths`,
  running the mandatory selftest before the tree scan:
  ```just
  # Guard: no typographic Unicode in user-facing CLI output (selftest first)
  check-output-ascii:
      python3 scripts/docs/check-output-ascii.py --selftest
      python3 scripts/docs/check-output-ascii.py
  ```
- **`.github/workflows/checks.yml`** (edit): add a step/job mirroring the
  always-on `docs-see-paths` job, running `--selftest` first then the tree scan
  (`python3 scripts/docs/check-output-ascii.py --selftest && python3
  scripts/docs/check-output-ascii.py`) -- no `paths:` filter, no `nix develop`,
  stdlib Python only.
- **Do not touch `flake.nix`** -- `checks.<system>` is VM-only by design.

### 3. `AGENTS.md` "CLI Output Style" (edit)

Broaden the rule text from em-dash-only to the full denylist `D` (em-dash,
en-dash, curly quotes, ellipsis, `x` not the multiplication sign), keep the
`--`/SSH rationale, and note it is enforced by
`scripts/docs/check-output-ascii.py`. This keeps doc and guard in sync, as
required when behavior changes.

### 4. No code sweep

Output is already clean, so there is no production-string edit. Comments, test
strings, and prose are intentionally left as-is.

## Critical files

| File | Action | Notes |
| --- | --- | --- |
| `scripts/docs/check-output-ascii.py` | create | lexical code/comment scan + mandatory `--selftest`; structure from `scripts/docs/check-see-paths.py#main` |
| `justfile` | edit | new `check-output-ascii` recipe by the other `check-*` recipes |
| `.github/workflows/checks.yml` | edit | new step/job like `docs-see-paths` |
| `AGENTS.md` | edit | broaden "CLI Output Style" charset + cite the guard |

## Verification

1. `python3 scripts/docs/check-output-ascii.py --selftest` -> all positive and
   negative cases pass (mandatory; runs first in the recipe and CI).
2. `python3 scripts/docs/check-output-ascii.py` -> exits `0` on the current tree.
   This run *is* the regression test for the "output is already clean" claim.
3. Inject-and-revert (confirm it *fails* on real violations): temporarily add a
   `D` char to (a) an `eprintln!` in a `cli/src` fn, (b) a multi-line
   `#[error("...")]` body in `cli/src/luks.rs`, (c) a `Commands` variant `///`
   help line in `main.rs`, and (d) a `modules/*.nix` `echo` -- each must produce a
   `path:line:` error; revert all.
4. Confirm *no* false positives: a `D` char in a plain `//` comment, in a
   non-clap `///` (e.g. `main.rs`'s `LockPolicy`), in a `#[cfg(test)]`
   `assert_eq!` message, and in a `cli/src/test_fixtures/` file all stay green;
   a TUI `Span` with `°`/`→` stays green.
5. `python3 scripts/docs/check-see-paths.py` still green (no interaction with the
   new guard).
6. `just check-output-ascii` runs selftest + scan; confirm the CI step exists
   (`rg check-output-ascii .github/workflows/checks.yml justfile`).
7. No Rust source changed -> no `cargo` run required.

## Implementation notes

- Implemented the `// ascii-guard: allow` escape marker the plan named as the
  prescribed future remedy, rather than leaving it documentation-only -- a line
  carrying the marker is exempt, and a selftest case covers it. This keeps the
  script-header documentation honest (doc and code in sync).
- CI wiring is a separate always-on top-level job (`output-ascii`) mirroring
  `docs-see-paths`, not an extra step on the existing job. The plan said
  "step/job"; a separate job keeps the guard independent and runs the selftest
  first via a `run: |` block.
- File enumeration uses a filesystem glob (`cli/src` `rglob('*.rs')`,
  `modules` `rglob('*.nix')`), matching `check-see-paths.py`'s style and keeping
  the script stdlib-only with no `git` subprocess. The plan said "tracked"; under
  `cli/src` and `modules` the glob set equals the tracked set (no untracked
  sources live there), so the two are equivalent here.
- The per-file Rust lexer is a small class (`_RustScan`) so the cross-line
  string/comment mode, brace depth, test-skip span, clap region, and pending-doc
  buffer travel together instead of as a pile of nonlocals.

## Follow Up

- `ValueEnum` variant `///` docs also render into clap `--help`, but the guard
  scans clap help only for `Parser`/`Subcommand`/`Args` derives. No `ValueEnum`
  variant in the tree carries doc help today (`cli/src/progress.rs#ProgressMode`
  has bare variants), so coverage is currently complete; if a `ValueEnum` variant
  gains a `///` line, extend `CLAP_DERIVE_RE` in
  `scripts/docs/check-output-ascii.py` to include `ValueEnum`.
