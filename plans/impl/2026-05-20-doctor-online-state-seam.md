# Replace doctor's bespoke braid-online ActiveState parser with the typed seam

## Context

`cli/src/doctor.rs` carries a private 3-variant `BraidOnlineActiveState` enum
plus two helpers (`read_braid_online_active_state`,
`classify_braid_online_active_state`) that re-classify
`systemctl show -P ActiveState braid-online.service` output for the
`check_braid_online_active_when_mounted` check. They were added on 2026-04-27
in `f9e6a36 fix(cli): classify braid-online doctor states` -- before the
lock-migration on 2026-05-19 (`ff6f766 fix(lock): move pool lock ownership
into rust dispatch`) extracted `UnitActiveState` and
`OnlineStateOps::unit_active_state` into `cli/src/online_state.rs` as the
project's single seam for `braid-online.service` lifecycle reads.

The two paths have drifted, and the typed seam itself has a related gap:

1. The doctor classifier flattens systemctl output into 3 variants
   (`OkSettled`, `Activating`, `Fail`) and must be kept in lockstep with the
   richer `UnitActiveState` (which `lock.rs` and `main.rs` already use). Any
   future variant or wording change has to land in two places.
2. `read_braid_online_active_state` ignores `exit_status`. A non-zero
   systemctl exit with empty stdout renders as the empty-state Fail line
   "braid-online.service is " (trailing space) instead of a diagnostic
   message. The typed seam already returns
   `OnlineError::SystemctlShow { exit_code, stderr }` for that case.
3. `UnitActiveState` itself is missing the `maintenance` variant. systemd's
   `unit_active_state_table` at
   `reference/systemd/src/basic/unit-def.c:104-113` lists eight active-state
   words; the Rust enum at `cli/src/online_state.rs:19-28` parses seven of
   them and folds `"maintenance"` into `Unknown("maintenance")`. For a
   service unit this maps to `SERVICE_CLEANING`
   (`reference/systemd/src/core/service.c:88`) -- an ExecCleanCommand is
   running, so `braid-online.service`'s ExecStop will not fire on shutdown.
   Before the doctor rewrite depends on `online_state.rs` as canonical, the
   seam needs to cover the full vocabulary or the new code will render a
   known-bad systemd state as "unrecognised".

Intended outcome: one source of truth for ActiveState classification on this
unit, the full systemd vocabulary in the canonical enum, a richer Fail
message when systemd reports an unrecognised state or non-zero exit, and the
deletion of three doctor-private items.

## Files to modify

- `cli/src/doctor.rs` -- delete private enum + helpers, rewrite the check to
  call through `OnlineStateOps`. Specifically: `cli/src/doctor.rs:179-184`
  (enum), `cli/src/doctor.rs:1005-1020` (helpers), `cli/src/doctor.rs:1059-1080`
  (the match in the check), and the imports near the top.
- `cli/src/online_state.rs` -- add the missing `Maintenance` variant to
  `UnitActiveState` at `:19-28`, parse `"maintenance"` at `:30-43`, fold
  `Maintenance` into the existing no-op arm of `mark_online` at `:281-285`,
  and add a small `systemd_word(&self) -> &str` method on `UnitActiveState`
  next to `parse`. Keeps the systemd vocabulary in one place.

## Reuse, not new code

`cli/src/online_state.rs` already exposes everything we need:

- `pub const BRAID_ONLINE_UNIT: &str = "braid-online.service";`
  (`cli/src/online_state.rs:16`)
- `pub enum UnitActiveState { Active, Activating, Deactivating, Inactive,
  Failed, Reloading, Refreshing, Unknown(String) }` (`:18-28`) -- the
  `Unknown(String)` variant already carries the systemctl word or stderr that
  the doctor currently throws away. Step 1 below adds the missing
  `Maintenance` variant before the doctor rewrite leans on this enum as
  canonical.
- `trait OnlineStateOps { fn unit_active_state(&self, unit: &str) ->
  Result<UnitActiveState, OnlineError>; ... }` (`:91-99`) plus
  `RealOnlineStateOps::new(runner: &'a dyn CommandRunner)` (`:106-109`),
  whose impl at `:113-126` already checks `output.exit_status != 0` and
  surfaces it as `OnlineError::SystemctlShow { unit, exit_code, stderr }`.
- The `&R: CommandRunner` -> `&dyn CommandRunner` coercion is the same
  pattern already in `main.rs:407,578,866,1003` and `lock.rs:983`.

The TUI sibling at `cli/src/tui/probe.rs:784-803` (`probe_daemon_status`) is
deliberately different (arbitrary daemons, coarser `DaemonStatus` that folds
`activating|reloading|deactivating` into `Transitioning`, deliberate tolerance
of non-zero exits per the tests at `cli/src/tui/probe.rs:2689-2699`). Leave
it alone -- it is not a unification target.

## Implementation

### 1. Round out `UnitActiveState` and add `systemd_word`

