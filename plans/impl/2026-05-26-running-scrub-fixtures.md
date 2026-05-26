# Fix fabricated / btrfs-impossible running-scrub fixtures

## Context

A `/ultrareview` finding flagged `idle_scrub_running` and `scrub_status_running`
as near-duplicate running-scrub fixtures and proposed consolidating them into one
cross-module fixture. Verification rejected that headline (see "Why not
consolidate" below) but surfaced real fidelity defects:

1. `scrub_status_running()` emits a `10.00% done` line that **is not real btrfs
   output in any mode**. The authoritative running `--raw` frame
   (`cli/tests/fixtures/nixos-25.11/btrfs-scrub-running.txt`,
   `reference/btrfs-progs/cmds/scrub.c:201`) emits `Bytes scrubbed:   N  (P%)`
   while a scrub runs; the only `done` token is `scrub done for <fsid>` on
   completion (`scrub.c:1681`). The fabricated line is also never parsed --
   `parse_btrfs_scrub_status` has no rule for it (only `parse_bytes_scrubbed`
   at `cli/src/parse/btrfs_scrub_status.rs:88`).

2. **Both** running fixtures emit a `Bytes scrubbed:` line with **no `Time left:`
   / `ETA:` lines** -- a frame btrfs never produces. `print_fs_stat`
   (`scrub.c:387-398`) prints `Status:` via `_print_scrub_ss` and the progress
   block via `print_scrub_summary` against the **same** `fs_stat->s` (braid uses
   `--raw` = `UNITS_RAW`, not `-R`/`print_raw`, so it takes the `print_scrub_summary`
   branch). One `in_progress` flag therefore gates both `Status: running`
   (`scrub.c:340-341`) and the entire `Time left:` / `ETA:` / `Total to scrub:` /
   `Bytes scrubbed:` block (`scrub.c:186-203`, no sub-conditions). So `Status: running` with a
   `Bytes scrubbed:` line *always* co-occurs with `Time left:` and `ETA:`; a frame
   with one but not the others is structurally impossible. The golden fixture
   confirms this empirically (it carries all of `Time left:`, `ETA:`,
   `Bytes scrubbed:`). Applies to `scrub_status_running` (the cited fixture) **and**
   `idle_scrub_running` (the identical sibling).

3. btrfs **derives** `Rate:`, `Time left:`, and `ETA:` from `bytes_scrubbed` and
   `duration` (`scrub.c:162-203`): `rate = bytes_scrubbed / duration`,
   `sec_left = (total - bytes_scrubbed) / rate`, `eta = now + sec_left`. They are
   not independent fields, so a faithful running frame's `Rate`/`Time left`/`ETA`
   must be mutually consistent with its `Bytes scrubbed:`/`Duration:`. To make
   `idle_scrub_running` faithful without a per-`pct` footgun, it is fixed at a
   single concrete 45% frame (below) rather than parameterized.

Every terminal-state sibling in the scrub module (`scrub_status_finished`,
`scrub_status_aborted`, `scrub_status_interrupted`) is correctly faithful: btrfs's
`in_progress=false` branch (`scrub.c:204-207`) emits only `Total to scrub:`, so
those fixtures rightly omit `Time left:` / `ETA:` / `Bytes scrubbed:`.

**Outcome:** make both running fixtures faithful, structurally-valid running
`--raw` frames, and bring `scrub_status_running` into line with its sibling family
-- dissolving the duplication at its root without cross-module coupling.

## The fix

Two files. `scrub_status_running` is a fixture-string edit. `idle_scrub_running`
also drops its `pct` parameter (its one call site is updated to a no-arg call). No
new helpers.

### 1. `cli/src/test_fixtures/scrub.rs` -- `scrub_status_running()` (lines 57-69)

Rewrite the raw-stdout string passed to the existing `scrub_status_output(...)`
helper (`scrub.rs:164`) to a faithful, sibling-aligned, self-consistent running
frame:

```
UUID:             12345678-1234-1234-1234-123456789abc
Scrub started:    Mon Jan  1 00:00:00 2024
Status:           running
Duration:         0:00:01
Time left:        0:00:01
ETA:              Mon Jan  1 00:00:02 2024
Total to scrub:   1073741824
Bytes scrubbed:   536870912  (50.00%)
Rate:             536870912/s
Error summary:    no errors found
```

- **Remove `10.00% done`** -- fabricated, never parsed.
- **Add `Bytes scrubbed:`, `Time left:`, `ETA:`** in btrfs field order
  (`Duration:` -> `Time left:` -> `ETA:` -> `Total to scrub:` -> `Bytes scrubbed:`
  -> `Rate:`), matching the golden fixture and `scrub.c:186-203`. Spacing mirrors
  the golden fixture (three spaces after `Bytes scrubbed:`, two before the paren).
- **Align `Total to scrub:` to 1073741824 (1 GiB) and `Duration:` to 0:00:01** --
  matches the `scrub_status_finished/_aborted/_interrupted` family, so the Running
  fixture belongs to its module rather than cloning the idle-scope fixture.
- **Internally self-consistent (matches btrfs's `scrub.c:162-203` derivation):**
  0.5 GiB of 1 GiB (50%) scrubbed in 1s at 0.5 GiB/s (`Rate: 536870912/s`) leaves
  0.5 GiB = 1s remaining (`Time left: 0:00:01`); `ETA` = start(00:00:00) +
  duration(1s) + time-left(1s) = 00:00:02. Jan 1 2024 is a Monday, so the ctime
  weekday is correct.

### 2. `cli/src/test_fixtures/idle.rs` -- `idle_scrub_running` (lines 181-203) + call site `idle.rs:213`

Drop the `pct` parameter and make this a single concrete 45%-complete running
frame. Update the sole call site `idle.rs:213` from `idle_scrub_running(45)` to
`idle_scrub_running()`, and rewrite the doc comment to describe a concrete,
derivation-consistent 45% running frame (it is no longer "percentage-driven").
With the parameter gone the body is a static string -- no `total`/`scrubbed`
arithmetic or `format!`.

```
UUID:             12345678-1234-1234-1234-123456789abc
Scrub started:    Mon Jan  1 00:00:00 2024
Status:           running
Duration:         0:00:05
Time left:        0:00:06
ETA:              Mon Jan  1 00:00:11 2024
Total to scrub:   30408704000
Bytes scrubbed:   13683916800  (45.00%)
Rate:             2736783360/s
Error summary:    no errors found
```

**Fully derivation-consistent, and no longer a footgun.** Per `scrub.c:162-203`
btrfs computes `Rate`/`Time left`/`ETA` from `bytes_scrubbed`/`duration`; it does
not emit them independently. At 45% of 30408704000,
`bytes_scrubbed = 13683916800` and `duration = 5s`, so the frame reads
`Rate: 2736783360/s` (13683916800 / 5), `Time left: 0:00:06`
(16724787200 / 2736783360 = 6), and `ETA: Mon Jan  1 00:00:11 2024`
(start 00:00:00 + 5s duration + 6s left). Fixing the whole frame at one concrete
percentage removes the earlier hazard where a `pct`-parameterized `Bytes scrubbed:`
paired with hardcoded `Rate`/`Time left`/`ETA` was btrfs-impossible for any
`pct != 45`.

The consumer `busy_when_scrub_running` asserts
`BusyReason::ScrubRunning { pct: Some(45) }`; idle derives that `45` from
`Bytes scrubbed:`/`Total to scrub:` via `pct_from_bytes` (`idle.rs:94`), and the
concrete frame feeds the identical `13683916800 / 30408704000 = 45%`, so the
assertion stays green. The percentage path itself is covered directly by the
`pct_from_bytes` unit tests in `progress.rs`, so dropping the parameter loses no
coverage. The parser also populates `time_left_secs`/`eta`/`rate_bytes_per_sec`,
which the idle path ignores -- zero behavioral risk.

## Why not consolidate (the finding's original proposal)

Reaffirming the verification conclusion -- do **not** merge the two fixtures:

- They live in deliberately scope-local modules (`test_fixtures/idle.rs`,
  `test_fixtures/scrub.rs`) whose docs favor flat, narrow, per-scope fixtures over
  shared builders. Merging couples the two scopes.
- It would break the uniform `scrub_status_*` family that `scrub_needs_resume.rs`
  consumes as a set.
- The consumer of `scrub_status_running` (`running_does_not_need_resume`,
  `cli/src/scrub_needs_resume.rs:104`) only needs `Status: running` to classify the
  state; feeding it the idle 45%-progress frame would add irrelevant detail and
  obscure intent.
- Parser/format drift is caught by the golden fixture
  (`cli/tests/fixtures/nixos-25.11/btrfs-scrub-running.txt`) plus the VM parser
  canary, not by these synthetic command-level mocks.

Aligning the scrub fixture's constants to its sibling family (above) removes the
residual duplication by differentiation, which is the correct resolution given
these constraints.

## Out of scope

- **Parser inline tests** (`cli/src/parse/btrfs_scrub_status.rs`) are not touched.
  `scrub_running_inline` carries `Time left:` + `ETA:` + `Bytes scrubbed:` and is
  the faithful real-`--raw`-shape counterpart. `scrub_running_minimal` is a
  deliberately synthetic parser-robustness test for incomplete/sparse input -- it
  asserts the parser tolerates missing optional fields without failing. It is
  **not** a model of real btrfs output: the same `scrub.c` proof used above shows
  `Status: running` always co-occurs with `Time left:`/`ETA:`/`Bytes scrubbed:`, so
  a frame carrying only `Status: running` is not something btrfs emits. Its
  robustness intent stands regardless, so it stays out of scope. (The test's own
  `Scenario:` comment -- "btrfs hasn't computed estimates yet" -- overstates real
  btrfs behavior the same way; tightening it is an optional adjacent cleanup, out
  of this fixture-focused plan.)
- No cross-module fixture consolidation or new shared builder.
- No TUI snapshot churn: `snapshot_scrub_tab_running.snap` ("Time left 34m 24s")
  is driven by the golden fixture's `0:34:24`, not by these synthetic fixtures.

## Verification

This is a pure synthetic unit-test fixture change. It does not touch parser logic,
the committed golden fixtures, or any parser-critical tool version, so **no VM
tests, parser canary, or fixture refresh are required** (per AGENTS.md "Parser
Compatibility").

1. `just test-rust` -- runs `cargo test` for `braid-cli`. Confirm the full suite
   passes, in particular:
   - `cli/src/scrub_needs_resume.rs::running_does_not_need_resume` still yields
     `ScrubNeedsResumeResult::No` (sole consumer of `scrub_status_running`).
   - `cli/src/idle.rs::busy_when_scrub_running` (call site updated to
     `idle_scrub_running()`) still yields
     `IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(45) })`.
   - `cli/src/parse/btrfs_scrub_status.rs` parser tests are unaffected (they use
     the golden fixture / inline strings, not these fixtures).
2. Sanity-grep `cli/src/test_fixtures/`:
   - `rg "% done"` returns nothing (fabricated line gone).
   - both running fixtures now contain `Time left:` and `ETA:` lines.
   - `rg "idle_scrub_running\("` shows the definition takes no args and the one
     call site passes none.

## Review findings folded in

- Plan-review (Low): "Proposed running scrub frame still omits real running-only
  fields (`Time left:` / `ETA:`)." Verified against `scrub.c:186` + `:340-341`
  (shared `in_progress` flag) and extended to the identical-defect sibling
  `idle_scrub_running` (user-confirmed scope). The first draft's claim that
  `idle_scrub_running` was "already faithful" was incorrect and is removed.
- Plan-review (Low): "`idle_scrub_running` still keeps a btrfs-impossible derived
  `Rate`." Verified against `scrub.c:162-203` (`Rate`/`Time left`/`ETA` are derived
  from `bytes_scrubbed`/`duration`, not independent). At 45% the faithful values
  are `Rate: 2736783360/s`, `Time left: 0:00:06`, `ETA: Mon Jan  1 00:00:11 2024`.
  The `scrub_status_running` frame was already derivation-consistent.
- Plan-review (Low): "`idle_scrub_running(pct)` stays parameterized while its
  derived fields are hardcoded for `pct=45`." Confirmed exactly one caller
  (`idle.rs:213`, `pct=45`) and that `pct_from_bytes` is unit-tested directly in
  `progress.rs`, so the parameter is unused flexibility that re-introduces the same
  fixture drift for any other `pct`. Resolution: drop the parameter, fix the frame
  at a concrete 45%, update the call site and doc comment.
- Plan-review (Low): "Plan repeats a false btrfs behavior claim for
  `scrub_running_minimal`." Confirmed via `print_fs_stat` (`scrub.c:387-398`): for
  braid's `--raw` (units, not `-R`), `_print_scrub_ss` and `print_scrub_summary`
  run against the same `fs_stat->s`, so `Status: running` always co-occurs with
  `Time left:`/`ETA:`/`Bytes scrubbed:`. The out-of-scope bullet no longer cites
  "btrfs hasn't computed estimates yet" as real output; `scrub_running_minimal` is
  reframed as a synthetic sparse-input robustness test, consistent with the same
  source proof used for the fixture fix.
