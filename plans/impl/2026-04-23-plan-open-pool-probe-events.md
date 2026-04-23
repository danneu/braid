# Fix: probe-time eprintln inside `plan_open_pool`

## Context

`cli/src/mount.rs::plan_open_pool` prints five informational lines
to stderr while probing (`pool already mounted at`, `[skip] disk: ...
not found`, `[skip] disk: ... LUKS header ...`, `[ok] disk: ...
already open`, `[ok] disk: ... found`). The function is otherwise
pure -- probe + validate, no mutations -- but the embedded I/O
couples it to a single output style.

Consequences today:

- `cli/src/tui/probe.rs` already avoids this by reimplementing the
  probe loop (`probe_pool_state`, lines 201-248). That duplication
  exists *because* `plan_open_pool` is not usable from the TUI:
  calling it would spray raw stderr through the TUI frame.
- The unit test `plan_open_pool_degraded_first_absent_picks_open_mapper`
  (`cli/src/mount.rs:1562`) prints five probe lines during `cargo
  test` runs with no way to silence them.
- Dry-run callers (`cmd_unlock` dry-run at `cli/src/unlock.rs:48`,
  `cmd_recover` dry-run at `cli/src/recover.rs:169`) inherit the
  same stderr output before the dry-run step plan. Current UX is
  consistent across dry and wet runs -- preserve that, just move
  the I/O out of the probe primitive.

This finding is from `feature-findings/unlock.md` (Simplicity
section, "Probe-time eprintln lines buried inside `plan_open_pool`").

## Design

Return structured events; callers render. Pure probe function,
rendering becomes the caller's explicit decision.

### Critical constraint: events must be returned on BOTH success and error paths

Today `plan_open_pool` prints probe lines inline as it walks the
membership, so those lines are already on stderr *before* any
error return (UUID mismatch at `mount.rs:212`, `ProbeError` from
`probe_config_disk`, the `no unlockable disks` error at line 236,
and most importantly `DegradedRefused` at line 243). Users and
tests rely on seeing the per-disk context ahead of the error. A
`Result<PlanOutcome, MountError>` shape would drop every event
accumulated before an `Err(...)` -- a user-facing regression.

The fix: an infallible outer function returning a report that
carries events plus the inner fallible outcome.

```rust
// cli/src/mount.rs
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeEvent {
    AlreadyMounted { mount_point: String },
    DiskAbsent { name: String },
    DiskLuksHeaderUnreadable { name: String },
    DiskLuksHeaderDamaged { name: String },
    DiskAlreadyOpen { name: String },
    DiskAvailable { name: String },
}

pub struct PlanReport {
    pub events: Vec<ProbeEvent>,
    pub result: Result<Option<OpenPlan>, MountError>,
}

pub fn plan_open_pool<R, F>(...) -> PlanReport {
    let mut events = Vec::new();
    let result = plan_open_pool_inner(runner, fs, config, membership,
        allow_degraded, command_hint, &mut events);
    PlanReport { events, result }
}

fn plan_open_pool_inner<R, F>(
    ..., events: &mut Vec<ProbeEvent>,
) -> Result<Option<OpenPlan>, MountError> {
    // existing body, with events.push(...) replacing each eprintln!
}
```

Callers preserve existing ordering by rendering *before*
propagating the error:

```rust
let report = mount::plan_open_pool(runner, fs, config, membership,
    allow_degraded, "unlock");
mount::print_probe_events(&report.events);
let plan = report.result?;
match plan { Some(plan) => ..., None => ... }
```

### Pure renderer + thin stderr wrapper (for testability)

Split rendering so the format contract is behaviorally pinnable:

```rust
pub fn render_probe_events(events: &[ProbeEvent]) -> String { ... }

pub fn print_probe_events(events: &[ProbeEvent]) {
    let text = render_probe_events(events);
    if !text.is_empty() {
        eprint!("{text}");
    }
}
```

`render_probe_events` uses the existing `tag()` helper
(`cli/src/mount.rs:71`) and reproduces the current line format
character-for-character.

### Why a named `PlanReport` struct (not a tuple or `Result<_, (events, err)>`)

Three production callers + two dry-run branches all need both
fields; a named struct lets each callsite pick up `report.events`
and `report.result` by name. A `Result<_, (Vec<_>, MountError)>`
would pollute the error type and break `?`-propagation
ergonomics. The inner/outer split keeps `?` working inside
`plan_open_pool_inner` while the outer function is infallible.

### What is *not* in scope

- Do not change dry-run vs wet-run rendering behavior. The
  `feature-findings/unlock.md` note about dry-run noise asymmetry
  is a separate UX question.
- Do not attempt to unify `tui/probe.rs` with `plan_open_pool`.
  That is a larger refactor (consolidating the refinement logic
  duplicated at `tui/probe.rs:238-244` and `mount.rs:192-201`) and
  is distinct from the eprintln-coupling fix.
- Do not change the `tag()` format or the exact line contents.

## Files to change

