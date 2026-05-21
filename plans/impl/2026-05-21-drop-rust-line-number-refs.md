# Plan: drop embedded Rust `.rs` line-number refs from comments

## Context

A `/verify-issue` review of `tests/module/pool-lock-replace-contention.{nix,py}`
surfaced two defects in the test preamble:

1. The prose attributed the lock to a non-existent `braid-wrapper.sh`.
2. The Why bullet pinned the regression at `cli/src/replace.rs:877` and
   `:476`, but `replace.rs` has grown to 6385 lines and those line numbers
   no longer match the cited symbols (`check_no_pending_operation` is now
   at line 1165; `journal::write_journal` is at line 592).

Commit `cd2fd12` ("docs(test): align pool-lock prose with rust dispatch",
2026-05-20) already swept the "wrapper lock" wording across all four
files the finding called out. The stale line numerals were untouched.

A wider grep shows this is one instance of a codebase-wide pattern: 41
embedded `*.rs:NN` line refs across `cli/src/` and `tests/` (37 in
first-party code after excluding 4 `tui/probe.rs` refs into
`reference/hddfancontrol/`), plus 3 same-comment `:NN` shorthand
continuations. Spot-checks of `journal.rs:107` (citing `replace.rs:707`
-> actually 906), `replace.rs:3331` (citing `replace.rs:94-98` ->
struct fields, not the referenced guard), and `replace.rs:3681` (citing
`replace.rs:224-229` -> struct decl, not the referenced boundary)
confirm they drift constantly.

**Outcome:** Eliminate the class of defect by dropping every embedded
`*.rs:NN` numeral from comments under `cli/src/` and `tests/`. Keep the
path and the surrounding symbol name -- both are durable handles that
let a reader grep to the right place. Same-comment shorthand
continuations (e.g. `add.rs:348, ... at :365` or `replace.rs:327` /
`:359`) drop with the explicit ref they extend.

Out of scope: refs into `reference/*` (vendored upstream `.c` / `.rs` /
`.py` checkouts). Those numerals do rot on the next
`just fetch-references`, but the vendored trees are stable between
refreshes and the cited line ranges often surround prose-relevant
upstream comment context that would be awkward to re-cite by symbol
alone. Sweeping them is a separate, larger judgement call; this plan
stays inside the project's own first-party comment surface.

Also out of scope: ref forms that are not `*.rs:NN` -- `*.nix:NN`,
`*.md:NN`, `*.py:NN`, and so on. The defect shape (line numbers drift
in heavy churn) is dominant for the Rust CLI files; the other suffixes
churn less and are addressed separately if needed.

This is a doc-only change. No code behavior, no test logic, no public
interface changes.

## Scope

Sweep 40 instances total: 37 explicit `*.rs:NN` refs plus 3 same-comment
`:NN` shorthand continuations. Counts verified against the current tree
with the post-revision lint commands in the Verification section.
Per-file inventory:

