# Plan: name-order present-device rows in `braid status`

## Context

Decision 024 (`docs/design/decisions/024-luks-uuid-identity.md:222-223`) states:

> `pool.json` key order is UUID order, not disk-name order. Display surfaces
> that need stable operator ordering must sort by `DiskName`.

`braid status` violates this for **present** pool devices. Two display
surfaces emit present rows by iterating `&pool.devices` directly:

- compact `Drives:` block -- `build_compact_drives` (`cli/src/status.rs:245`)
- verbose `Disks:` block -- `build_disk_reports` (`cli/src/status.rs:946`)

`pool.devices` is built from parsed `btrfs filesystem show` output
(`cli/src/probe.rs:419-477`), which btrfs emits in **devid order**
(`list_sort(NULL, all_devices, cmp_device_id)`,
`reference/btrfs-progs/cmds/filesystem.c:314`). The **missing/unpooled** half
of each surface is already name-sorted (compact via `membership.iter_by_name()`
at `status.rs:266`; verbose via `config_disks`, itself built from
`iter_by_name()` at `status.rs:510-514`). So the two halves diverge: present
rows are devid-ordered, missing rows are name-ordered.

Impact: after a remove+add (devids no longer monotonic with names), or with
names like `toshiba2`/`toshiba10` whose lexical order differs from devid order,
the operator-facing listing is inconsistent between its present and missing
halves and unstable across pool history. The TUI -- the other display surface
-- already renders name-ordered via `DiskIdentity.names` from `iter_by_name()`
(`cli/src/tui/model.rs:69-75`), so CLI `status` is the lone outlier.

