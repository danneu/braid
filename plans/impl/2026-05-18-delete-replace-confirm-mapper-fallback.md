# Plan: Delete dead mapper-keyed fallback in replace confirm block

## Context

During the phase-3c UUID-identity migration (commit `74298d3`), the
`!params.yes` confirm block in `ReplacePlan::execute` was switched from
mapper-keyed to UUID-keyed `PoolDevice` lookup, but a mapper-keyed
`.or_else(...)` fallback was left behind with a comment marking it as
a test accommodation:

```rust
ReplaceSource::Live { mapper, .. } => pool
    .devices
    .iter()
    .find(|d| d.luks_uuid == old_uuid)
    .map(|d| d.underlying.as_str())
    .or_else(|| {
        // Fallback for tests that synthesize a pool whose
        // observed mapper matches but luks_uuid differs;
        // identity decisions already flowed via old_uuid.
        pool.devices
            .iter()
            .find(|d| d.mapper == *mapper)
            .map(|d| d.underlying.as_str())
    }),
```

Three problems:

1.  **Unreachable in production.** `resolve_replace_source`
    (`cli/src/replace.rs:1619-1652`) only emits `ReplaceSource::Live`
    when it just found a `PoolDevice` with `luks_uuid == old_uuid` in
    the same `PoolState` that flows unchanged into `execute`. The
    primary find cannot miss.

2.  **Unreachable in tests too.** The fallback is gated by
    `!params.yes`. Test fixture default is `yes: true`
    (`cli/src/test_fixtures/replace.rs:294`); the only two `yes(false)`
    callsites (`replace.rs:4863`, `replace.rs:4936`) also pass
    `dry_run(true)`, which short-circuits in `cmd_replace`
    (`replace.rs:1455-1459`) before `execute` runs. No current test
    reaches the fallback. The comment misnames its own purpose.

3.  **Decision-024 anti-pattern.** Even though the branch is dead,
    `docs/decisions/024-luks-uuid-identity.md:98` is explicit: "Code
    must not parse mapper names or LUKS labels to decide membership,
    target a member, or correlate live pool state." Future readers
    will see two ways to find a device row and may copy the
    mapper-based path into a real code site.

Separate from the dead fallback, the test pool synthesizer
(`replace_work_plan_test_pool`, `replace.rs:1792-1836`) seeds the
matched live device's `luks_uuid` via `synth_test_uuid(devid)` while
the planner's `old_uuid` is hardcoded to `"99999999-..."` -- i.e. the
synthetic pool encodes a state that `resolve_replace_source` would
reject in production. Leaving that mismatch in place keeps the trap
the fallback was masking; any future test that adds `execute`-path
coverage on the Live arm would silently get `None` for `old_hw`
instead of the disk-hw-info line a real run would produce.

Intended outcome: delete the dead branch; align the test fixture so
synthesized pool state matches the production UUID invariant. No
behavioral change in any code path (production or test).

## Approach

Two-part mechanical cleanup, both in `cli/src/replace.rs`:

### 1. Delete the mapper-keyed fallback

Replace the `.or_else(...)` block at `cli/src/replace.rs:448-456`
with the bare UUID-keyed find:

```rust
ReplaceSource::Live { mapper: _, .. } => pool
    .devices
    .iter()
    .find(|d| d.luks_uuid == old_uuid)
    .map(|d| d.underlying.as_str()),
```

`mapper` becomes unused inside the `Live { .. }` destructure for this
match arm; either rename to `_` or drop the binding entirely (the
arm still needs to discriminate `Live` vs `Missing`).

### 2. Align the test pool synthesizer with production UUID invariant

Thread `old_uuid` into `replace_work_plan_test_pool` so the
synthesized live device's `luks_uuid` matches what
`resolve_replace_source` would have selected on:

-   Update the signature at `replace.rs:1792`:
    ```rust
    fn replace_work_plan_test_pool(
        replace_source: &ReplaceSource,
        old_uuid: &LuksUuid,
        will_clear_last_missing: bool,
        total_devices: u64,
    ) -> PoolState
    ```
-   Replace `luks_uuid: synth_test_uuid(*devid)` at `replace.rs:1810`
    with `luks_uuid: old_uuid.clone()` for the live-disk row.

Call-site updates (each has a borrow-vs-move quirk that needs care to
keep the helper signature `&LuksUuid`):

