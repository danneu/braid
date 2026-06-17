# Pivot: convert internal line-number citations to `path#symbol`

## Context

A review finding flagged two test-comment citations in `cli/src/recover.rs`
(`recover.rs:451`, `recover.rs:1406`) that cite source by line number, which
`docs/dev/doc-citations.md` and `AGENTS.md` "File citations" forbid ("never line
numbers") because line numbers drift on every edit above them and silently mislead
the next reader.

The finding's headline is correct but **undercounts**. A clean scan of all tracked
`.rs`/`.nix`/`.py` files (excluding `reference/` and `plans/`) shows **10 internal
line-number citations across 5 files**, of which **9 are already stale** -- they
resolve to unrelated code today. The finding's own location pointers (`:16437`,
`:16439`) are themselves stale, landing in a test body rather than the preamble --
proof of the exact drift the rule exists to prevent.

The ideal pivot is to fix the **whole class as it exists today**: convert all 10
internal citations to the project's mandated form. This is a comments-only change
-- no behavior, no tests, no API. Scope decisions (confirmed with the user): **no
new enforcement checker** (the true signal is ~10 hits and a robust checker's
exclusion list would dominate and be fragile -- rely on the doc + review); and
**reference/external upstream citations are out of scope** (governed by a different
rule with a different fix shape -- see "Out of scope" below).

## The convention (from `docs/dev/doc-citations.md`)

Cite code as `path#symbol` -- a `fn`/`struct`/`enum`/`impl`/`const`/method
(`Type::method`), the drift-proof greppable half. Never `path:line`. Observed house
idiom (verified against existing comments):

- **Same-file citation** -> bare backtick symbol, no path prefix:
  `` `cmd_ack_impl` `` (precedent: `cli/src/ack.rs#cmd_ack_impl`,
  `cli/src/discover.rs#PoolMembership::insert` usage).
- **Cross-file, Rust comment** -> backtick `` `path#symbol` `` with full path:
  `` `cli/src/membership.rs#PoolMembership::insert` `` (precedent:
  `cli/src/tui/app.rs` comments).
- **Cross-file, Python test comment** -> bare `path#symbol`, no backticks:
  `cli/src/main.rs#acquire_per_policy` (precedent already in repo:
  `tests/module/pool-lock-precedes-state-read.py` cites exactly this symbol).

Many of the target comments **already name the symbol** in their prose (e.g.
`check_raid1_relocation_space`, `plan_recover_refuses_replace_on_externally_mounted_pool`),
so the fix is usually deleting the `at <file>:NN` tail or swapping the parenthetical
-- not rewriting the comment.

## The 10 citations to convert

### `cli/src/recover.rs` -- 5 citations, 2 test preambles (same-file -> bare symbol)

Preamble of `wait_for_kernel_replace_no_ops_when_just_mounted_false`:
- `recover.rs:440` (the `if state.just_mounted` gate) -> the gate now lives at line
  ~480 in `RecoverWorkAction::execute`, `WaitForKernelReplace` arm. Replace with a
  reference to the `if state.just_mounted` gate in `` `RecoverWorkAction::execute` ``
  (the Intent line already names `WaitForKernelReplace.execute`).
- `recover.rs:1310` (planner already-mounted refusal) -> now at ~line 1340 inside
  `` `plan_recover` `` (`if open_plan.is_none() && is_replace_pool_mutation(...)`).
  The comment already names the pinning test
  `` `plan_recover_refuses_replace_on_externally_mounted_pool` `` -- keep that,
  drop the line number, anchor on `` `plan_recover` ``.
- `recover.rs:1381` (the `open_plan.is_some()` push gate) -> now at line 1406 in
  `` `plan_recover` ``. Replace with `` `plan_recover` ``'s `open_plan.is_some()`
  push gate.

Preamble of `remount_cycle_no_ops_when_just_mounted_false`:
- `recover.rs:451` (the `if state.just_mounted` gate) -> now at line ~491,
  `RecoverWorkAction::execute`, `RemountCycle` arm. Same treatment as `:440` above.
- `recover.rs:1406` (the `open_plan.is_some()` push gate) -> coincidentally still
  accurate today, but still a line number. Anchor on `` `plan_recover` ``'s push
  gate, same as `:1381`.

### `cli/tests/support/golden_common.rs` -- 2 citations (cross-file Rust -> backtick `path#symbol`)

- `preflight.rs:327` (the `for alloc_type in ["Data","Metadata","System"]` loop) ->
  `` `cli/src/preflight.rs#check_raid1_relocation_space` `` (fn at line 347; comment
  already names the function and quotes the loop).
- `preflight.rs:333-335` (the `bytes_on_target == 0` skip) -> same anchor
  `` `cli/src/preflight.rs#check_raid1_relocation_space` `` (the skip is at line 357,
  inside the same fn). Both citations collapse to one symbol.

### `cli/src/parse/btrfs_device_usage.rs` -- 1 citation (cross-file Rust -> backtick)

- `tests/progress-monitoring.py:164` -> line 164 sits inside the
  `` `device remove progress observed` `` subtest (defined at line 138) and is the
  line that captures the `btrfs-device-usage-removing.txt` fixture the comment
  already names. Replace with a reference to the `device remove progress observed`
  subtest in `` `tests/progress-monitoring.py` `` (the fixture name in the comment
  is the corroborating greppable token).

### `tests/cli/braid-unlock-key-file.py` -- 1 citation (Python -> bare `path#symbol`, no backticks)

- `storage.nix:265` -> the `--key-file`/`--allow-degraded` exit-2 contract is
  consumed at line ~318 in the `braid-auto-unlock` systemd service script. Replace
  with `modules/braid/storage.nix#braid-auto-unlock` (greppable token
  `braid-auto-unlock`; fuller anchor `systemd.services.braid-auto-unlock` if
  preferred).

