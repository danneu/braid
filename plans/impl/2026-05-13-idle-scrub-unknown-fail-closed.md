# Fix: `cmd_idle` silently treats `ScrubState::Unknown` as idle

## Context

`cli/src/idle.rs:84-95` guards busy on the scrub layer with
`if let ScrubState::Running { .. } = scrub.state` and falls through to
`IdleResult::Idle` for every other variant -- including
`ScrubState::Unknown`. The parser produces `ScrubState::Unknown`
whenever `btrfs scrub status --raw` returns empty stdout or a
`Status:` line with a word it does not classify
(`cli/src/parse/btrfs_scrub_status.rs:269,275`).

The autosuspend gate is supposed to be fail-closed:
`docs/decisions/016-auto-suspend.md:49` -- *"Don't know" never becomes
"allow suspend"*. The same parser layer is already handled
fail-closed for the sysfs-exclop side (line 59 of the same doc) and
for the resume-needs-scrub consumer
(`cli/src/scrub_needs_resume.rs:32-40` -- `ScrubState::Unknown` returns
`Err(StatusUnknown)`). `cmd_idle` is the only mission-critical
consumer that gets it wrong. Status (`cli/src/status.rs:641`) and TUI
(`cli/src/tui/probe.rs:106`) consumers correctly stay informational.

Effect of the bug: a future kernel/btrfs-progs upgrade that reshapes
`Status:` output, or an empty-stdout edge case, makes
`parse_btrfs_scrub_status` return `Ok(state: Unknown)`. `cmd_idle`
returns `Idle` -> exit 0 -> autosuspend allows suspend mid-scrub.

No existing test seeds an `Ok(Unknown)` scrub body. The current
`busy_unknown_on_scrub_parse_failure` test exercises the parser-`Err`
path (command exits non-zero), not the parser-`Ok(Unknown)` path.

## Files to modify

- `cli/src/idle.rs` -- replace the `if let` with an exhaustive `match`
  on `ScrubState`, and add a unit test for the `Ok(Unknown)` path.
- `docs/decisions/016-auto-suspend.md` -- extend the fail-closed branch
  list (line ~59) to cover the scrub-state Unknown case alongside the
  sysfs-side unrecognized-parser-value clause.

## Code change

Replace the `if let ... = scrub.state { ... } IdleResult::Idle` block
at `cli/src/idle.rs:84-97` with a fully enumerated match that mirrors
the pattern in `cli/src/scrub_needs_resume.rs:32-40`:

```rust
match scrub.state {
    ScrubState::Running {
        bytes_scrubbed,
        total_bytes,
        ..
    } => {
        let pct = match (bytes_scrubbed, total_bytes) {
            (Some(scrubbed), Some(total)) => pct_from_bytes(scrubbed, total),
            _ => None,
        };
        IdleResult::Busy(BusyReason::ScrubRunning { pct })
    }
    ScrubState::Never
    | ScrubState::Finished { .. }
    | ScrubState::Aborted { .. }
    | ScrubState::Interrupted { .. } => IdleResult::Idle,
    ScrubState::Unknown => busy_unknown("scrub", "unrecognized scrub state"),
}
```

Rationale for the exhaustive match over the finding's literal "extra
arm before fall-through" prescription:

- Mirrors `scrub_needs_resume.rs:32-40`. Two of three policy consumers
  of `ScrubState` now share the same structural shape; only the
  per-variant verdict differs.
- No wildcard. A future `ScrubState` variant forces a maintainer
  decision; it cannot silently inherit the "Idle" verdict.
- Diff size is the same.

Reuse the existing `busy_unknown` helper at `cli/src/idle.rs:100-102`
-- do not introduce a new error constructor.

## New test

Add to the `tests` module in `cli/src/idle.rs`, alongside the existing
`busy_unknown_on_scrub_probe_failure` (line 355) and
`busy_unknown_on_scrub_parse_failure` (line 377) tests:

