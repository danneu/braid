# Plan: fix add live-pool duplicate-UUID error (ADR-024 aligned)

> References use symbols and test-function names, not line numbers: this
> file's `add.rs` line targets drifted +28 lines mid-session (an unrelated
> commit landed), so locate everything by `rg <symbol>` / `rg <test name>`.

## Context

`braid add` refuses a disk whose LUKS UUID collides with a device already
live in the mounted btrfs pool. The refusal is rendered by
`cli/src/add.rs#duplicate_live_pool_uuid_error`, which today synthesizes a
`DiskName` for the colliding live device by parsing its mapper basename:

```rust
let synth_name = DiskName::parse(&live_device.mapper.0)
    .unwrap_or_else(|_| DiskName::parse("foreign")...);
```

`PoolDevice.mapper` is the full mapper basename (`braid-foreign`), and
`DiskName::parse` permits interior hyphens (`cli/src/types.rs#is_valid_disk_name`),
so the parse succeeds as `DiskName("braid-foreign")`. The
`AddError::DuplicateUuid` Display then renders `braid-{name}` ->
**`braid-braid-foreign`** (double prefix). The empty by-id placeholder
`(/dev/disk/by-id/)` is rendered as noise too.

The original finding (Low/Correctness) framed this as a cosmetic
double-prefix and proposed swapping in `config::name_from_mapper`. Deeper
investigation shows the real problem is architectural -- the live-pool arm
should not be naming the colliding device at all:

1. **ADR 024 (LUKS UUID Is Disk Identity)** records that braid "does not
   invent a new member identity for the clone"
   (`docs/design/decisions/024-luks-uuid-identity.md`, Limits/Non-Goals).
   `duplicate_live_pool_uuid_error` does exactly that -- it fabricates a
   `DiskName` for the cloned/foreign live device.
