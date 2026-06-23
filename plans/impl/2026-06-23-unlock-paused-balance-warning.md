# Pivot: pin the unlock paused-balance warning at the unit level

## Context

`unlock_warns_on_paused_balance` (`cli/src/unlock.rs`) is a hollow test: it
asserts only `result.expect(...)` and would stay green if the post-mount
`emit_paused_balance_warning` call were deleted. `MockRunner` is a lenient
`HashMap` lookup with no consumption check, so the configured balance fixture
going unused trips nothing. The `cmd_unlock` -> emitter wiring on the success
path is therefore unpinned at the Rust level; only the slow VM lane
(`tests/cli/braid-unlock.py` Test 8) catches a regression.

A review proposed wrapping `cmd_unlock` in `status_tag::testing::capture_with_color`
and asserting the captured stderr contains the warning. **That fix cannot work
as written.** `capture_with_color` captures only bytes routed through
`status_tag::emit_status` (its thread-local `CAPTURED` buffer, fed by
`capture_line`). The paused-balance warning writes *directly* to
`&mut std::io::stderr()` (`cli/src/unlock.rs`, in `UnlockPlan::execute`),
bypassing that seam -- so the capture would be empty and the new assertion
would fail.

The root cause is an architectural inconsistency. Commit `b4732870`
("drop bespoke post-mount stderr-capture seam") deliberately established
`status_tag::emit_status` as *the* one canonical stderr seam for unlock and
rerouted the two enrichment warnings in the same `match` block through it
(see `plans/impl/2026-05-21-drop-bespoke-unlock-stderr-capture-seam.md`). The
paused-balance warning three lines below -- dating to the original
`bf8c555b` draft -- is the lone remaining stderr write in `cmd_unlock` that
still bypasses the seam. That bypass is simultaneously why the proposed test
fix fails and a residual violation of a documented invariant.

**Outcome:** route the paused-balance warning through the canonical seam,
making it both consistent with its siblings and capturable, then pin the
`cmd_unlock` -> warning wiring at the unit level.

## Approach

Turn the side-effecting emitter into a **pure** function that computes the
warning text, and let the single `unlock` call site decide the sink (the
canonical seam). This decouples "compute the warning" from "emit it," removes
a now-vestigial injectable writer whose only purpose was testability, and
yields the cleanest call site -- matching how `b4732870` left the sibling
warnings (`emit_status` at the call site).

### Production change

**1. `cli/src/status.rs` -- replace `emit_paused_balance_warning` with a pure
`paused_balance_warning`.**

Current: `pub fn emit_paused_balance_warning<R: CommandRunner>(runner, mount_point, out: &mut dyn Write) -> bool`
that probes via `get_balance_report` and, when `Paused`, writes four lines to `out`.

New: `pub(crate) fn paused_balance_warning<R: CommandRunner>(runner, mount_point) -> Option<String>`
returning the rendered block when paused, else `None`. Reuse `paused_balance_advice`
(unchanged) and build the byte-identical block with one `format!`:

```rust
matches!(get_balance_report(runner, mount_point), BalanceReport::Paused { .. })
    .then(|| {
        let advice = paused_balance_advice(mount_point);
        format!(
            "\n  {}\n    resume:  {}\n    cancel:  {}\n",
            advice.header, advice.resume_cmd, advice.cancel_cmd,
        )
    })
```

- Tighten `pub` -> `pub(crate)`: the only caller is in-crate (siblings
  `paused_balance_advice`/`get_balance_report` are already `pub(crate)`).
- Update the `///` to state the new boundary intent (returns the operator
  warning text for a paused balance, or `None`; caller owns the sink).
- `paused_balance_advice` and `get_balance_report` are unchanged, so the
  `doctor.rs` / `recover.rs` / `status.rs` consumers of those are untouched.

**2. `cli/src/unlock.rs` -- route the call site through the canonical seam.**

In `UnlockPlan::execute`, replace the direct-stderr call with:

```rust
// Best-effort: warn if a paused balance was found on mount. skip_balance
// prevents the kernel from resuming it silently, but the user should know
// so they can resume or cancel explicitly. Routed through the canonical
// status_tag::emit_status seam (see ADR / plan 2026-05-21) so it is
// consistent with the sibling enrichment warnings and capturable in tests.
if let Some(warning) = crate::status::paused_balance_warning(runner, mount_point) {
    crate::status_tag::emit_status(&warning);
}
```