**`cli/src/mount.rs`**
- Add `pub enum ProbeEvent` and `pub struct PlanReport`.
- Split `plan_open_pool` into an infallible outer returning
  `PlanReport` and a private `plan_open_pool_inner` returning
  `Result<Option<OpenPlan>, MountError>` that pushes into a
  `&mut Vec<ProbeEvent>`.
- Replace each of the five `eprintln!` at lines 168, 182, 206,
  221, 227 with an `events.push(ProbeEvent::...)`.
- Add `pub fn render_probe_events(&[ProbeEvent]) -> String` (pure)
  and `pub fn print_probe_events(&[ProbeEvent])` (thin `eprint!`
  wrapper). `render_probe_events` is where the format lives; the
  output must be byte-for-byte identical to today's stderr lines.
- Update the regression test at line 1562 to unwrap
  `report.result` and the inner `Option`. Also add an `assert_eq!`
  on `report.events` to pin the event sequence --
  `[DiskAbsent{"disk1"}, DiskAlreadyOpen{"disk2"},
  DiskAlreadyOpen{"disk3"}]`. This turns previously uncaptured
  stderr noise into a real behavioral assertion.
- Update the test helper `open_and_mount_for_test`
  (`cli/src/mount.rs:697-717`): the current `match
  plan_open_pool(...)? { Some(p) => p, None => return Ok(false) }`
  becomes `let report = plan_open_pool(...); let plan = match
  report.result? { Some(p) => p, None => return Ok(false) };`.
  Many mount-module unit tests flow through this helper, so
  without this edit the module will not compile. No need to render
  events from the test helper -- tests read them from the return
  value when they care.
- Add a new test `render_probe_events_formats_mixed_probe_result`:
  build a fixture `Vec<ProbeEvent>` covering every variant
  (`AlreadyMounted`, `DiskAbsent`, `DiskLuksHeaderUnreadable`,
  `DiskLuksHeaderDamaged`, `DiskAlreadyOpen`, `DiskAvailable`) and
  assert on the exact multiline string. Intent block: byte-for-byte
  compatibility with pre-refactor stderr; reverts that change any
  wording/padding must fail this test.
- Add a degraded-refused event retention test
  `plan_open_pool_emits_events_before_degraded_refused`: construct
  a scenario where `DegradedRefused` fires, assert
  `report.result.is_err()` **and** `report.events` contains the
  per-disk events that preceded the error. Intent: pins the High
  reviewer finding -- losing events on the error path is a
  regression.

**`cli/src/unlock.rs`**
- `cli/src/unlock.rs:39` (shared for wet + dry-run at line 48):
  after the call, invoke
  `mount::print_probe_events(&report.events)`, then use
  `report.result?` and destructure the `Option<OpenPlan>`.

**`cli/src/recover.rs`**
- `cli/src/recover.rs:169` (dry-run) -- same pattern.
- `cli/src/recover.rs:230` (initial wet-run) -- same pattern.
- `cli/src/recover.rs:677` (post-relock cycle) -- same pattern. The
  post-relock cycle re-probes and re-renders the same lines today;
  preserve that by calling `print_probe_events` again. If the
  reviewer later decides the second render is noise, that is a
  one-line deletion at a clearly labeled callsite -- cheaper than
  inventing a silencing flag now.

## Critical files to read before editing

- `cli/src/mount.rs:153-260` -- current `plan_open_pool` body.
- `cli/src/mount.rs:1562-1611` -- the regression test that currently
  asserts only on `plan` fields (the one bonus-regression-target).
- `cli/src/unlock.rs:30-100` -- dry-run and wet-run callsites.
- `cli/src/recover.rs:160-240` and `cli/src/recover.rs:670-700` --
  callsites including the post-relock cycle.
- `cli/src/tui/probe.rs:201-248` -- confirm we are *not* touching
  the TUI probe (and keep the refinement comment consistent).

## Verification

1. **Rust unit tests**: `just test-rust`. Three behavioral
   contracts covered:
   - `plan_open_pool_degraded_first_absent_picks_open_mapper`
     (updated) pins the event *sequence* for the non-error happy
     path.
   - `render_probe_events_formats_mixed_probe_result` (new) pins
     byte-for-byte stderr formatting for every `ProbeEvent`
     variant. Any wording, padding, or tag drift in
     `render_probe_events` fails this test.
   - `plan_open_pool_emits_events_before_degraded_refused` (new)
     pins that probe events accumulate on the error path --
     directly targets the reviewer's High finding. If a future
     refactor accidentally returns `Err` before events flush, this
     fails.
2. **VM suite**: `just test-vm` -- specifically
   `braid-unlock`, `degraded-raid1`, `no-silent-degraded`,
   `auto-unlock-key-{present,missing,wrong}`, and
   `pool-lock-contention`. None currently assert on the per-disk
   probe eprintln lines (confirmed by grep across `tests/` and
   `cli/tests/`); they should pass unchanged. The one grep hit,
   `tests/cli/braid-unlock.py:235`, asserts on the
   `format_degraded_refused` error body, not the probe lines, and
   is unaffected.
3. **Parser canary**: not required; this change does not touch any
   parser or tool-facing command.

No fixture refresh is required (no parser-critical tool version
change).