All in `cli/src/online_state.rs`:

**a. Add the `Maintenance` variant** at `:19-28`:

```rust
pub enum UnitActiveState {
    Active,
    Activating,
    Deactivating,
    Inactive,
    Failed,
    Maintenance,
    Reloading,
    Refreshing,
    Unknown(String),
}
```

**b. Parse `"maintenance"`** in `UnitActiveState::parse` at `:31-43`:
add `"maintenance" => Self::Maintenance` to the match arms.

**c. Preserve `mark_online`'s no-start semantics** at `:268-286`: add
`UnitActiveState::Maintenance` to the existing no-op match arm alongside
`Active | Activating | Deactivating | Reloading | Refreshing`. Don't try to
`systemctl_start` while the unit is in the cleaning sub-state -- that races
the cleanup hook and the safety-critical reasoning (ExecStop will not fire
during cleaning) is what the doctor surface will report on the read side.

**d. Add `systemd_word`** alongside the existing `parse` impl block:

```rust
impl UnitActiveState {
    pub fn systemd_word(&self) -> &str {
        match self {
            Self::Active => "active",
            Self::Activating => "activating",
            Self::Deactivating => "deactivating",
            Self::Inactive => "inactive",
            Self::Failed => "failed",
            Self::Maintenance => "maintenance",
            Self::Reloading => "reloading",
            Self::Refreshing => "refreshing",
            Self::Unknown(reason) => reason.as_str(),
        }
    }
}
```

A short `///` doc comment per `AGENTS.md`: "Canonical systemd word for known
variants; the captured reason text for `Unknown` -- callers render this
verbatim into user-facing diagnostics so the parser and rendering stay in
one place."

### 2. Rewrite `check_braid_online_active_when_mounted`

Replace `cli/src/doctor.rs:1059-1080` with:

```rust
let outcome = RealOnlineStateOps::new(ctx.runner).unit_active_state(BRAID_ONLINE_UNIT);
match outcome {
    Ok(state @ (UnitActiveState::Active
              | UnitActiveState::Reloading
              | UnitActiveState::Refreshing)) => CheckResult::ok(
        name,
        format!("braid-online.service is {}", state.systemd_word()),
    ),
    Ok(UnitActiveState::Activating) => CheckResult::warn(
        name,
        "braid-online.service is activating -- UPS shutdown hook is not confirmed yet; re-run braid doctor shortly",
    ),
    Ok(state @ (UnitActiveState::Deactivating
              | UnitActiveState::Inactive
              | UnitActiveState::Failed
              | UnitActiveState::Maintenance)) => CheckResult::fail(
        name,
        format!(
            "braid-online.service is {} -- UPS shutdown will not unmount the pool. \
             Run `systemctl start braid-online.service` or re-run `braid unlock`.",
            state.systemd_word()
        ),
    ),
    Ok(UnitActiveState::Unknown(reason)) => CheckResult::fail(
        name,
        format!(
            "braid-online.service ActiveState unrecognised ({reason}) -- UPS shutdown will not unmount the pool. \
             Run `systemctl start braid-online.service` or re-run `braid unlock`."
        ),
    ),
    Err(e) => CheckResult::fail(
        name,
        format!(
            "braid-online.service ActiveState read failed: {e} -- UPS shutdown will not unmount the pool. \
             Run `systemctl start braid-online.service` or re-run `braid unlock`."
        ),
    ),
}
```

Wording preserved so the existing test assertions still hold:
- `braid_online_check_ok_when_settled_success_state` asserts
  `r.message.contains(status)` for each of `active`/`reloading`/`refreshing`.
  `systemd_word()` returns the same word -> still contained.
- `braid_online_check_warns_when_activating` asserts the Warn message
  byte-for-byte (`cli/src/doctor.rs:4436-4439`). Kept byte-identical above.
- `braid_online_check_fails_when_inactive_and_mounted` asserts the message
  contains `inactive` and `UPS shutdown`. Both still present.
- `braid_online_check_fails_for_unsafe_systemctl_states` runs
  `["deactivating", "failed", "unknown", "", "bogus"]`. `deactivating`/
  `failed` go through the Fail arm with `systemd_word()`; `unknown`/`bogus`/
  `""` go through `Unknown(reason)`. The test skips containment for the
  empty case (`if !status.is_empty()` at `cli/src/doctor.rs:4476-4478`), so
  the new "unrecognised ()" wording is fine. `"unknown"` and `"bogus"` are
  contained in the new message.

### 3. Delete the dead doctor-private items

Remove from `cli/src/doctor.rs`:

- `enum BraidOnlineActiveState` at `:179-184`
- `fn classify_braid_online_active_state` at `:1005-1011`
- `fn read_braid_online_active_state` at `:1013-1020`

And add to the imports near `cli/src/doctor.rs:20`:

```rust
use crate::online_state::{
    BRAID_ONLINE_UNIT, OnlineStateOps, RealOnlineStateOps, UnitActiveState,
};
```

