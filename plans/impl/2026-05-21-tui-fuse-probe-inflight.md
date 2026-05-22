# Plan: fuse inflight flip into the matching effect-push branch in `Model::new`

## Context

A code-review finding ("Severity: Low, Category: Correctness") flagged
that `Model::new` at `cli/src/tui/model.rs:331-348` sets
`fan_probe_inflight` and `ups_probe_inflight` from
`*.is_some()` in statements that are textually separate from the
`if let Some(...) { effects.push(...) }` blocks that actually emit
`Effect::ProbeFan` / `Effect::ProbeUps`.

Today both halves are gated on the same predicate, so the invariant
holds: every `inflight=true` is paired with a real effect, and every
worker is infallible (`probe_fan_for_tui`, `probe_ups_for_tui` both
always send a finished event). The risk the finding identifies is
structural -- a future refactor that gates the effect push on an
additional condition (or makes emission fallible) without re-checking
the predicate that drives the flag would silently lock the TUI into
"inflight" forever, because:

- The `inflight` flag is set at construction.
- The `RefreshFan`/`RefreshUps`/`RefreshPool` handlers all early-return
  if the flag is `true` (the duplicate-probe guard at `app.rs:117`,
  `app.rs:127`, `app.rs:284`, `app.rs:316`).
- Only `Message::FanProbeFinished` / `Message::UpsProbeFinished` clear
  the flag, and those messages are only produced by the worker thread
  that the effect actually spawned.

The finding's *proposed* fix -- "move the inflight flip into
`execute_effect`" -- conflicts with the codebase's Elm-style boundary:
`execute_effect` takes `(Effect, &mpsc::Sender<Event>)` and
intentionally has no `&mut Model` access. The `BrowseRunCommand`
analogy in the finding is reversed: `browse/state.rs:540-547` sets
`loading=true` and bumps `command_gen` next to the effect emission
inside the update-path code, *not* in `execute_effect`. The same
pattern is already established at every other emission site for
`ProbeFan`/`ProbeUps` (`app.rs:116-132`, `app.rs:290-295`,
`app.rs:322-326`).

The pivot: keep the existing layering, fix the constructor's lone
deviation from the established pattern by fusing the flag flip into
the same `if let Some(...)` block as the effect push.

## Files to modify

- `cli/src/tui/model.rs` -- the constructor at lines 322-379.
- `cli/src/tui/app.rs` -- add two unit tests under the existing
  `mod tests` block.

## Change in `cli/src/tui/model.rs`

Replace the two separated "compute flag from `is_some()` + later
`if let Some(...)` push" pairs at lines 331-348 with a single
construction per subsystem that pushes the effect *and* sets the flag
to `true` in the same branch.

Sketch:

```rust
let mut fan_probe_inflight = false;
if let Some(fc) = fan_control.as_ref() {
    effects.push(Effect::ProbeFan {
        sysfs_root: std::path::PathBuf::from("/sys"),
        dev_root: std::path::PathBuf::from("/dev"),
        disk_by_id: disk_by_id.clone(),
        fan_control: fc.clone(),
    });
    fan_probe_inflight = true;
}

// Kick off the UPS probe immediately so the first render shows
// live state rather than a placeholder that disappears on the
// next poll tick.
let mut ups_probe_inflight = false;
if let Some(u) = ups_config.as_ref() {
    effects.push(Effect::ProbeUps {
        name: u.name.clone(),
    });
    ups_probe_inflight = true;
}
```

Preserve the existing UPS-section comment ("Kick off the UPS probe
immediately ...") -- it documents intent, not structure, and stays
correct.

The `Model { ... fan_probe_inflight, ... ups_probe_inflight, ... }`
construction at lines 349-378 stays unchanged; both local bindings
still flow into it.

### Why this is the right shape

- Removes the structural drift: the flag and the effect are now
  produced by the same branch of the same `if let`. A future
  maintainer who adds a condition or a fallible step to the effect
  push physically cannot leave the flag stranded.
- Matches the pattern every other emission site already uses (see the
  references above), so there is no new convention to learn.
- Stays inside the established Elm-style layering -- no signature
  change to `execute_effect`, no new `Event::ProbeStarted` round-trip.
- Touches one file, ~6 net lines of source.

### Reuse of `fan_probe_effect` / `ups_probe_effect`?

Considered and rejected. The helpers at `app.rs:57-74` take `&Model`,
but the constructor is still building the model when it needs the
effect -- reusing them would require constructing the model first and
then post-hoc collecting effects, a much larger restructure for no
runtime benefit. The constructor's inline `Effect::ProbeFan {...}` /
`Effect::ProbeUps {...}` literal is a tolerable, two-instance
duplication.

## Tests to add in `cli/src/tui/app.rs`

The existing `refresh_fan_emits_probe_when_idle` at `app.rs:977-994`
and the analogous UPS test pin the update-path invariant ("emit
effect AND set inflight"). Add the matching pair for the constructor
path, using the same preamble/style conventions described in
`AGENTS.md` (Intent / Why it exists / Scenario). Two tests per
subsystem:

1. `model_new_with_fan_control_emits_probe_and_sets_inflight`:
   Build a `Model::new(...)` with `fan_control = Some(...)`, assert
   the returned effect list contains exactly one `Effect::ProbeFan`,
   and assert `model.fan_probe_inflight == true`.

2. `model_new_without_fan_control_emits_no_probe_and_clears_inflight`:
   Build `Model::new(...)` with `fan_control = None`, assert the
   effect list contains no `Effect::ProbeFan`, and assert
   `model.fan_probe_inflight == false`.

3 + 4: mirror the above for `ups_config` / `Effect::ProbeUps` /
`model.ups_probe_inflight`.

These are behavioral (post-conditions on the public constructor
return values), structure-insensitive (no field of `Effect` other
than the variant is asserted, except optionally the carried config to
match the existing `refresh_fan_emits_probe_when_idle` style), and
they directly pin the invariant the pivot is meant to preserve.

Reuse the existing `sample_fan_control()` and `sample_ups_config()`
fixtures already used by the surrounding tests (see e.g.
`app.rs:980, 1007, 1083, 1110`). For the other `Model::new` arguments
that the constructor demands but these tests do not care about
(`disk_by_id`, `disk_luks_uuid`, `disk_devid`, `mount_point`,
`advisories`, `paths`), pass empty collections / a `tempfile::tempdir`
for `StatePaths::custom` the same way
`refresh_pool_with_fan_idle_emits_both` already does at
`app.rs:1005-1006`.

## Verification

1. `just test-rust` -- runs the new constructor tests plus the
   existing `refresh_fan_emits_*` / `refresh_ups_emits_*` tests; all
   should pass.
2. `cargo check -p braid-cli` is implied by `just test-rust`, but if a
   tight loop is wanted, that's the fast feedback step.
3. No NixOS VM test is needed: the change is internal to the TUI
   model layer, does not affect any parser output, systemd unit, or
   on-disk state, and has no observable user-visible behavior change
   today. The pivot is preemptive structural hygiene.

No fixture refresh is required; no parser-critical tool version is
involved.

## Out of scope

- `Effect::ProbePool` at `model.rs:324-330` is unconditional and has
  no inflight flag in the same shape (`PoolStatus::Loading` plays the
  role). Not touched by this plan.
- Reworking `execute_effect` to carry model state. Explicitly
  rejected above.
- Extracting a shared "subsystem probe kick-off" helper between the
  constructor and the `Refresh*` handlers. Out of scope for a
  Low-severity hygiene fix; the two-instance duplication is
  tolerable.