**Root cause beyond the symptom:** the present-device display-name rule
("join membership by LUKS UUID -> `member.name`, else fall back to mapper
basename") is duplicated verbatim in **three** places, each deciding ordering
independently:

- `build_compact_drives` (`status.rs:246-250`)
- `build_disk_reports` (`status.rs:953-960`)
- `build_devid_names` (`status.rs:295-301`)

The fix is therefore not two inline sorts but a small unification: extract the
shared resolver, then sort the two display surfaces through it. This dissolves
the duplication that let the orderings drift apart and prevents a future fourth
surface from re-diverging.

Intended outcome: present rows are name-ordered, consistent with the
missing/unpooled half and with the TUI, satisfying decision 024; the
display-name rule lives in exactly one function.

## Scope / approach

Single file: `cli/src/status.rs`. No behavior change to parsing, probing,
membership, or the missing-half ordering. No fixture refresh (no parser/tool
change).

### 1. Extract the shared present-device name resolver

Add one helper that captures the decision-024 join:

```rust
/// Single source of the decision-024 present-device display-name rule:
/// UUID-join membership to the operator name, falling back to the raw mapper
/// basename for foreign live devices. Shared so the compact summary, verbose
/// reports, and devid->name map cannot diverge on naming or row ordering.
fn present_display_name(member: Option<&DiskMember>, mapper: &MapperName) -> String {
    member
        .map(|m| m.name.as_str().to_owned())
        .unwrap_or_else(|| mapper.0.clone())
}
```

Signature takes `Option<&DiskMember>` + `&MapperName` (not `pd` + `membership`)
so `build_disk_reports`, which already binds `matched_member` for its `by_id`
fallback, reuses that single `by_uuid` lookup instead of looking up twice.

Replace all three inline copies with calls to this helper:

- `build_compact_drives`: `let member = membership.by_uuid(&pd.luks_uuid);`
  then `present_display_name(member, &pd.mapper)`.
- `build_devid_names`: same pattern (it keeps building a `devid -> name` map;
  no ordering change there, it just stops re-implementing the rule).
- `build_disk_reports`: pass its existing `matched_member` and `&pd.mapper`;
  the adjacent `by_id` fallback continues to branch on the same
  `matched_member`.

### 2. Sort present rows by resolved name in both display surfaces

**`build_compact_drives`** -- before the present-push loop, collect present
devices paired with their resolved name and sort lexically, then push:

```rust
let mut present: Vec<(&PoolDevice, String)> = pool
    .devices
    .iter()
    .map(|pd| (pd, present_display_name(membership.by_uuid(&pd.luks_uuid), &pd.mapper)))
    .collect();
present.sort_by(|(_, a), (_, b)| a.cmp(b));
// then iterate `present`, building each CompactDrive from `pd` + `name`
```

The unpooled/missing loop (`iter_by_name()`, `status.rs:266`) stays exactly as
is and is still appended after, preserving the present-then-missing grouping.

**`build_disk_reports`** -- sort the present devices the same way *before* the
loop that pushes to both `disk_reports` and `human_details`, so the two Vecs
stay in lockstep. The unpooled `config_disks` loop is unchanged (already
name-ordered via `status.rs:510-514`).

### Ordering contract (forced, not optional)

- **Lexical** comparison on the resolved name string, identical to
  `iter_by_name`'s `left.name.cmp(&right.name)` (`membership.rs:314`) and
  `DiskName`'s derived `Ord` (`types.rs:105`). Do **not** introduce natural
  sort -- it would desync the present half from the name-sorted missing half.
- **Grouped, not globally interleaved:** each surface emits `[present sorted
  by name]` then `[missing/unpooled sorted by name]`. This matches the current
  structure and keeps present disks visually grouped (diagnostic value).
  Decision 024 only requires sorting by `DiskName`, which grouped-by-status
  satisfies.

## Non-goals

- TUI ordering -- already name-ordered; not touched.
- Missing/unpooled half ordering -- already correct; not touched.
- `build_devid_names` map output -- order-independent; it only adopts the
  shared resolver, no behavioral change.
- No global present+missing interleave; no natural/numeric sort.

## Tests

Add two Rust unit tests in the `status.rs` test module, each using a pool whose
**devid order is the reverse of name order** so the sort is observable (every
existing fixture coincidentally has devid order == name order, which is why the
bug is currently untested). Reuse existing helpers `test_uuid(..)`,
`disk_member_with(..)`, `membership_from(..)` (see `status.rs:5407-5408`); for
the verbose test mirror the runner/`device_stats` harness in
`build_disk_reports_routes_foreign_mapper_errors_to_doctor` (`status.rs:5030`).

Fixture shape for both: three members. Two are present, e.g. UUID-A -> name
`bravo` and UUID-B -> name `alpha`; `pool.devices` is in devid order
`[devid 1 = bravo, devid 2 = alpha]`. The third member is not present in
`pool.devices` and has a name that sorts before both present names, e.g.
UUID-C -> name `aardvark`.

Expected after fix: grouped order, not global order -- row 0 = present
`alpha`, row 1 = present `bravo`, row 2 = missing/unpooled `aardvark`. This
pins both halves of the contract: present rows sort by name, and the
present-then-missing grouping is preserved even when a missing name would sort
first globally.

1. `build_compact_drives_sorts_present_rows_by_name_not_devid`
   - Intent: present compact rows are ordered by resolved `DiskName`, not by
     btrfs devid order.
   - Why it exists: decision 024 requires name ordering; present rows came
     straight off devid-ordered `pool.devices`, diverging from the
     name-sorted missing half.
   - Scenario: a pool whose devids run opposite to disk names (post
     remove+add, or `toshiba2`/`toshiba10`); operator expects `alpha` before
     `bravo`.
   - Assert: `drives[0].name == "alpha"`, `drives[1].name == "bravo"`,
     `drives[2].name == "aardvark"`, with statuses present, present, missing.

2. `build_disk_reports_sorts_present_rows_by_name_not_devid`
   - Same Intent/Why/Scenario at the verbose `Disks:` surface.
   - Assert ordering on `ctx.disks`: `alpha`, `bravo`, then `aardvark`, with
     statuses present, present, missing/unpooled. Assert `ctx.human_details`
     matches the same order and statuses, so the two Vecs stay in sync.

These are behavioral and structure-insensitive: they assert the operator-visible
row order, not the existence of the helper.

## Verification

- `just test-rust` -- runs `cargo test` for `braid-cli`; the two new tests are
  the real regression guard. Confirm they **fail before** the sort change and
  pass after (write tests first per the repo's TDD norm).
- No fixture refresh and no VM tests required: this is pure in-process
  formatting logic with no parser or tool-version surface. A manual `braid
  status` would only show the difference on a multi-disk pool with
  non-monotonic devids, so it is not a practical check -- the unit tests cover
  it.

## Files

- `cli/src/status.rs` -- add `present_display_name`; route the three sites
  through it; sort present rows in `build_compact_drives` and
  `build_disk_reports`; add two unit tests.

Reference only (not edited): `cli/src/membership.rs:312` (`iter_by_name`
comparison to match), `cli/src/types.rs:105` (`DiskName` `Ord`),
`docs/design/decisions/024-luks-uuid-identity.md:222-223` (the contract).
The decision doc already states this ordering rule, so no doc change is needed.
