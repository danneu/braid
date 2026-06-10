# Plan: pin the byte-less running-scrub wiring in `cmd_idle`

## Context

`braid idle` is the autosuspend gate: `Busy` blocks suspend (exit 1), `Idle`
allows it (exit 0). The gate is deliberately fail-closed -- every unknowable
probe maps to `Busy`, never to idle.

When a scrub is running, `cmd_idle` derives a percentage from the byte counters
and returns busy regardless of whether that percentage is computable
(`cli/src/idle.rs:94-105`):

```rust
ScrubState::Running { bytes_scrubbed, total_bytes, .. } => {
    let pct = match (bytes_scrubbed, total_bytes) {
        (Some(scrubbed), Some(total)) => pct_from_bytes(scrubbed, total),
        _ => None,
    };
    IdleResult::Busy(BusyReason::ScrubRunning { pct })   // Busy is unconditional
}
```

The `Busy` decision sits *outside* the `(bytes_scrubbed, total_bytes)` match --
correct -- but **no `cmd_idle` test pins that invariant for the `pct: None`
case**. The existing wiring test `busy_when_scrub_running` (`idle.rs:242-252`)
only feeds the both-counters-present fixture (`pct: Some(45)`).
`busy_reason_display_pins_cli_strings` (`idle.rs:260-301`) exercises the
`pct: None` *Display*, not the `cmd_idle` wiring.

`Running { pct: None }` is a parser-contract / format-drift boundary, **not** a
transcript of live early-scrub output. On the target btrfs a running scrub
always carries byte counters: `_print_scrub_ss`
(`reference/btrfs-progs/cmds/scrub.c`) emits `Scrub started:` / `Status:` /
`Duration:` as one fixed block, and `print_scrub_summary`'s in-progress branch
always prints `Total to scrub:` + `Bytes scrubbed:` -- so live running output is
`pct: Some` (captured in `cli/tests/fixtures/nixos-26.05/btrfs-scrub-running.txt`).
The genuine pre-progress window is `no stats available` -> `ScrubState::Never`,
a different cell. The byte-less *running* case arises instead from the parser's
deliberate tolerance: it makes every Running field except `error_count` an
`Option` and accepts a sparse `Status: running` record
(`scrub_running_minimal`, `cli/src/parse/btrfs_scrub_status.rs:437-472`). Because
braid parses only the human-summary `Total to scrub` / `Bytes scrubbed` lines,
any btrfs-progs output drift (a live risk -- see AGENTS.md parser-compatibility)
that keeps `Status: running` but reshapes or omits those lines yields
`Running { bytes: None }`. The fail-closed contract is that a running scrub
blocks suspend whether or not progress is quantifiable. A refactor that folded
the Busy/Idle choice into the pct match -- returning `Idle` when the percentage
can't be computed -- would compile, keep the parser tests green (they classify
`ScrubState`, not `IdleResult`), keep `busy_when_scrub_running` green, and
**silently allow suspend whenever the pct is unknowable**.

The codebase already treats each `cmd_idle` arm as a wiring invariant worth its
own test: `idle_when_scrub_never` / `_aborted` / `_interrupted`
(`idle.rs:185-232`, commit `439a663b`) and the most recent addition
`pool_offline_when_non_btrfs_at_mount_point` (`idle.rs:156`, commit `00deb4c4`)
carry exactly this "compiles clean, parser-green, silently flips the gate"
rationale. The byte-less running scrub is the one conspicuous empty cell in that
matrix.

**Outcome:** add the missing wiring test (plus a sparse running-scrub fixture)
so the `Running { pct: None } -> Busy` invariant is pinned at the `cmd_idle`
boundary, matching the file's established pattern. This is a pure test backfill;
no production behavior changes.

## Changes

### 1. New fixture: `idle_scrub_running_no_bytes()`

File: `cli/src/test_fixtures/idle.rs` (add directly after `idle_scrub_running`
at `:210`).

Match the parser's `scrub_running_minimal` shape exactly: a sparse `Status:
running` record with no `Scrub started:`, no `Duration:`, and no byte lines. Do
**not** keep `Scrub started:` -- `_print_scrub_ss`
(`reference/btrfs-progs/cmds/scrub.c`) always emits `Scrub started:` /
`Status:` / `Duration:` together, so a started-but-no-`Duration` record is one
btrfs never produces; pairing them here would be a fabricated transcript. The
byte-less running record is a parser-contract fixture (see Context), so keep it
identical in spirit to the parser-side `scrub_running_minimal`. Reuse the
already-imported `mock_ok` helper (`super::shared::mock_ok`, `idle.rs:7`) and
`idle_mp()`.

```rust
/// Sparse running-scrub record (parser parity: `scrub_running_minimal`)
/// whose byte counters are absent, so `cmd_idle` must still report busy
/// with `pct: None`. This is a parser-contract / format-drift case, not
/// live btrfs output -- a real running scrub always carries byte counters
/// (`idle_scrub_running` is the percentage-bearing case).
pub(crate) fn idle_scrub_running_no_bytes() -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        },
        mock_ok(
            "btrfs scrub status --raw /mnt/storage",
            "UUID:             12345678-1234-1234-1234-123456789abc\n\
             Status:           running\n\
             Error summary:    no errors found\n",
        ),
    )
}
```

Parser confirmation: this stdout parses to
`Running { total_bytes: None, bytes_scrubbed: None, .. }` (the exact shape
`scrub_running_minimal` pins), which `idle.rs:100-104` maps via the `_ => None`
arm to `pct: None`. The `pct` outcome depends only on the byte fields, so the
absent `Scrub started:` / `Duration:` / estimate lines do not affect it.

### 2. Re-export the fixture

File: `cli/src/test_fixtures.rs` -- add `idle_scrub_running_no_bytes` to the
`pub(crate) use idle::{...}` block (`:162-166`). The block already carries
`#[allow(unused_imports)]`, so no warning risk.