```rust
// Intent: a parser result of `Ok(state: ScrubState::Unknown)` after a
//   clean (zero-exit) scrub-status invocation must surface as
//   Busy::Unknown, not Idle.
// Why it exists: closes the last fail-open seam in the autosuspend
//   gate. The parser-Err path is covered by
//   busy_unknown_on_scrub_parse_failure; this test pins the
//   parser-Ok-but-Unknown path that the previous
//   `if let ScrubState::Running` shape silently treated as idle. Same
//   fail-closed contract the sysfs branch and scrub_needs_resume.rs
//   already obey.
// Scenario: btrfs-progs upgrade reshapes the `Status:` line (or
//   stdout is empty); parse_btrfs_scrub_status returns
//   Ok(BtrfsScrubStatusOutput { state: ScrubState::Unknown }).
#[test]
fn busy_unknown_on_scrub_state_unknown() {
    let (scrub_req, scrub_out) = crate::test_fixtures::scrub_status_unknown();
    let runner = MockRunner::default().with_output(scrub_req.clone(), scrub_out);
    let fs = IdleMockFs::with_exclop("none");

    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_idle_busy_unknown_prefix(result, "scrub:");
    assert_eq!(runner.requests(), vec![scrub_req]);
}
```

Plumbing:

- The fixture `scrub_status_unknown()` already exists at
  `cli/src/test_fixtures/scrub.rs:122-124` (it returns
  `RawCommandOutput { exit_status: 0, stdout: "" }`, which the parser
  converts to `Ok(state: Unknown)`). Re-exported via
  `cli/src/test_fixtures.rs:202`.
- Add `scrub_status_unknown` to the `use crate::test_fixtures::{...}`
  import list at `cli/src/idle.rs:108-111`. Importing through the
  module path (as in the test body above) also works -- pick whichever
  matches the surrounding tests; the existing tests import everything
  by name, so add to that list.

## Documentation update

`docs/decisions/016-auto-suspend.md` line 59 currently enumerates
fail-closed branches for the *sysfs* probe layer only:

> Fail-closed branches: `list_dir("/sys/fs/btrfs")` IO errors, any
> read error on a non-allowlisted entry's `exclusive_operation`
> (including `NotFound`), unrecognized parser values, and an empty
> `/sys/fs/btrfs/` after the mount check passed all surface as
> `Busy(BusyReason::Unknown)` and exit 1.

Add a complementary sentence for the scrub probe (so the doc fully
covers what `cmd_idle` now does):

> The scrub probe is held to the same contract: a
> `parse_btrfs_scrub_status` result of `ScrubState::Unknown` (empty
> stdout or an unrecognized `Status:` word) surfaces as
> `Busy(BusyReason::Unknown)` and exits 1. Parser drift must not
> silently allow suspend.

## Verification

Targeted runs use `cargo test --lib <filter>` directly; the `just
test-rust` recipe (`Justfile:104`) takes no args and runs the full
unit-test set unconditionally.

1. `cargo test --lib --manifest-path cli/Cargo.toml idle::tests::busy_unknown_on_scrub_state_unknown`
   -- new test passes (red before the code edit, green after).
2. `cargo test --lib --manifest-path cli/Cargo.toml idle::tests` -- no
   regressions in the existing `cmd_idle` suite (Running,
   probe-failure, parse-failure, mountinfo, sysfs branches all still
   pass).
3. `just test-rust` -- full unit-test suite; in particular,
   `scrub_needs_resume::tests::unknown_is_hard_error` still passes
   (sibling consumer of the same enum unchanged).
4. `cargo build -p braid-cli` -- exhaustive `match` compiles with no
   wildcard warning.
5. Sanity grep: `grep -n 'if let ScrubState' cli/src/idle.rs` -- zero
   hits after the edit.
6. Sanity grep: `grep -rn 'ScrubState::Unknown' cli/src/` -- confirm
   `idle.rs` now appears in the list alongside `scrub_needs_resume.rs`,
   `status.rs`, and the TUI consumers.

## Out of scope

- `status.rs:641` and `tui/probe.rs:106` -- informational paths that
  correctly route `ScrubState::Unknown` to a non-blocking
  `ScrubReport::Unknown`. No change.
- Renaming/restructuring `BusyReason::Unknown` or the `busy_unknown`
  helper. The existing shape works.
- Adding a shared "scrub-state classifier" helper across
  `cmd_idle` / `cmd_scrub_needs_resume` / `status`. The three
  consumers have genuinely different per-variant verdicts; collapsing
  them into one would obscure intent.
