# Plan: make `lock` execute output capturable and pin the "pool already locked" terminal line

## Context

A `/verify-issue` salvage on a low-severity testing finding surfaced a real,
in-scope architectural issue in `cli/src/lock.rs`. Three threads converge:

- **Progress rows bypass the capture sink.** `LockPlan::execute` and its helper
  `CloseMapperCtx::close_one` emit per-disk status rows through **bare `eprint!`**
  (16 call sites in `lock.rs:505-818`), plus a bare `eprintln!` for the terminal
  line (`lock.rs:846`). These write straight to the process stderr fd and bypass the
  test-capture sink `status_tag::emit_status` -> `testing::capture_line`, so almost
  none of `lock`'s real-run output can be asserted in fast Rust unit tests. The lone
  exception is the umount-retry warn (`lock.rs:468`), which already uses `emit_status`
  -- which is exactly why it is the only line a unit test currently pins (`lock.rs:2352`).
- **Note rendering is hand-rolled and lossy.** Every other mutating command renders
  real-run plan notes through the shared helper `cli/src/preview.rs#emit_notes_to_stderr`
  (`add`, `remove`, `unlock`, `recover`, `enroll_key_file`, `remove_missing`, `main.rs`).
  `lock`'s `execute` is the lone outlier: it hand-rolls a loop (`lock.rs:677-681`) that
  matches **only** `PreviewNote::Warn` and bypasses the shared renderer, so a future
  `Info`/`Skip`/`PerDisk` lock note would silently not render on a real run. This
  violates the ADR-022 output contract
  (`docs/design/decisions/022-dry-run-preview-model.md#output-contract`: real-run notes
  must "render to stderr through the shared preview renderers").
- **The shared renderer is itself not capture-aware.** `emit_notes_to_stderr`
  (`preview.rs:230-236`) writes via bare `eprint!`, which is why `replace` carries its
  own capture wrapper. Routing it through `emit_status` makes every command's notes
  unit-testable at once.
- Consequently the documented terminal line `pool already locked` (`docs/commands/lock.md`;
  ADR-024 requires it be suppressed when cleanup is uncertain) has **no fast unit
  test** for its emission or suppression. The behavior *is* covered by VM tests
  (`tests/cli/braid-recover.py`, `braid-lock.py`, `luks-lock-skipped-no-false-closed.py`),
  but those need a full NixOS VM build; a millisecond-level regression signal is missing.

The original finding's headline ("untested, fail-open") was wrong -- VM tests guard
the contract. The salvageable kernel: route lock's notes through the shared (now
capture-aware) renderer and its progress rows through `emit_status`, fixing the
ADR-022 violation *and* making the terminal line -- plus the rest of execute's output
-- unit-testable. Then add the fast unit tests the finding wanted.

**Production output is byte-identical before/after.** `emit_status` and the converted
`emit_notes_to_stderr` both write via `eprint!("{line}")` in non-test builds, and
`render_notes_for_stderr_with` renders today's `Warn`-only lock notes identically to
the hand-rolled loop; only test-observability changes.

## Approach

### 1a. Make the shared note renderer capture-aware (`cli/src/preview.rs`)

`emit_notes_to_stderr` (`preview.rs:230-236`) currently does
`eprint!("{}", render_notes_for_stderr_with(notes, style, color_enabled))`. Change
the write to `crate::status_tag::emit_status(&render_notes_for_stderr_with(...))`.
Update the doc comment (`preview.rs:225-229`): the reason `replace` keeps its own
wrapper is no longer "this helper isn't capture-aware" but that `replace` routes
through its own `replace_stderr_capture` sink -- state that accurately.

This is zero production-output change (`emit_status` -> `eprint!` in non-test builds)
and benefits all 7 callers (`add`, `remove`, `unlock`, `recover`, `enroll_key_file`,
`remove_missing`, `main.rs`) by making their real-run notes capturable. (`main.rs`
calls it twice, `main.rs:1309`/`:1379`; the other six call it from a plan `execute`
and a separate non-plan path.)

### 1b. Replace lock's hand-rolled note loop with the shared renderer (`cli/src/lock.rs`)

