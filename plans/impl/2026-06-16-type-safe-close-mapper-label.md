# Plan: make the close-trailer disk label type-safe (`close_mapper_best_effort` takes `&DiskName`)

## Context

braid's post-commit mapper-close trailer (`disk <label>: locking...` / `: locked` /
`: lock failed`) is produced by `close_mapper_best_effort` in
`cli/src/mapper_close.rs`. Its three production callers -- `replace`, `remove`, and
`recover` -- each derive `<label>` by stripping `braid-` off the *observed* mapper
basename:

```rust
let old_label = mapper.as_str().strip_prefix("braid-").unwrap_or(mapper.as_str());
```

This violates ADR 024's "Display code has an explicit join rule"
(`docs/design/decisions/024-luks-uuid-identity.md`): user-facing surfaces must
resolve a device's identity to its `DiskName`, never echo a drifted mapper basename.
braid deliberately tolerates mapper drift -- it closes the *observed* mapper, gated
by a UUID double-drift probe -- so when an operator has opened the old member under a
drifted mapper (`braid-WRONG`), the trailer prints `disk WRONG: locking...` instead
of `disk disk2`. That is the exact "echo the drifted mapper basename" anti-pattern the
ADR calls out. It is cosmetic (one status line; the close still targets the correct
dm slot and the destructive decision is gated by the UUID probe), but the bug is
copy-pasted across all three callers and is currently unpinned by tests.

`remove` leaks the same drifted basename in two *additional* surfaces outside the
close trailer: its `pool: removing {mapper}...` and `pool: {mapper} removed` progress
rows (`remove.rs#RemoveWorkPlan::execute`) interpolate the observed mapper, so under
drift they print `pool: removing braid-WRONG...`. These are the same display-join
violation, and they leave `remove` internally inconsistent -- its final
`Done. Disk 'disk2' removed from pool.` line already uses the `DiskName`. Unlike the
close trailer, these are free-form `format!` rows that the type change does not reach,
so they get a direct fix plus a test guard. (`replace` and `recover` have no analogous
progress-row leak -- their pool-level rows name no disk.)

