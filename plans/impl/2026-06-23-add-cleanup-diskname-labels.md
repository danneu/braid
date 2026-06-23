# Pivot: source `add`'s cleanup-row label from the operator `DiskName`

## Context

`add`'s rollback cleanup (`LuksCleanupGuard`) is the **only** close-trailer in
the codebase that derives its user-facing disk label from a mapper basename:

```rust
// cli/src/add.rs:449-453
let label = mapper
    .as_str()
    .strip_prefix("braid-")
    .unwrap_or(mapper.as_str());
```

ADR 024 forbids this categorically: *"Every user-facing disk row is labeled with
an operator `DiskName`, never a mapper basename"*
([024-luks-uuid-identity.md](../../docs/design/decisions/024-luks-uuid-identity.md),
lines 99-100). The sibling commands (`replace`/`recover`/`remove`) honor the rule
by routing their post-commit close through the shared
`close_mapper_best_effort(disk_label: &DiskName)`
([`cli/src/mapper_close.rs#close_mapper_best_effort`](../../cli/src/mapper_close.rs)),
whose doc says: *"`disk_label` is the journaled operator name, never derived from
a mapper basename, so mapper drift cannot leak into user-facing disk status
rows."* They each carry a drift test that closes `braid-WRONG` while asserting the
row reads `disk disk2`.

`add` has no such test, and the strip-prefix derivation is the reason it *can't*
have a meaningful one: the guard stores only `Vec<MapperName>` and has no
`DiskName` to label from. Today this is **benign** -- `add` only ever tracks
mappers it opened this invocation, constructed canonically as `braid-<name>` via
`mapper_name(name)` ([`cli/src/config.rs#mapper_name`](../../cli/src/config.rs)),
so `strip_prefix("braid-")` always recovers exactly the disk name. But it is a
latent ADR-024 violation and an inconsistency with three sibling commands, and it
defeats the regression test the originating finding asked for.

**This is a pivot, not the finding's literal fix.** The finding ("add a drift
test") cannot be satisfied alone: a test that tracks `braid-WRONG` and asserts
`disk disk2` would *fail* against today's code (it would emit `disk WRONG`). The
test only becomes writable once the label is sourced from a typed `DiskName`. So
the work is: fix the provenance, *then* add the test.

**Intended outcome:** `add`'s cleanup rows are labeled from the operator
`DiskName`, the `strip_prefix("braid-")` is gone, `add` is consistent with its
siblings and ADR 024, and a drift test pins the behavior so a regression to
mapper-derived labels fails loudly.

## Approach

The guard already shares the close *mechanics* with the siblings -- its Drop impl
calls the same `close_mapper_with_retry` helper (`add.rs:459`). The **only** point
of divergence is label sourcing. So the elegant fix is surgical: thread the
operator `DiskName` through `track()` and label the row from it. Keep the guard.

Do **not** collapse `LuksCleanupGuard` into `close_mapper_best_effort`: the guard
is a different abstraction (RAII, multi-mapper, reverse-order close on unwind) and
deliberately uses distinct `(cleanup)` wording to signal *rollback* rather than
the siblings' normal post-commit close. Reuse would erase both.

## Code changes -- `cli/src/add.rs`

### 1. Carry the `DiskName` with each tracked mapper (lines 418-435)

Replace the bare `Vec<MapperName>` with a small named struct so the provenance
intent is explicit and the two values cannot index-drift:

```rust
/// A mapper this invocation of `cmd_add` opened, paired with the operator
/// `DiskName` that labels its cleanup progress row. Per ADR 024 the row label
/// is the typed `DiskName`, never the mapper basename; the mapper stays the
/// close target.
struct TrackedMapper {
    name: DiskName,
    mapper: MapperName,
}

struct LuksCleanupGuard<'a, R: CommandRunner> {
    runner: &'a R,
    mappers: Vec<TrackedMapper>,
    armed: bool,
}
```

Change `track` to take both:

```rust
fn track(&mut self, name: DiskName, mapper: MapperName) {
    self.mappers.push(TrackedMapper { name, mapper });
}
```