Replace the `Warn`-only loop at `lock.rs:677-681` with
`preview::emit_notes_to_stderr(&self.notes, PerDiskStyle::Bracketed);` (the
`Bracketed` style every other command uses). Also rewrite the explanatory comment
directly above it (`lock.rs:673-676`), which currently reads "this loop is the single
emit point for all of them ... as `PreviewNote::Warn`" -- both claims go false once the
loop is gone and the shared renderer handles every note kind. Replace it with something
like: "Render accumulated plan notes to stderr through the shared preview renderer
before any mutation -- the same path every other command uses, so dry-run and real-run
share one renderer and any future Info/Skip/PerDisk note also surfaces." (Mirror the
care taken with the parallel doc comment updated in 1a.) Widen lock's import
(`lock.rs:10`) from
`use crate::preview::{Preview, PreviewCompleteness, PreviewNote};` to
`use crate::preview::{self, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};`
-- it needs both `self` (to qualify the `preview::` path) and `PerDiskStyle`, matching
the sibling modules (`unlock`/`remove`/`replace`). This fixes the ADR-022 violation and
the silent `Info`/`Skip`/`PerDisk` drop;
output for today's `Warn`-only notes is byte-identical
(`render_notes_for_stderr_with` renders `Warn` via the same `status_line(Warn, ...)`).

### 1c. Route the remaining execute / close_one progress rows through `emit_status` (`cli/src/lock.rs`)

Both functions already define `let line = |t, body: &str| status_line(t, color_enabled, body);`
(`close_one` at line 502, `execute` at line 671). Convert each remaining bare
progress-row emit with the mechanical pattern
`eprint!("{}", line(StatusTag::X, body))` -> `emit_status(&line(StatusTag::X, body))`:

- `CloseMapperCtx::close_one`: the 4 sites at `lock.rs:505, 514, 525, 536`.
- `LockPlan::execute` (non-note rows): the 11 sites at
  `lock.rs:690, 698, 702, 718, 733, 752, 765, 780, 781, 806, 818`.

Then the **terminal line** at `lock.rs:846`:
`eprintln!("pool already locked")` -> `emit_status("pool already locked\n")`.
Keep it **untagged** (no `[ok]`) to preserve the exact documented output; the dry-run
sibling `nothing to do.` is likewise plain.

Notes:
- No change to `cli/src/status_tag.rs`. Color is already test-aware via
  `color_enabled_for_stderr()` -> `testing::color_override()`, which
  `capture_with_color` drives. `emit_status` is already imported in `lock.rs`.
- Leave `lock.rs:468` (retry warn) as-is -- already correct.
- Keep the `line` closure as a pure formatter (mirrors `mapper_close.rs`'s explicit
  `emit_status(&status_line(...))`). Do **not** introduce a side-effecting `emit`
  closure -- keeping the diff a pure `eprint!("{}", X)` -> `emit_status(&X)` transform
  keeps it trivially reviewable.

### 2. Add four fast unit assertions (capture-based, `cli/src/lock.rs` test module)

Wrap `cmd_lock_impl` in `crate::status_tag::testing::capture_with_color(false, || { ... })`
and assert on the captured string (same shape as the existing capture tests at
`lock.rs:2328` / `:2398`). With steps 1a-1c done, the terminal line is now in the buffer.

- **(A) Emission -- extend `lock_already_locked` (`lock.rs:2075`).** It already builds
  the canonical case: unmounted (`MountpointCheck` exit 1), empty `lock_fs(&[])`,
  `lock_test_membership`. Wrap its call in `capture_with_color`, keep the `Ok`
  expectation, and assert the line appears as its own **final, untagged** line:
  `assert_eq!(captured.lines().last(), Some("pool already locked"));`. This pins
  emission *and* the untagged form -- a plain `contains()` would still pass if the line
  regressed to a tagged `[ok]   pool already locked`, violating the untagged-output
  goal and `docs/commands/lock.md#braid-lock`. (Scenario (A) emits the two
  `[ok]   disk <name>: already closed` rows then the terminal line, so `last()` is the
  terminal line.) Add the standard Intent/Why/Scenario preamble. Converts the test
  from "returns Ok" to "actually emits the documented line."
- **(B) Suppression under `cleanup_uncertain` -- new test.** Model the mocks on
  `unverified_fallback_candidate_is_warned_and_skipped` (`lock.rs:3233`): unmounted,
  `lock_fs(&["/dev/mapper/braid-aaa"])`, and **omit** the
  `CryptsetupStatus { mapper: braid-aaa }` mock so UUID verification fails and
  `cleanup_uncertain` becomes true. Run through `cmd_lock_impl` (not just `plan_lock`)
  inside capture; assert the output **contains**
  `[warn] skipping mapper braid-aaa: cannot verify backing LUKS UUID` and
  **does not contain** `pool already locked`. (Bonus: this is the first execute-path
  unit coverage of the uncertain case -- today only `plan_lock` is unit-tested for it.)