(`OnlineStateOps` is imported because `RealOnlineStateOps::unit_active_state`
is defined on the trait, so the trait has to be in scope at the call site.)

## Tests

Existing tests at `cli/src/doctor.rs:4220-4533` all keep passing with the
wording in step 2 for the happy paths. Three test changes lock in the new
behaviour:

**1. Extend the parametrized doctor Fail test** with `"maintenance"`, plus a
known-state wording assertion. At `cli/src/doctor.rs:4451`, the status list
currently reads `["deactivating", "failed", "unknown", "", "bogus"]`. Add
`"maintenance"`. The existing in-loop assertions (`r.message.contains(status)`
+ `UPS shutdown` + actionable hint) are necessary but not sufficient on
their own -- the `Unknown(reason)` wording from step 2 also contains
`maintenance` and the hint, so the parametrized assertions alone cannot
distinguish "parsed as Maintenance, rendered via `systemd_word`" from
"folded into `Unknown("maintenance")` and rendered via `unrecognised`". Add
a maintenance-specific assertion inside the loop:

```rust
if status == "maintenance" {
    assert!(
        r.message.contains("braid-online.service is maintenance"),
        "expected known-state Fail wording, got: {}", r.message,
    );
}
```

That sentence is only produced via the Fail arm at step 2 lines 156-166 (the
`is {systemd_word()}` branch). Dropping the `"maintenance"` arm in
`UnitActiveState::parse`, or dropping the `Maintenance` arm in `systemd_word`,
or dropping `Maintenance` from the doctor Fail arm all reroute the
maintenance case through `Unknown(reason)` -> "unrecognised (maintenance)",
which fails this assertion. This is the behavioural coverage the finding
asks for.

**2. Extend the parametrized `mark_online` no-start test** with
`UnitActiveState::Maintenance`. At
`cli/src/online_state.rs:488-513` (`mark_online_starts_when_lifecycle_enabled`),
the test already asserts no-start for `Deactivating`. Add a sibling
assertion for `Maintenance`, locking in the preserved current behaviour
(no `systemctl start` race against systemd's cleanup hook).

**3. Add a new doctor test for non-zero `systemctl show` exit** -- the gap
that motivates the migration in the first place:

- **Intent:** `check_braid_online_active_when_mounted` reports a Fail with a
  diagnostic message when `systemctl show` exits non-zero, instead of the
  empty-state "braid-online.service is " line the old code produced.
- **Why it exists:** the silent-exit-status weakness flagged by the finding
  was untested -- without coverage, the same drift can re-open.
- **Scenario:** the unit is masked or absent; systemctl exits 4 with stderr
  "Unit braid-online.service not loaded." Doctor must Fail and the message
  must mention `braid-online.service` and the systemctl error so an operator
  can diagnose.

Construct `RawCommandOutput` inline with `exit_status: 4` and
`stderr: "Unit braid-online.service not loaded."`; the shared fixture
`cli/src/test_fixtures/doctor.rs:268-279` hardcodes `exit_status: 0` and
doesn't fit. Assert `r.status == Fail` and that the message contains all of:

- `braid-online.service` (unit name surfaced)
- `UPS shutdown will not unmount the pool` (safety implication)
- `exit 4` (the exit-code substring from `OnlineError::SystemctlShow`'s
  `Display` -- see the `#[error]` attribute at
  `cli/src/online_state.rs:49-54`: `"systemctl show {unit} failed (exit
  {exit_code}): {stderr}"`)
- `Unit braid-online.service not loaded.` (the stderr substring carried
  through by the same `Display`). Without this assertion an implementation
  could drop `{e}` or hand-roll an error message that hides stderr and still
  pass, losing the operator's diagnostic line.

## Verification

- `just test-rust` -- runs `cargo test -p braid-cli`. Exercises the rewritten
  `check_braid_online_active_when_mounted` against all existing braid-online
  doctor tests plus the new non-zero-exit test, and the existing
  `online_state.rs` tests including the `systemd_word` mapping if I add a
  unit test for it (optional, low value, skip unless `just test-rust` flags
  drift).
- Type-check the `&R` -> `&dyn CommandRunner` coercion at the doctor call
  site -- `cargo check -p braid-cli` (or just rely on `just test-rust`).
- `just test-vm` for any UPS / braid-online integration check that exercises
  doctor under UPS (the existing UPS VM tests cover this end-to-end). Run
  only if the Rust-level checks pass.

## Out of scope

- TUI daemon-status probes (`cli/src/tui/probe.rs:784-803` and friends):
  different semantics, intentionally separate.
- Any docs change. `docs/decisions/020-ups-integration.md` and
  `docs/decisions/018-systemd-lifecycle.md` don't reference the
  doctor-internal classifier.
- CLI output style is already `--` (double hyphen) throughout the new
  messages per `AGENTS.md`.