### 2. Label from the `DiskName`, delete the strip-prefix (Drop impl, lines 442-476)

Remove lines 450-453 entirely. Label each row from `tracked.name` (which already
implements `Display`; `add.rs` uses `format!("disk {name}: ...")` throughout) and
keep `&tracked.mapper` as the close target:

```rust
for tracked in self.mappers.iter().rev() {
    let name = &tracked.name;
    emit_status(&status_line(
        StatusTag::Wait, color_enabled,
        &format!("disk {name}: locking (cleanup)..."),
    ));
    match close_mapper_with_retry(self.runner, &sleeper, &tracked.mapper, color_enabled) {
        Ok(()) => emit_status(&status_line(
            StatusTag::Ok, color_enabled,
            &format!("disk {name}: locked (cleanup)"),
        )),
        Err(e) => emit_status(&status_line(
            StatusTag::Warn, color_enabled,
            &format!("disk {name}: lock failed (cleanup, {e})"),
        )),
    }
}
```

The `(cleanup)` wording is unchanged. The busy-retry diagnostic emitted *inside*
`close_mapper_with_retry` (`cryptsetup close <mapper> busy, retrying...`) still
echoes the raw mapper -- that is the sanctioned ADR-024 carve-out (it is a command
echo, not a disk-status row) and is untouched.

### 3. Pass the `DiskName` at the two production call sites

Both sites already have the operator name in scope (confirmed by exploration):

- `add.rs:1178` (`AddPlan::execute`, `ClosedPresentLuks` pass):
  `luks_guard.track(target.name.clone(), target.mapper_name.clone());`
- `add.rs:1395` (`AddPlan::execute`, `Fresh` pass, where `let name = &target.name`):
  `luks_guard.track(name.clone(), target.mapper_name.clone());`

## Test changes -- `cli/src/add.rs`

### 4. Mechanically update the 6 existing guard tests

Each `guard.track(MapperName::from_basename("braid-X".into()))` becomes
`guard.track(DiskName::parse("X").unwrap(), MapperName::from_basename("braid-X".into()))`,
keeping `X` matched to the mapper basename so **every existing assertion stays
identical**:

| Test fn (`add.rs`) | track args (mapper basename -> disk name) |
|---|---|
| `guard_closes_on_armed_drop` (4149) | `braid-aaa`/`aaa`, `braid-bbb`/`bbb` |
| `guard_close_failure_emits_cleanup_warn_row` (4183) | `braid-aaa`/`aaa` |
| `guard_retries_busy_close_before_success` (4212) | `braid-aaa`/`aaa` |
| `guard_noop_when_disarmed` (4263) | `braid-aaa`/`aaa` |
| `preexisting_mapper_not_closed` (4284) | `braid-new`/`new` |
| `already_owned_open_outcome_is_not_tracked_by_guard` (4306) | `braid-existing`/`existing` |

(Use the existing `disk("...")` test helper where already in scope, else
`DiskName::parse("...").unwrap()`.)

### 5. Add the drift regression test (the finding's ask, now meaningful)

New test mirroring the siblings' close-trailer drift tests
(`replace.rs#live_replace_old_close_labels_drifted_mapper_with_disk_name`):