Production bytes are unchanged: `emit_status` is `eprint!("{line}")` in non-test
builds, and the `format!` reproduces the exact four-line block (verified against
the existing `expected` literals). VM Test 8's `"paused balance"` substring grep
stays valid.

### Test changes

**3. `cli/src/unlock.rs` `unlock_warns_on_paused_balance` -- pin the wiring.**
Wrap `cmd_unlock` in `capture_with_color(false, || { result = Some(...) })`
(the idiom already used by `unlock_warns_when_post_mount_probe_errors`), keep
the success assertion, and add:

```rust
assert!(
    captured.contains("paused balance detected -- will not auto-resume"),
    "expected paused-balance warning on stderr, got: {captured:?}"
);
```

This pins both halves the test always meant to cover: unlock still returns
`Ok(())` *and* the warning fires through the real stderr seam.

**4. `cli/src/unlock.rs` `unlock_btrfs_balance_status_paused_classifies_as_paused`
-- adapt to the pure signature.** Replace the `Vec` sink + `bool` with
`assert_eq!(crate::status::paused_balance_warning(&runner, &mp), Some(expected))`,
keeping the exact four-line `expected`. Update its `// Why it exists` preamble:
`unlock_warns_on_paused_balance` now captures the header substring, so this test's
job is pinning the *exact full text* (resume/cancel command lines + formatting),
guarding parser/fixture/literal drift.

**5. `cli/src/status.rs` unit tests -- adapt to the pure signature.**
- `emit_paused_balance_warning_writes_to_buffer` -> `paused_balance_warning_returns_block_when_paused`:
  assert `paused_balance_warning(&runner, &status_mp()) == Some(expected)` (same literal).
- `emit_paused_balance_warning_silent_when_idle` -> `paused_balance_warning_none_when_idle`:
  assert it returns `None`.

## Files

- `cli/src/status.rs` -- `emit_paused_balance_warning` -> `paused_balance_warning`
  (signature, body, doc, visibility); two unit tests.
- `cli/src/unlock.rs` -- call site in `UnlockPlan::execute`; tests
  `unlock_warns_on_paused_balance` and `unlock_btrfs_balance_status_paused_classifies_as_paused`.

No doc changes: `docs/commands/unlock.md` and `docs/design/principles.md`
describe the warning generically and quote no exact text.

## Reuse

- `paused_balance_advice` (`cli/src/status.rs`) -- unchanged single source of the
  warning wording; keeps unlock and doctor from drifting.
- `get_balance_report` (`cli/src/status.rs`) -- unchanged probe; already swallows
  command/parse errors into `BalanceReport::Unknown`, so the post-mount path stays
  best-effort.
- `status_tag::emit_status` + `status_tag::testing::capture_with_color`
  (`cli/src/status_tag.rs`) -- the canonical emit + capture seam this change
  finishes adopting.

## Verification

- `just test-rust` (or `cargo test -p braid-cli`). Specifically:
  - `unlock_warns_on_paused_balance` now fails if the `paused_balance_warning`
    call is removed from `UnlockPlan::execute` (the capture assertion goes red) --
    confirm by temporarily deleting the call and re-running.
  - `paused_balance_warning_returns_block_when_paused`,
    `paused_balance_warning_none_when_idle`, and
    `unlock_btrfs_balance_status_paused_classifies_as_paused` pass against the
    pure signature.
  - The three IDLE-fixture `capture_with_color` unlock tests
    (`unlock_warns_when_post_mount_probe_errors`,
    `unlock_tolerates_post_mount_probe_mounted_false`,
    `unlock_tolerates_post_mount_save_membership_failure`) still pass unchanged.
- `cargo clippy -p braid-cli` clean (no unused-writer / dead-code warnings from
  the dropped parameter).
- VM lane unchanged in behavior: `tests/cli/braid-unlock.py` Test 8 still finds
  `"paused balance"` in stderr (production output is byte-identical).
- ASCII gate: `scripts/docs/check-output-ascii.py` -- the warning text is
  unchanged ASCII.