The journaled operator name (a `DiskName`) is already in scope at every call site.
The ideal fix is not to pass the right string at each site -- that leaves the footgun
for the next caller -- but to make the footgun unrepresentable: change
`close_mapper_best_effort`'s `disk_label` parameter from `&str` to `&DiskName`, so a
stripped mapper string can no longer be passed. This matches braid's newtype-sealing
direction (recent commits: "seal MapperName/MountPoint inner fields", "introduce the
Fsid newtype", "make CredentialVerifyTarget construct-safe") and is directly analogous
to the existing "resolve credential-verify member names through the uuid join" fix.

**Outcome:** the drifted-mapper close trailer shows the operator name (`disk disk2`)
on all three commands, `remove`'s progress rows do too, and the close-trailer bug
class becomes a compile error rather than a convention.

## The fix

### 1. Type the parameter (the chokepoint) -- `cli/src/mapper_close.rs`

- `close_mapper_best_effort`: change `disk_label: &str` to `disk_label: &DiskName`.
- Add `DiskName` to the `use crate::types::{...}` import (currently imports only
  `MapperName`).
- The three `format!("disk {disk_label}: ...")` lines need **no change**: `DiskName`
  implements `Display` rendering the bare name (`cli/src/types.rs`,
  `impl fmt::Display for DiskName`).
- Optional: extend the function's doc comment to note `disk_label` is the journaled
  operator name (so a future reader sees the `&DiskName` type is load-bearing, not
  incidental).

This single type change turns the bug class into a compile error.

### 2. Pass the journaled `DiskName` at each call site

| File / site | Current | Change |
|---|---|---|
| `cli/src/replace.rs`, `ReplacePlan::execute` Live-close block | derives `old_label` by stripping `mapper` | delete the `old_label` binding; pass `&old_name` -- the `DiskName` destructured from `work_plan` at the top of `execute`, the same value cloned into the journal at this site |
| `cli/src/remove.rs`, `RemoveWorkPlan` execute | derives `close_label` by stripping `mapper_str` | delete `close_label`; pass `&work_plan.name` -- the `DiskName` already used for the "removed" messaging and journaled into `OpKind::Remove` |
| `cli/src/recover.rs`, `close_old_mapper_best_effort` | derives `old_label` by stripping `mapper` inside the helper | add a `disk_label: &DiskName` param (place it after `mapper`, mirroring `close_mapper_best_effort`); delete the strip; pass it through to the inner call |
| `cli/src/recover.rs`, sole caller of that helper | destructures `journal::OpKind::Replace { old_uuid, .. }` | widen to `{ old_uuid, old_name, .. }` and pass `old_name`; **keep** the `else { unreachable!(...) }` |

Notes:
- `mapper` / `old_uuid` (replace, recover) stay -- only their use as a *label source*
  is removed. In `remove`, `mapper_str` is instead deleted entirely (see section 4):
  the close-label fix and the progress-row fix together remove its last uses.
- No `clone` anywhere: all are borrows of in-scope `DiskName`s. The journal stores
  `OpKind::Replace.old_name` and `OpKind::Remove.name` as `DiskName` (not `String`),
  so recover passes a `&DiskName` with zero parsing.
- In `replace.rs`, prefer the stack-local `old_name` from the `work_plan` destructure
  over adding a binding inside the journal `if let`; both are the same value and the
  local keeps the pattern minimal.
- The dm slot that gets closed (the `mapper` argument) and the UUID double-drift gate
  are untouched. **This is a pure presentation fix.**

### 3. Fix the unit-test harness (mechanical, compile-driven)

`cli/src/mapper_close.rs` tests route through `run_best_effort`, which passes the
literal `"disk2"` as `disk_label`. After the signature change it must build and pass
`&DiskName::parse("disk2").unwrap()`. The asserted output strings
(`[wait] disk disk2: locking...`, etc.) are unchanged because `Display` renders
`disk2`.

### 4. Render `remove`'s progress rows from the `DiskName` -- `cli/src/remove.rs`

Independent of the close trailer, `RemoveWorkPlan::execute` prints two progress rows
from the observed mapper:

```rust
&format!("pool: removing {mapper_str}..."),   // -> pool: removing braid-WRONG...
&format!("pool: {mapper_str} removed"),       // -> pool: braid-WRONG removed
```

Render both from `work_plan.name` (the `DiskName`) instead -- `pool: removing
{name}...` / `pool: {name} removed`. This matches `remove`'s own final
`Done. Disk 'disk2' removed from pool.` line and satisfies the ADR 024 display join.
Once the close-label fix (section 2) drops the `close_label` use and this change drops
the two display uses, the `let mapper_str = work_plan.target_mapper.as_str();` binding
is dead -- delete it. The actual `btrfs device remove` still targets
`work_plan.target_mapper.dev_path()`, unchanged. These rows are free-form `format!`s,
not the typed chokepoint, so the regression test (not the compiler) is their guard.

## Explicitly out of scope

- **No `probe_then_close` helper extraction.** The three sites share a
  probe-then-`match MapperOwnership` skeleton, but diverge semantically on two arms:
  the `Inactive` arm (`replace`/`remove` call `warn_close_skipped_inactive`; `recover`
  is intentionally silent -- inactive is normal during replay) and the post-success
  trailer (`replace`/`recover` print "Old device closed..."; `remove` is silent). A
  shared helper would need 2-3 behavior knobs encoding exactly those differences,
  hiding nothing while forcing readers to round-trip through the helper. The
  genuinely-identical parts (`probe_observed_mapper_uuid`, `warn_close_skipped_inactive`)
  are already shared in `cli/src/probe_mapper_uuid.rs`. Leave the per-command policy
  inlined and commented where it is.
- **`add.rs` and `mount.rs` rollback paths.** Both use the same strip idiom but call
  `close_mapper_with_retry` directly (not `close_mapper_best_effort`) to roll back
  mappers they opened *this same run* via `mapper_name(&name)`. There is no
  plan/execute drift window, so the basename equals the freshest name available and no
  journaled `DiskName` is in scope. Not the ADR 024 violation; left unchanged.
- **`config::name_from_mapper`** stays -- it is the sanctioned display-only mapper
  parser for diagnostics. This refactor just stops the close sites from reinventing it
  inline. (If the refactor orphans it, that is a separate cleanup, not this change.)
- **The two `MapperOwnership` enums** (`probe_mapper_uuid.rs` and `luks.rs`) are not
  conflated or merged here. The three target sites all use the `probe_mapper_uuid` one.

## Tests

The bug lives at the *call sites* (which name they choose to pass), so the catching
tests must too -- the chokepoint formatting is already covered by the existing
`mapper_close.rs` tests.

Add one regression per command (`replace`, `remove`, `recover`; 3 total), reusing each
command's existing `braid-WRONG` drift setup (observed mapper `braid-WRONG`,
journaled / work-plan name `disk2`, UUID probe returns `Owned`). Capture stderr with
`crate::status_tag::testing::capture_with_color(false, || { ... })` and assert:

- positive: `captured.contains("[wait] disk disk2: locking...")` (and
  `... disk disk2: locked`);
- **negative**: `!captured.contains("WRONG")`.

The negative assertion is load-bearing: it fails on today's code and cannot be
satisfied by a half-fix. These are behavioral and structure-insensitive (they assert
user-visible output, not call shapes or helper existence), so they survive future
internal reshuffling. The existing `braid-WRONG` drift tests already assert the close
*targets* the observed mapper; these add the missing *label* assertion. For `remove`,
the whole-captured `!contains("WRONG")` now also guards the two progress rows fixed in
section 4, not just the close trailer -- keep it whole-captured for that reason.

Caveat: each regression must drive a *successful* (exit-0) close. The busy-retry
diagnostic inside `close_mapper_with_retry` (`cli/src/mapper_close.rs`) legitimately
echoes the raw `MapperName` (`cryptsetup close braid-WRONG busy, retrying...`) and is
not a `disk_label` surface; a busy-then-success setup would trip `!contains("WRONG")`
on that intended line. The happy-path single-close setup avoids it.

If minimizing the unit tests, the `recover` test is the single highest-value one -- it
exercises both the new `disk_label` parameter threading and the caller-side `old_name`
destructure.

The only VM-test change is mechanical: `remove`'s progress wording changes, so update
`tests/cli/braid-remove-disk.py` expectations `pool: removing braid-disk2...` ->
`pool: removing disk2...` and `pool: braid-disk2 removed` -> `pool: disk2 removed`
(the `disk disk2:` close-trailer expectations there are already correct, and the
`/dev/mapper/braid-disk2` device-path checks are unaffected). No *new* VM test is
needed: the closed dm slot is unchanged and already VM-covered
(`tests/cli/luks-mapper-drift.py`).

## Verification

```
cargo build           # fails between steps 1 and 2 -- the type change forces the call sites (intended)
just test-rust        # unit tests, incl. the three new drift-label regressions
just check-output-ascii   # trailer strings stay ASCII (unchanged, but cheap to confirm)
just test-vm braid-remove-disk   # VM test whose remove-progress wording changes (braid-disk2 -> disk2)
```

Expected: the three new unit assertions fail before the call-site edits (proving they
catch the bug) and pass after; `braid-remove-disk` passes with the updated `disk2`
progress-row expectations.

## Risks / notes

- The type change forces edits at all three call sites + the test harness before it
  compiles. That coupling is the safety property, not breakage.
- `recover.rs`: keep the `else { unreachable!("post-maintenance recovery runs only for
  Replace journals") }` when widening the destructure; the bound `old_name` is a
  `&DiskName` borrow (no clone).
- Single commit: the type change + three call sites + harness + `remove` progress-row
  fix + the three regressions + the `braid-remove-disk.py` expectation update are one
  atomic "make the bug class unrepresentable" change.

## Implementation notes

- `RemovePlan::execute` now emits its balance/remove status rows through
  `status_tag::emit_status` instead of direct `eprint!`, preserving production bytes
  while letting the drift-label regression capture the remove progress rows and close
  trailer through the same status test seam.