-   **`replace_work_plan_for_test`** (`replace.rs:1751-1789`). The
    helper is currently invoked at lines 1755-1759 *before* `old_uuid`
    is defined at line 1768. Move the `let old_uuid = LuksUuid::parse(
    "99999999-9999-9999-9999-999999999999").unwrap();` binding above
    the `replace_work_plan_test_pool(...)` call (between today's
    `let config = ...` at 1754 and the pool call at 1755), then pass
    `&old_uuid` into the helper. The existing `old_uuid,` field at
    line 1775 of the `ReplaceWorkPlanInput { ... }` struct literal can
    keep moving `old_uuid` by value -- the borrow from line 1755 ends
    before the struct literal begins, so no `.clone()` is needed
    here.

-   **`ExistingLuks` enroll-keyfile inline test** (`replace.rs:3585-
    3622`). `old_uuid` is already defined at line 3592 *before* the
    `ReplaceWorkPlanInput { ... }` struct literal that starts at line
    3595. The struct literal both moves `old_uuid` at field
    `old_uuid,` (line 3597) and constructs the `pool: replace_work_
    plan_test_pool(...)` (line 3604) within the same literal, so the
    borrow at the pool call has to coexist with the field move. Pass
    `&old_uuid` into the pool call and change line 3597 from
    `old_uuid,` to `old_uuid: old_uuid.clone(),` so the value is still
    owned when the pool call's borrow evaluates. Do *not* lift the
    pool call out of the struct literal -- keep the surgical
    difference to one line.

The non-live filler devices keep `synth_test_uuid(next_devid)` --
they need stable distinct UUIDs and never collide with `old_uuid`.

`synth_test_uuid` is still used for the filler rows, so it stays. No
other consumer touches the live-device UUID seed.

### Critical files to modify

-   `cli/src/replace.rs` -- both edits above; no other file changes.

### Existing helpers reused

-   `resolve_replace_source` (`cli/src/replace.rs:1619`) is the
    invariant being relied on; no change.
-   `synth_test_uuid` (`cli/src/replace.rs:1843`) stays for filler
    rows.
-   `LuksUuid::clone` -- already used throughout this file.

## Out of scope (separate follow-ups)

Three sibling instances of mapper-keyed `PoolDevice` lookup exist in
production code paths. Each involves real logic (not dead code) and
warrants its own verify-issue pass, not a bundle with this cleanup:

-   `cli/src/add.rs:201` -- `classify_braid_disk_fsid` decides
    `BraidLabeledAlreadyInPool` by mapper match instead of `luks_uuid`.
-   `cli/src/replace.rs:1597` -- `check_new_not_in_pool` rejects a
    new disk by reconstructed `mapper_name(&new_name)` instead of
    `new_uuid` (and `replace.rs:1429` already does the UUID check, so
    this is partially redundant).
-   `cli/src/replace.rs:767` -- pre-mutation defense-in-depth guard
    keys off reconstructed mapper instead of `new_uuid`.

The test-only sibling at `cli/src/remove.rs:825`
(`#[cfg(test)]`-gated) is analogous to the test fixture cleaned up
here, but lives in a `remove`-specific synthesizer; same fix shape
applies but is independent.

## Verification

1.  Read the diff: the `.or_else(...)` block is gone, the
    `mapper`/`_mapper` destructure inside `ReplaceSource::Live` is no
    longer used for the find, and the test-pool synthesizer's live
    row uses `old_uuid.clone()`.

2.  `just test-rust` -- all unit tests in `cli/src/replace.rs` must
    pass. The two test sites that call `replace_work_plan_test_pool`
    drive only `.render_steps()`, which never touches the deleted
    code path, so output must be byte-identical to before.

3.  `just test-vm confirm-then-passphrase-on-stdin replace-live-disk
    replace-dead-disk replace-new-in-pool-guard
    replace-cloned-luks-header-rejected recover-replace-not-started
    recover-replace-completed` -- the registered VM checks that
    exercise the real `cmd_replace -> execute` path on real LUKS
    containers. `confirm-then-passphrase-on-stdin` is the one that
    actually drives `!params.yes` on `braid replace` (and `braid
    add`) by piping `"yes\n<passphrase>\n"` to stdin -- this is the
    only test that exercises the confirm block whose fallback is
    being deleted. The other six cover the live and missing replace
    paths, the duplicate-UUID guards, the cloned-header rejection,
    and replace recovery, so production behavior on the surrounding
    flow is also pinned end-to-end.

4.  Spot-check `grep -n 'd.mapper == \*mapper' cli/src/replace.rs`
    returns no matches (the only such pattern in this file was the
    fallback being deleted).

5.  No fixture refresh, no doc updates, no journal/membership schema
    impact. Decision-024 is reinforced by the deletion; no rewording
    needed.