**cli/src/** -- explicit `*.rs:NN` refs (23 hits across 13 files; from
`grep -rnE '[a-z_]+\.rs:[0-9]+([-:][0-9]+)?' cli/src/ | grep -v 'reference/'`)
- `remove.rs:1536, 2565, 2773`
- `add.rs:5485, 8085`
- `monitor.rs:410, 522`
- `journal.rs:107, 108`
- `discover.rs:560`
- `probe_mapper_uuid.rs:14`
- `replace.rs:3331, 3681, 6014` (self-refs into the same file)
- `idle.rs:645`
- `recover.rs:14151, 14457, 14605`
- `pool.rs:414`
- `probe.rs:889` (self-ref)
- `test_fixtures/enroll_key_file.rs:196`
- `test_fixtures/mount.rs:49, 273`

**cli/src/** -- shorthand `:NN` continuations (3 hits across 3 files;
from `grep -rnE '[[:space:]`(/]:[0-9]{2,}' cli/src/` -- literal
backtick, single-quoted)
- `add.rs:8085` -- `:365` continues `add.rs:348` on the same line.
- `recover.rs:14151` -- `:359` continues `cli/src/replace.rs:327` on
  the same line.
- `replace.rs:988` -- `` `:2697` `` is a bare-shorthand for
  `recover.rs:2697` (the `finish_uncommitted_replace_recovery` recovery
  arm). The file isn't in the explicit-ref list because there is no
  explicit `recover.rs:NN` earlier in the comment; rewrite to name the
  function only (`finish_uncommitted_replace_recovery`'s recovery arm).

**tests/** -- explicit `*.rs:NN` refs in `.py`/`.nix` preambles and
inline comments (14 hits across 11 files)
- `module/pool-lock-replace-contention.py:7, 8, 9` (the originally cited
  preamble; line 9 is `state_io.rs:62`, which still happens to match the
  current file but goes the same way for consistency)
- `module/ups-lb-during-remove-missing.py:194`
- `module/ups-lb-during-replace.py:282`
- `cli/braid-unlock.py:523`
- `cli/unlock-uuid-mismatch.nix:7`
- `cli/unlock-uuid-mismatch.py:9`
- `cli/add-passphrase-mismatch.py:9, 10`
- `cli/remove-no-membership.py:8`
- `cli/recover-bootstrap-crash.py:6`
- `cli/replace-passphrase-mismatch.py:13`
- `repro/btrfs-replace-rejected-during-scrub.py:114`

**Explicitly excluded** (do not touch):
- `cli/src/tui/probe.rs:541, 575, 600, 633` -- all four point into
  `reference/hddfancontrol/`. Vendored upstream refs are out of scope
  (see Context).
- `tests/repro/btrfs-replace-rejected-during-scrub.py:11, 13, 19` --
  these are `reference/btrfs-progs/cmds/replace.c:50-64`, a shorthand
  `(:330-356, ...)` continuing it, and a `docs/testing.md:64-72` ref.
  None are `*.rs` refs in first-party code. (Line 114 in the same file
  is a `cli/src/cmd.rs:620-643` ref and IS in scope, listed above.)
- `findings-*.md`, `plans/**`, `self-notes/**`, `docs/decisions/**` --
  point-in-time research artifacts; line refs in dated docs are
  legitimate.
- Any `*.rs:NN` strings that are program data, not comments -- e.g.
  error messages, format strings, test fixtures. None surfaced in the
  greps, but eyeball the final diff before commit.

## Transformation rules

For each instance, apply the lowest-touch rule that works:

1. **Symbol already named alongside the line ref** -- the common case.
   Drop the `:NN` numerals; either drop the now-empty path parens too or
   collapse the prose. Examples:
   - `` `check_no_pending_operation` (cli/src/replace.rs:877) `` ->
     `` `check_no_pending_operation` (cli/src/replace.rs) `` (or just
     drop the parens entirely if the path adds nothing -- prefer the
     shorter form).
   - `the close_mapper_best_effort call (replace.rs:707)` ->
     `the close_mapper_best_effort call in replace.rs`.

2. **Only a line ref, no symbol named** -- read the cited file at the
   stated line, pick the smallest enclosing symbol (function, struct,
   match arm, error variant), and rewrite. Examples:
   - `the close in pool.rs:642` -> `the close in pool.rs's <fn name>`.
   - `(probe.rs:132 -> fs.exists only)` already mentions the API; keep
     `(probe.rs's fs.exists)`.

3. **Self-references inside the same file** -- the surrounding context
   already implies the file. Drop the file+line entirely and refer to
   the symbol by name only. Example:
   - `the old==new guard at replace.rs:94-98` (inside replace.rs at line
     3331) -> `the old==new guard` (if surrounding prose makes the
     target clear) or `the old==new guard in <fn name>`.

4. **Shorthand `:NN` continuations** -- drop with the explicit ref they
   extend. Rewrite to a symbol name (re-using the same path/file
   established by the now-stripped earlier ref). Examples:
   - `(replace.rs:327` / `:359)` -> name both symbols
     (`pool_replace_device` for :327, the matching `pool_resize_device`
     call for :359) or collapse to one phrase
     ("`pool_replace_device` and `pool_resize_device` in
     `cli/src/replace.rs`").
   - `(missing first at add.rs:348, keyfile second at :365)` -> name
     both `eprintln!` callers / functions instead of the line numbers.
   - `` `:2697` `` shorthand -> drop and let the surrounding symbol
     name (`finish_uncommitted_replace_recovery`) carry the citation.

Tone constraint: existing test preambles follow the three-bullet
Intent/Why/Scenario shape from AGENTS.md "Test Conventions". Don't
restructure prose -- only swap the numerals for symbol names where
necessary, keep paragraphs intact.

## Files to read while editing

To apply rules 2 and 4 (find the smallest enclosing symbol for a bare
line ref or a shorthand continuation), read the cited file at the
stated line:

- `cli/src/replace.rs` -- for refs into 94-98, 224-229, 1015-1041,
  329-343, 707, 327, 359, 176
- `cli/src/recover.rs` -- for refs into 2935, 2697
- `cli/src/probe.rs` -- for refs into 132, 47, 125
- `cli/src/pool.rs` -- for refs into 642, 373-388, 310
- `cli/src/cmd.rs` -- for refs into 271-283, 841-858, 620-643, 1004-1014
- `cli/src/mount.rs` -- for refs into 81-91, 88, 690
- `cli/src/main.rs` -- for refs into 707
- `cli/src/add.rs` -- for refs into 298, 323, 348, 365, 1273-1297
- `cli/src/remove.rs` -- for refs into 162-164, 208
- `cli/src/ack.rs` -- for refs into 1018-1022
- `cli/src/remove_missing.rs` -- for refs into 30-58
- `cli/src/test_fixtures.rs` -- for refs into 61

No existing helpers or utilities are involved -- this is a comment
sweep, not a code change.

## Verification

Doc-only change. Behavior verification is structural ("the pattern is
gone"), not functional.

1. **Lint -- explicit first-party `*.rs:NN` refs are gone.** After
   editing, this command must return no output. The `reference/` filter
   is required because the plan deliberately keeps the four
   `tui/probe.rs` -> `reference/hddfancontrol/*.rs:NN` refs, which
   would otherwise show up here as matched output lines:

   ```
   grep -rnE '[a-z_]+\.rs:[0-9]+([-:][0-9]+)?' cli/src/ tests/ \
       | grep -v 'reference/'
   ```

   Edge case: if a single comment line embeds BOTH a first-party
   `cli/src/...rs:NN` ref AND a `reference/...rs:NN` ref, the
   `grep -v reference/` will drop it as a false negative. No such
   line exists today; if one is introduced during the sweep, fix it
   before relying on this lint.

2. **Lint -- same-comment `:NN` shorthand is gone.** This catches the
   continuation pattern (`:365`, `` `:2697` ``, `(:359)`) that survives
   the explicit-ref grep. Note the literal backtick inside the
   character class -- an earlier draft used `\x60`, which `grep -E`
   treats as the three literal characters `x`, `6`, `0` (silently
   missing backtick forms like `` `:2697` ``). Single-quote the
   pattern so the backtick reaches grep verbatim:

   ```
   grep -rnE '[[:space:]`(/]:[0-9]{2,}' cli/src/ tests/
   ```

   Expected remaining hit (do NOT fix -- out of scope):

   - `tests/repro/btrfs-replace-rejected-during-scrub.py:13` -- the
     `(:330-356, gated by ...)` shorthand continues a
     `reference/btrfs-progs/cmds/replace.c:50-64` ref on the line
     above. Both the parent ref and its shorthand are excluded by the
     "no `reference/*` edits" rule.

   Any other hit must be either swept (if it's a first-party Rust
   line-ref shorthand) or confirmed not-a-line-ref (e.g. a literal
   `:NN` inside a string, URL, or unrelated comment). Document any
   non-obvious kept hits in the commit message.

3. **Rustdoc still parses doc comments.** Many touched items are `///`
   doc comments on `pub` / `pub(crate)` items. `cargo test` does not
   validate intra-doc links, so the rustdoc gate is separate. Run
   from the workspace root:

   ```
   RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --document-private-items
   ```

   Must succeed with no warnings. (Use `--document-private-items`
   because most touched comments are on private items.)

4. **Rust still builds and tests pass.** Routine gate -- run:

   ```
   just test-rust
   ```

5. **NixOS test preambles still parse as Python.** The test scripts are
   read via `builtins.readFile` and executed as Python; a syntax error
   in a `# ...` comment can't break them, but a stray unbalanced
   `"""`/`'''` from a botched edit can. Run one VM check end-to-end as
   a smoke test:

   ```
   just test-vm pool-lock-replace-contention
   ```

6. **Eyeball the cited preamble.** Open
   `tests/module/pool-lock-replace-contention.py` and confirm the Why
   bullet reads naturally without the numerals -- this is the
   originally cited file and the canonical worked example.

7. **Diff is doc-only.** Skim `git diff` and confirm no `*.rs` line
   outside a comment changed, and no test logic moved.

## Out of scope (deliberate)

- **No `reference/*` ref edits.** Vendored upstream is its own scope
  (see Context). The `tui/probe.rs` refs into `reference/hddfancontrol/`
  and the `btrfs-replace-rejected-during-scrub.py` ref into
  `reference/btrfs-progs/cmds/replace.c` stay as-is.
- **No non-`.rs` line-ref edits.** Refs of the form `*.nix:NN`,
  `*.md:NN`, `*.py:NN`, etc. are left alone -- the defect shape is
  dominant for the Rust files; the other suffixes are addressed
  separately if needed.
- **No lint added to prevent regression.** A `pre-commit` hook or
  `Justfile` recipe that fails on new `*.rs:NN` line refs would prevent
  the class of defect from coming back, but adding tooling is a
  separate concern from the sweep itself. Track as a follow-up if the
  user wants it.
- **No changes to `findings-*.md`, `plans/**`, `self-notes/**`, or
  `docs/decisions/**`.** Point-in-time docs are allowed to reference
  point-in-time line numbers.
- **No restructuring of prose.** Preserve existing Intent/Why/Scenario
  bullet structure in test preambles; only the numerals change.

## Commit shape

One commit, since the change is uniform doc-only across many files:

```
docs: drop embedded `.rs` line-number refs from comments

cli/src/* and tests/* comments embedded `*.rs:NN` line refs (and
same-comment `:NN` shorthand continuations) alongside symbol names;
the numerals drift as files grow and mislead readers who trust them.
Keep paths and symbol names, drop the numerals. No behavior change.

Out of scope: refs into `reference/*` and non-`.rs` line refs.

See plans/wip/plan-the-ideal-fix-glittery-dawn.md.
```

## Follow Up

- Add a lint/check that rejects new first-party `*.rs:NN` line-number references in comments under `cli/src/` and `tests/`.
- Clean up existing rustdoc warnings in `cli/src/inhibit.rs`, `cli/src/config.rs`, `cli/src/discover.rs`, `cli/src/luks.rs`, `cli/src/parse/btrfs_replace_status.rs`, and `cli/src/types.rs` so `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --workspace --document-private-items` can run as a reliable gate.