The Intent/Why/Scenario preamble goes **directly above `#[test]`** as a
contiguous `//` block, per [testing.md](../../docs/dev/testing.md) ("Every test's
preamble is a contiguous block of `//` line comments directly above the test
item") and matching the sibling drift tests in `replace.rs`/`recover.rs`:

```rust
// Intent: cleanup [wait]/[ok] rows label the disk by its operator
//   DiskName, never the tracked mapper's basename.
// Why it exists: ADR 024 forbids deriving user-facing disk labels from a
//   mapper basename; a regression to strip_prefix("braid-") would silently
//   re-introduce mapper-derived labels and otherwise slip through.
// Scenario: add tracked a mapper opened under a drifted basename
//   (braid-WRONG) for the disk the operator named `disk2`; the guard fires
//   on unwind and the cleanup rows must say `disk disk2`, not `WRONG`.
#[test]
fn guard_cleanup_row_uses_disk_name_under_mapper_drift() {
    let runner = SpyRunner::new(MockRunner::default());
    let captured = crate::status_tag::testing::capture_with_color(false, || {
        let mut guard = LuksCleanupGuard::new(&runner);
        guard.track(
            DiskName::parse("disk2").unwrap(),
            MapperName::from_basename("braid-WRONG".into()),
        );
    });
    assert!(captured.contains("[wait] disk disk2: locking (cleanup)..."), "{captured:?}");
    assert!(captured.contains("[ok]   disk disk2: locked (cleanup)"), "{captured:?}");
    assert!(!captured.contains("WRONG"), "cleanup row must not echo drifted mapper basename: {captured:?}");
    // Close target stays the observed mapper, mirroring the sibling commands.
    let closed = runner.closed.lock().unwrap();
    assert_eq!(*closed, vec!["braid-WRONG"]);
}
```

## Doc changes -- `docs/design/decisions/024-luks-uuid-identity.md`

The change brings `add` into compliance with an existing invariant, so the ADR
must record it (AGENTS.md: any change to behavior/invariant updates the
decisions).

1. **Label-provenance rule, route #2 (lines ~107-123).** Extend the "typed
   `DiskName` carried through the operation" route to name `add`'s pre-commit
   rollback cleanup alongside the post-commit closes: `LuksCleanupGuard` carries a
   typed `DiskName` per tracked mapper and labels its `disk <name>: ... (cleanup)`
   rows from it, never the mapper basename.

2. **"Tests That Enforce This" (lines 275-286).** Add `add`'s cleanup guard to the
   close-trailer pin: cite
   `cli/src/add.rs#guard_cleanup_row_uses_disk_name_under_mapper_drift` as pinning
   that the cleanup row names the disk `disk2` while closing the tracked
   `braid-WRONG` mapper. The existing busy-retry carve-out citation
   (`cli/src/add.rs#guard_retries_busy_close_before_success`) is unchanged. Use the
   `path#symbol` citation form already used in that section.

## Out of scope / decided

- **Keep `LuksCleanupGuard`; do not reuse `close_mapper_best_effort`.** Different
  semantics (RAII, multi-mapper, reverse-order) and intentional `(cleanup)`
  wording. Close mechanics are already shared via `close_mapper_with_retry`.
- **`principles.md` Principle 13** (announce long-running work) references
  `close_mapper_best_effort` only as a `[warn]`-row example; it is not about label
  provenance and stays unchanged.
- **No NixOS VM test.** The drift scenario is unconstructible in real `add` (the
  mapper is always canonical), so it is correctly a unit-level pin.

## Verification

1. `just test-rust` -- the 6 updated guard tests must still pass with identical
   assertions, and `guard_cleanup_row_uses_disk_name_under_mapper_drift` must pass.
2. Sanity-confirm the new test *fails* against the pre-change Drop impl (it would
   emit `disk WRONG`), proving it actually pins provenance -- do this by
   temporarily reverting only the Drop label before the final commit, then
   restoring.
3. `just check-output-ascii` -- new echo strings are ASCII (they are). Run via
   the recipe, not the script directly: `scripts/docs/check-output-ascii.py` is
   not executable in this repo, and the recipe invokes it through `python3` (with
   a `--selftest` pass first).
4. `just docs-build` -- validates the ADR 024 markdown links and cross-links.
   Note: `mdbook-linkcheck2` validates markdown links only; it does **not** check
   the `path#symbol` code-span citations in the ADR body.
5. `rg 'fn guard_cleanup_row_uses_disk_name_under_mapper_drift' cli/src/add.rs`
   and `rg 'guard_cleanup_row_uses_disk_name_under_mapper_drift'
   docs/design/decisions/024-luks-uuid-identity.md` -- confirm the cited test
   symbol exists in code and is referenced in ADR 024, since the code-span
   citation is not auto-validated by `just docs-build`.