### 3. New wiring test: `busy_when_scrub_running_no_bytes`

File: `cli/src/idle.rs` -- add the fixture to the test module's
`use crate::test_fixtures::{...}` import (`:122-126`), then add the test
immediately after `busy_when_scrub_running` (`:252`) so the two running-scrub
cases sit together. Match the existing wiring tests' style and Intent / Why it
exists / Scenario preamble.

```rust
// Intent: a running scrub whose byte counters are absent maps to
//   Busy(ScrubRunning { pct: None }), never Idle.
// Why it exists: the sibling busy_when_scrub_running only pins the
//   both-counters-present case (pct: Some). The Busy decision sits
//   outside the (bytes_scrubbed, total_bytes) match, but no cmd_idle
//   test pins that for pct: None. A refactor folding the Busy/Idle
//   choice into the pct match -- returning Idle when the percentage
//   cannot be computed -- would compile, keep parser tests green (they
//   classify ScrubState, not IdleResult), keep busy_when_scrub_running
//   green, and silently allow suspend whenever pct is unknowable. Same
//   wiring-pin contract as idle_when_scrub_{never,aborted,interrupted}.
// Scenario: btrfs-progs output drift (parser-compatibility risk) keeps
//   `Status: running` but reshapes/omits the `Total to scrub` /
//   `Bytes scrubbed` lines braid parses; the parser tolerates this
//   sparse record (scrub_running_minimal), pct is unknowable, and the
//   gate must still block suspend.
#[test]
fn busy_when_scrub_running_no_bytes() {
    let (scrub_req, scrub_out) = idle_scrub_running_no_bytes();
    let runner = MockRunner::default().with_output(scrub_req, scrub_out);
    let fs = IdleMockFs::with_exclop("none");

    let result = cmd_idle(&runner, &fs, &idle_mp());
    assert_eq!(
        result,
        IdleResult::Busy(BusyReason::ScrubRunning { pct: None })
    );
}
```

## Why this shape (and not an alternative)

- **Not a code change.** The match arm is already correct (`Busy` is
  unconditional). The risk is a future refactor, and the only structure-
  insensitive guard against it is a behavioral test at the `cmd_idle` boundary.
- **Not a table-driven merge** of the wiring tests. The file deliberately keeps
  each wiring case as its own `#[test]` with a full Intent/Why/Scenario preamble
  (AGENTS.md / `docs/dev/testing.md` convention). Collapsing them would shed the
  per-case rationale. Add one cell, keep the pattern.
- The assertion is behavioral (a running scrub blocks suspend even when its
  progress is unquantifiable) and structure-insensitive (asserts the public
  `IdleResult` from `cmd_idle`, not internals) -- it clears the test-quality bar
  rather than pinning an implementation detail.

## Verification

1. **Run the suite:** `just test-rust` -- the new test and the whole `idle`
   module must stay green. Targeted: `cargo test busy_when_scrub_running_no_bytes`
   from `cli/`.
2. **Prove it has teeth (do, then revert):** temporarily change the
   `_ => None` arm body region in `cli/src/idle.rs` so the byte-less case
   returns `IdleResult::Idle`, e.g.

   ```rust
   match (bytes_scrubbed, total_bytes) {
       (Some(scrubbed), Some(total)) =>
           IdleResult::Busy(BusyReason::ScrubRunning { pct: pct_from_bytes(scrubbed, total) }),
       _ => IdleResult::Idle, // injected regression
   }
   ```

   Confirm `busy_when_scrub_running_no_bytes` fails while the parser tests and
   `busy_when_scrub_running` still pass (demonstrating the gap this test closes),
   then revert the mutation.
3. No `flake.nix` change and no fixture-refresh: this is a pure Rust unit test
   (cargo auto-discovers it) using a hand-written mock, not a captured tool
   fixture. The ASCII-only check exempts tests, so the preamble/fixture text is
   fine as-is.

## Files touched

- `cli/src/test_fixtures/idle.rs` -- add `idle_scrub_running_no_bytes()` fixture.
- `cli/src/test_fixtures.rs` -- re-export it.
- `cli/src/idle.rs` -- import the fixture in `mod tests`; add
  `busy_when_scrub_running_no_bytes`.