2. ADR 024 (Runtime Handles And Labels #10) names the canonical shape for
   this refusal: `DuplicateUuid { scope: LivePool }`.
3. **`replace.rs` already implements that shape** via
   `DuplicateUuidScope` (`cli/src/replace.rs#DuplicateUuidScope`,
   `ReplaceError::DuplicateUuid`): it renders the collision by `uuid` +
   scope (`membership` / `live_pool`) and names neither party. A doc
   comment records that this replaced a "pre-migration text contract" that
   used to name the disk.

So `add`'s live-pool arm is the lone holdout still synthesizing a name
from a mapper -- both buggy (double prefix) and contrary to ADR 024 and
its own `replace` sibling. The ideal fix converges `add` onto the
established `replace` / ADR-024 model: name only the real add target and
report the colliding side by scope, surfacing nothing derived from the
foreign device's mapper.

## Scope of the bug

Three collision arms feed `AddError::DuplicateUuid`
(`cli/src/add.rs#assert_target_uuid_unique`):

| Arm | Colliding party | Identity source | Status |
| --- | --- | --- | --- |
| 1. in-flight (two add targets) | operator-supplied target | real `(DiskName, ByIdPath)` | correct, keep |
| 2a. membership | existing pool member | real `(name, by_id)` from membership | correct, keep |
| 2b. live-pool | foreign/clone live btrfs device, NOT in membership | **synthesized from mapper** | **buggy + ADR violation** |

Only arm 2b is wrong. Arms 1 and 2a name real, legitimately-resolved
identities; they stay on the symmetric `DuplicateUuid` variant untouched.

Arm 2b fires precisely when the colliding live device's UUID is in the
live btrfs pool but absent from membership -- a foreign/unmanaged device
(manual `btrfs device add`, a membership desync, or the execute-time
race recheck in `recheck_execute_live_pool_targets`). There is no
legitimate `DiskName` to resolve its UUID to (the UUID isn't in
membership, and it equals the add target's own UUID), so naming it by
mapper is the wrong move on every axis.

## Approach (ideal): scope-only refusal, no mapper

Render the refusal by naming the **real add target** and reporting the
colliding side by **scope only**. Surface nothing derived from the
foreign device's mapper. This is the genuinely braid-ideal shape:
consistent with `replace`'s name-nothing-foreign contract (it still names
the add target, where `replace` -- being single-target -- names neither),
faithful to ADR 024 (no invented identity, no leaning on a foreign
handle), and it makes the double-prefix bug class structurally impossible.

### 1. Add a dedicated error variant in `cli/src/add.rs#AddError`

```rust
/// Live-pool UUID collision: an add target's LUKS UUID matches a
/// device already live in the btrfs pool but absent from membership
/// (a foreign/cloned device). Per ADR 024 braid does not invent an
/// identity for the clone, so this names only the real add target and
/// reports the colliding side by scope -- mirroring
/// `ReplaceError::DuplicateUuid { scope: LivePool }`.
#[error(
    "duplicate LUKS UUID {uuid}: add target braid-{name} ({by_id}) \
     collides with a device already in the live pool -- detach the \
     cloned or unintended disk before retrying (this typically \
     indicates a dd-cloned disk)"
)]
DuplicateUuidLivePool {
    uuid: LuksUuid,
    name: DiskName,
    by_id: ByIdPath,
}
```

- Names the **real** add target (`name`, `by_id`) -- legitimate operator
  identity, useful for multi-target `add` runs (which one collided), and
  not invented.
- Reports the colliding side as "a device already in the live pool"
  (scope prose). No `DiskName`, no `MapperName`, no empty by-id.
- Keeps the existing dd-clone guidance tail (richer than `replace`'s
  generic tail, and specific to this refusal's purpose).
- Wording note: `replace` renders the literal token `live_pool` (its
  `DuplicateUuidScope` Display). `add` uses prose ("the live pool") for
  readability; aligning both to one form later is out of scope.

### 2. Rewrite `duplicate_live_pool_uuid_error` and drop the foreign handle

Rewrite `cli/src/add.rs#duplicate_live_pool_uuid_error` to build the new
variant from `(uuid, name, by_id)` alone. Delete the
`DiskName::parse(mapper)` synthesis, the `ByIdPath::parse("/dev/disk/by-id/")`
placeholder, and the `duplicate_uuid_error` (by-id sort) call. Rewrite the
helper's own doc comment (currently "Render a live-pool UUID collision
with the same synthesized live side as `assert_target_uuid_unique` ...") --
there is no synthesized live side anymore. The `live_device: &PoolDevice`
parameter becomes unused -- **drop it**. All 4 callers
(`rg 'duplicate_live_pool_uuid_error('`) used the live device only to
build this error.

Then remove the now-orphaned foreign-device handle from the planner, so
nothing downstream carries it. If we merely stop binding `device` at the
callsites, `LivePoolMatch::DifferentBacking { device }` would be read only
by tests (`dead_code` is warn-level and CI compiles `--tests`, so it
survives silently) -- the planner would still plumb the foreign handle and
two tests would pin a "which device" contract no production code consumes.
Instead, dissolve it:

- Make `cli/src/add.rs#LivePoolMatch::DifferentBacking` a **unit variant**
  and drop the `'a` lifetime from `LivePoolMatch` and
  `cli/src/add.rs#classify_live_pool_match`. All three variants become
  unit, matching the existing `SameBacking`/`NoMatch`
  `assert_eq!(result, LivePoolMatch::...)` test style.
- In `classify_live_pool_match`, change the accumulator
  `let mut different_backing = None;` (`Option<&PoolDevice>`) to a `bool`,
  `different_backing.get_or_insert(device)` to `different_backing = true`,
  and the trailing `if let Some(device) = different_backing` to
  `if different_backing`. The loop still binds `device` for the
  canonicalize-error message and still scans every row, so precedence
  (different-backing wins) and the later-row error path are unchanged.
- The 3 production `DifferentBacking { device }` match arms (in
  `recheck_execute_live_pool_targets` and the two `build_add_work_plan`
  pass loops) become `DifferentBacking`; the `assert_target_uuid_unique`
  arm-2b `find(...)` lookup becomes `any(...)`.

### 3. Keep the symmetric arms; fix the now-stale comments

Keep `AddError::DuplicateUuid` and `cli/src/add.rs#duplicate_uuid_error`
unchanged for arms 1 and 2a (arm 2a continues to name the real member --
legitimate, not the bug; the pre-existing add/replace asymmetry on the
membership arm is out of scope). Then fix the comments that will otherwise
contradict the code (the project forbids comments that lie about behavior):

- `AddError` enum doc comment (the `DuplicateUuid` paragraph): stop
  claiming `DuplicateUuid` covers the live `pool.devices` case / "names
  both `(name, by_id)` pairs" for it; document the new
  `DuplicateUuidLivePool` split and cite ADR 024 + the `replace`
  name-nothing-foreign contract.
- `assert_target_uuid_unique` doc comment, step-2 clause (currently
  "raise `AddError::DuplicateUuid` naming the in-flight target plus a
  synthesized `(name, by_id)` for the colliding existing member
  (membership case) or live device (live-pool case)"): split it -- the
  membership case stays `DuplicateUuid` naming the real member; the
  live-pool case raises `DuplicateUuidLivePool` naming only the add target,
  colliding side by scope.
- Inline `// (2b) Live-pool collision.` comment (currently "we render its
  observed `mapper` via `MapperName::Display` as the 'name' surface, and
  use an empty by-id placeholder"): rewrite to the scope-only refusal (no
  mapper, no placeholder by-id).

### 4. Tests

Update the four arm-2b tests; leave arm-1/2a tests alone. (The two
`classify_live_pool_match` precedence tests are handled in step 2.)

- `execute_live_pool_recheck_rejects_different_backing` (live mapper
  `clone-foreign`): assert `DuplicateUuidLivePool`, body names
  `add target braid-disk2 (...)` + "live pool"; assert the body contains
  **neither** `braid-braid` **nor** the foreign mapper string
  `clone-foreign` (proves we never surface the foreign handle).
- `add_open_present_luks_same_uuid_different_backing_rejects_clone` and
  `add_closed_present_luks_same_uuid_different_backing_rejects_clone`
  (live mapper `braid-foreign`): match `DuplicateUuidLivePool { uuid,
  by_id, .. }`, keep the candidate-by-id assertion against `by_id`.
- `add_pre_write_uniqueness_assert_live_pool_collision` (live mapper
  `braid-foreign`): match `DuplicateUuidLivePool`, add a body assertion --
  names the add target + "live pool", contains no `braid-braid` and no
  `braid-foreign`. **This `braid-foreign` case is the canonical
  double-prefix regression** (only a `braid-`-prefixed mapper can produce
  `braid-braid-...` pre-change).
- **New focused regression test** on `duplicate_live_pool_uuid_error` (or
  via `assert_target_uuid_unique`) using a live mapper `clone-foreign`:
  pins that the rendered message names the add target, says "live pool",
  and contains no token derived from the live device's mapper (no
  `clone-foreign`). Use the three-section `// Intent / Why it exists /
  Scenario` preamble (Test Conventions).

Arm-1/2a tests that MUST stay green unchanged:
`add_cloned_disk_duplicate_uuid_refusal` (name1=diska/name2=diskb) and
`add_pre_write_uniqueness_assert_membership_collision`.

## Reuse / references

- `cli/src/replace.rs#DuplicateUuidScope` and
  `ReplaceError::DuplicateUuid` -- the established model being mirrored.
- `docs/design/decisions/024-luks-uuid-identity.md` -- authority for "do
  not invent identity for a clone," the `scope: LivePool` shape, and the
  UUID-is-identity / mapper-is-not-identity rules.
- `AddError` reaches the CLI boundary via Display/`?` propagation; no
  `match` on `AddError` variants exists in `main.rs`/`cmd.rs`. Even if one
  appeared, a non-exhaustive match would fail to compile (not silently
  misbehave), so the new variant needs only its `#[error(...)]` attribute.
- `cli/src/config.rs#name_from_mapper` -- not needed by this design (we
  render no mapper-derived name); noted only to record it was the
  finding's proposed helper.

## Verification

1. `just test-rust` -- exercises the new variant, the rewritten arm-2b
   tests, the two `classify_live_pool_match` test updates, and the new
   regression test. The CLI crate package is `braid-cli`; prefer
   `just test-rust`.
2. TDD check (two complementary regressions):
   - `add_pre_write_uniqueness_assert_live_pool_collision` (mapper
     `braid-foreign`) fails pre-change because the old message double-prefixes
     to `braid-braid-foreign`; passes after.
   - the new `clone-foreign` helper test fails pre-change because the old
     message *names the foreign side* (`braid-clone-foreign`) and lacks the
     "live pool" scope wording -- not because of a double prefix (a
     non-`braid-` mapper never produces `braid-braid-...`); passes after.
3. Pure error-rendering / planner-internal change -- no fixture/VM/parser
   surface touched, so no `capture-*` / `test-parsers` obligation. A
   targeted `just test-rust` is sufficient. (The cloned-header VM test
   `tests/cli/braid-add-cloned-luks-header-rejected.py` exercises the
   mapper-backing-mismatch guard, not this duplicate-UUID arm, so it is
   unaffected.)

## Alternatives considered (not chosen)

- **Scope + mapper diagnostic.** Same variant but also carry
  `live_mapper: MapperName` and render `... a live pool device (mapper
  {live_mapper})`. Gives the operator a handle to the foreign device, but
  reintroduces leaning on the least-trustworthy mapper braid has,
  diverges from `replace`'s deliberate name-nothing contract, and the
  remedy (detach the named add target) does not require it. One extra
  field if ever wanted.
- **Minimal local patch.** Keep synthesizing a `DiskName` but strip the
  prefix correctly (`name_from_mapper(&m).unwrap_or(&m)` then parse,
  placeholder on failure). Fixes the visible double-prefix and breaks zero
  tests, but still fabricates identity from the mapper (ADR-024 tension),
  keeps the empty `(/dev/disk/by-id/)`, and still renders a misleading
  `braid-clone-foreign` for non-braid mappers. Leaves the root violation
  in place.

## Implementation notes

- `LivePoolMatch` doc treatment: with all three variants now unit, the old
  per-variant note on `SameBacking` ("Unit because callers only need the
  no-op signal") no longer distinguishes anything, so it was folded into the
  enum-level doc ("All variants are unit ... no foreign `PoolDevice` handle is
  carried") and `DifferentBacking` got a one-line doc recording it is refused
  by scope per ADR 024. The plan specified the variant shape, not the doc
  wording.
- Closed-clone test (`add_closed_present_luks_same_uuid_different_backing_rejects_clone`)
  gained a `by_id` assertion. The plan said "keep the candidate-by-id assertion
  against `by_id`" for both open and closed tests, but only the open test had
  one; the closed test previously asserted UUID only, so the assertion was
  added (not kept) to mirror the open test against the new single-`by_id`
  variant.
- `assert_target_uuid_unique`'s trailing doc paragraph ("names both by-id paths
  explicitly") was also softened to "naming the real parties, or the add target
  plus scope for the live-pool arm". Step 3 named only the step-2 clause, but
  the trailing sentence would otherwise contradict the live-pool arm's new
  scope-only refusal.