- **(C) Suppression when a real close happened (`all_already_closed == false`) --
  extend `lock_partial_state` (`lock.rs:2091`).** It already opens `braid-aaa` and
  closes it on an unmounted pool. Wrap in capture, keep the `Ok` expectation, and assert:
  - `assert!(!captured.contains("pool already locked"))` -- the suppression.
  - `assert!(captured.contains("[wait] disk aaa: locking..."))` **and**
    `assert!(captured.contains("[ok]   disk aaa: locked"))` -- the close progress rows
    (exact strings from `close_one`, `lock.rs:505`/`516`; `disk_label` is `aaa`,
    non-orphan so no `(orphan)` suffix). Without these, reverting the `close_one`
    conversion (step 1c) to bare `eprint!` would leave the test green.

  Add the Intent/Why/Scenario preamble (it currently has only a one-line comment).
  Pins both the `all_already_closed` half of the step-5 guard *and* the `close_one`
  capture conversion.
- **(D) Mounted-path row conversion -- extend the umount-retry capture test
  (`lock.rs:2331`).** Tests (A)/(B)/(C) are all *unmounted* scenarios, so none of the
  nine mounted-path row conversions (pause / unmount / `--forget`) gets a fast
  assertion. The existing umount-retry-succeeds test already runs `cmd_lock_impl` over
  the **mounted** path inside `capture_with_color` and asserts the retry warn; add one
  line -- `assert!(captured.contains("[ok]   pool: unmounted /mnt/storage"))` (exact
  string from `lock.rs:733`, `[ok]` padded to three trailing spaces like (C)) -- to pin
  the unmount-success row conversion (`lock.rs:733`, in 1c's list). Like (A)/(C) this
  fails before 1c converts row 733 and passes after, extending the fast signal to
  the most intricate branch of `execute` at ~zero cost. No preamble change needed (the
  test already has one); this is a single added assertion.

Preamble house style: three `//` sub-headings (Intent / Why it exists / Scenario),
per existing lock tests and `docs/dev/testing.md`.

### Out of scope (considered, not done)
`replace` keeps its own note path (`emit_replace_notes_to_stderr` ->
`replace_stderr_capture`, `replace.rs:1145-1159`) -- a *separate* capture sink from
`status_tag`'s. Making `emit_notes_to_stderr` capture-aware (1a) does not require
touching it, and collapsing `replace` onto the shared sink is a distinct refactor not
attempted here. (1a corrects the now-stale doc-comment rationale on
`emit_notes_to_stderr` to reflect this.)

## Critical files
- `cli/src/preview.rs` -- make `emit_notes_to_stderr` capture-aware and fix its doc
  comment (1a).
- `cli/src/lock.rs` -- swap the note loop for the shared renderer (1b), convert the 15
  progress rows and the terminal line (1c), and add/extend the four unit assertions
  (tests (A)-(D)).
- Read-only reference patterns: `cli/src/mapper_close.rs` (`emit_status(&status_line(...))`),
  `cli/src/status_tag.rs` (`emit_status`, `testing::capture_with_color`,
  `color_enabled_for_stderr`), `cli/src/replace.rs` (its `render_notes_for_stderr_with`
  wrapper).

## Verification
1. `just test-rust` (or `cargo test -p braid-cli lock`) -- the four new/extended
   assertions pass for the right reason (write them first, watch (A)/(C)/(D) fail
   before steps 1b/1c, then implement).
2. **Critical regression check (wider than lock):** step 1a routes *every* command's
   notes through the capture buffer, and 1b/1c route ~16 more lock lines there. Run the
   *full* Rust suite and confirm no existing `capture_with_color` assertion across
   `add`/`remove`/`unlock`/`recover`/`enroll_key_file`/`remove_missing`/`main.rs`/`lock`
   breaks on newly-captured note/row lines. (`main.rs` carries no capture-based unit
   tests, so the audit there reduces to "production output byte-identical" -- nothing
   to re-assert, but it is in the 1a blast radius and the list must be complete.) The
   known lock capture tests (`lock.rs:2328`, `:2398`)
   use `.contains()` / `!.contains()` and should be robust; breakage elsewhere means a
   test was silently not seeing notes before, and its assertion should be updated to
   account for them.
3. Confirm production output is unchanged: both `emit_status` and the converted
   `emit_notes_to_stderr` resolve to `eprint!("{line}")` in non-test builds. The VM
   tests `braid-lock.py` / `braid-recover.py` / `luks-lock-skipped-no-false-closed.py`
   (substring matches) continue to hold.
4. `scripts/docs/check-output-ascii.py` -- the new plain line and assertions are ASCII.