### `tests/module/alert-state-lock.py` -- 1 citation (Python -> bare `path#symbol`, no backticks)

- `cli/src/main.rs:489` -> the pool lock is taken at dispatch via
  `acquire_per_policy(&pool_lock, lock_policy(&cli.command))` at line ~531 in `main`.
  Replace with `cli/src/main.rs#acquire_per_policy`. Keep the existing "per ADR 026"
  reference (`docs/design/decisions/026-pool-lock-rust-owned.md`). This exact symbol
  citation already has precedent in `tests/module/pool-lock-precedes-state-read.py`.

## Out of scope (deliberately)

These cite vendored/external code and are governed by a **different** rule
(`docs/dev/reference-source.md#citing-reference-code`: inline excerpt + `pkg
<version>` stamp, not `path#symbol`). Different fix shape -> separate follow-up, do
not touch in this pass:

- `cli/src/tui/probe.rs` (4 cites into `reference/hddfancontrol/...`)
- `modules/braid/fan-control.nix` (upstream hddfancontrol, already version-stamped
  "in 2.0.6")
- `tests/module/systemd-lifecycle.py` (nixpkgs `test_driver/machine/__init__.py`)

No enforcement script is added (user-confirmed). No code, behavior, tests, or
fixtures change.

## Verification

This is a comments-only change; correctness = "every new anchor resolves, no line
numbers remain, nothing else changed."

1. **No internal line-number citations remain.** Re-run the scan; expect zero hits
   in the 5 edited files (only the out-of-scope reference/external cites remain):
   ```
   git ls-files '*.rs' '*.nix' '*.py' | grep -vE '^(reference/|plans/)' \
     | xargs grep -nE "[a-z_]+\.(rs|nix|py):[0-9]+" \
     | grep -vE "line!|column!|file!|https?://|localhost:"
   ```
2. **Every new anchor is greppable.** For each symbol cited, confirm one match at
   the definition, e.g. `rg 'fn check_raid1_relocation_space'`,
   `rg 'acquire_per_policy' cli/src/main.rs`, `rg 'braid-auto-unlock' modules/braid/storage.nix`,
   `rg 'device remove progress observed' tests/progress-monitoring.py`,
   `rg 'fn plan_recover' cli/src/recover.rs`.
3. **No behavior touched.** `git diff` shows only comment lines changed (no `+`/`-`
   on a code line). As cheap insurance that no edit mangled a surrounding line, run
   `just test-rust` once: it covers compilation + unit + golden tests only
   (`cargo test --lib --bin braid --test golden_nixos_26_05 --test tty_guard --test
   confirm_yes`), which compiles the three edited Rust files' targets. It does **not**
   run doctests -- the `--lib --bin --test` selectors never pass `--doc` -- and none
   are involved anyway: every edit is a `//` or `#` line comment, not a `///` doc
   comment with a fenced example.
4. **ASCII-only over the *added* lines.** `just check-output-ascii` exempts comments
   by design ("Comments (`//`, `/* */`, plain `///`) are exempt"), so it won't
   validate these edits -- and a whole-file scan is the wrong tool too: these files
   already carry legal pre-existing Unicode in comments (em-dashes, arrows -- ~48 in
   `recover.rs` alone), which comments are exempt from and AGENTS.md permits
   rendering. Scope the check to lines this change *adds*; expect zero hits:
   ```
   git diff --unified=0 -- cli/src/recover.rs cli/tests/support/golden_common.rs \
     cli/src/parse/btrfs_device_usage.rs tests/cli/braid-unlock-key-file.py \
     tests/module/alert-state-lock.py \
     | grep -E '^\+' | grep -vE '^\+\+\+' | rg -n '[^[:ascii:]]'
   ```
   The final stage is `rg`, not `grep`: `[:ascii:]` is non-POSIX, and the macOS
   system grep (`/usr/bin/grep`, BSD) rejects it with `grep: invalid character
   class`; `rg` (already used in item 2) supports it. The two `grep` filter stages
   use only POSIX-safe anchors, so they're fine on BSD grep. All 8 edited lines are
   pure ASCII today and the citation replacements are ASCII, so the diff-scoped scan
   stays clean without tripping on the pre-existing comment Unicode.
   (`check-docs-see-paths` stays dropped: it scans only
   `docs/design/decisions/*.md` `## See` sections, and this change edits no docs.)

## Implementation notes

- `tests/cli/braid-unlock-key-file.py`: used the plan's primary greppable anchor
  `modules/braid/storage.nix#braid-auto-unlock` rather than the offered fuller form
  `#systemd.services.braid-auto-unlock` -- the bare service name is the token the
  comment's prose already names and matches the plan's verification grep.
- `cli/src/recover.rs`: applied the same-file backtick convention to the pinning
  test name (`` `plan_recover_refuses_replace_on_externally_mounted_pool` ``) and to
  `` `plan_recover` `` in both preambles, for consistency with the doc-citations
  house idiom rather than leaving them as the prior un-backticked prose.

## Follow Up

- Convert the out-of-scope reference/external line-number citations to the
  inline-excerpt + `pkg <version>` stamp form per
  `docs/dev/reference-source.md#citing-reference-code`: `cli/src/tui/probe.rs` (4
  cites into `reference/hddfancontrol/...`), `modules/braid/fan-control.nix`
  (`src/probe/mod.rs:84 in 2.0.6`), and `tests/module/systemd-lifecycle.py`
  (nixpkgs `test_driver/machine/__init__.py:858`). These were deliberately deferred
  from this pass (different rule, different fix shape).
