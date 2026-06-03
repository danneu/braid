# Fix the compact `Drives:` missing-row example in status.md

## Context

`docs/commands/status.md` documents the human-readable `Drives:` compact
listing. Its only missing-disk example renders the devid column as `-`:

```
  toshiba3     -    -        missing
```

That is stale. Commit `4f1701e1` ("fix(status): name missing devices by
live-confirmed devid", 2026-05-13) changed `build_compact_drives` so a missing
member whose devid the live filesystem confirms missing renders `devid=N` --
but it did not update this doc. The compact `Drives:` block is mounted-only, so
a "missing" row almost always corresponds to a btrfs-MISSING device (`devid N
... path MISSING`), which carries an authoritative devid. The common output is
therefore `devid=3`, not `-`.

The `-` form is the rare fallback: a persisted devid the live filesystem no
longer reports missing, or a member with no recorded devid. So the doc's only
example shows the uncommon case and hides the operationally load-bearing value
-- the devid an operator opens `braid status` to read (the section's own "When
to use it" says "To find device IDs needed by other commands (`--missing-id`)").

Intended outcome: the example shows the representative `devid=N` form, and a
short prose note explains the columns, the `-` fallback, and the hot-unplug
caveat (a displayed devid is not always directly usable by `remove-missing` /
`replace`) -- matching this file's "one canonical example + prose for
deviations" house style (e.g. the JSON `disks[]` element + `>`-note convention).
The section currently has zero prose; this also closes that gap.

This is documentation-only. The code is correct and already pinned by tests
(`build_compact_drives_missing_member_shows_devid_when_live_confirmed`,
`..._hides_stale_persisted_devid`, and the end-to-end
`build_status_missing_device_banner_and_compact_row_name_member_end_to_end`,
which asserts the row `contains("devid=3")`). No code or test changes.

## Approach (Option A: single representative row + prose note)

Edit only the **"Drives (compact listing)"** section of
`docs/commands/status.md` (currently the bare example block at lines ~112-119).

1. **Change the missing row** in the example from
   `toshiba3     -    -        missing` to
   `toshiba3     -    devid=3  missing`.
   (Leave the device column `-` -- an unpooled member always shows `-` for its
   device, confirmed in `cli/src/status.rs#build_compact_drives`.)

2. **Add a concise prose note** after the example block. Content, in the file's
   terse ASCII style (`--`, not em-dash):

   - Each row is the disk `name`, its short kernel device, its btrfs `devid`,
     and state.
   - A disk not assembled into the live pool (missing, offline, or
     LUKS-mismatched) shows `-` for its device.
   - The `devid` column shows `devid=N` when the live pool currently counts
     that device missing -- a btrfs-MISSING device, or a hot-unplugged member
     whose backing device is gone (null-underlying). It falls back to `-` when
     no live devid exists: a persisted devid the live pool no longer counts
     missing, or a member with no recorded devid.
   - That devid is the input to the `braid remove-missing --missing-id` /
     `braid replace` workflows -- but mirror the caveat the JSON `missing_devids`
     bullet already states in this same file: a transient hot-unplug devid shown
     here is refused by both `remove-missing` and `replace` until btrfs promotes
     it to MISSING. Cross-link
     [Hot-unplug while pool is mounted](../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted).
   - State values use the same vocabulary as the
     [Per-disk detail](#per-disk-detail) section below, rendered lowercase and
     hyphenated in this compact list (e.g. `missing`, `offline`,
     `luks-uuid-mismatch`).

   Phrasing notes for the implementer:
   - The hot-unplug caveat covers BOTH commands. Per `recovery-scenarios.md`
     ("Hot-unplug while pool is mounted"), `remove-missing --missing-id N` and
     `replace` (with or without `--missing-id`) both refuse a null-underlying
     devid until btrfs promotes it to MISSING. Do not imply only `remove-missing`
     is affected, and do not describe the displayed devid as universally usable.
   - Keep the note consistent with the JSON `missing_devids` bullet in this same
     file, which already states the null-underlying rejection -- the compact
     note must not contradict or undercut it.
   - The required-vs-optional `--missing-id` detail (required by
     `remove-missing`, optional for `replace`) lives in those command pages; the
     compact note only needs to name the workflow and the caveat, not re-litigate
     the flag contract.
   - Use code spans for `braid remove-missing` / `braid replace`; they are
     already linked in the Related commands section, so no inline links needed.
   - Two link anchors are mdbook-linkcheck2-validated -- keep both exact:
     `#per-disk-detail` (the `### Per-disk detail` heading in this file, home of
     the "Disk states (compact `Drives:` list and detail view)" table) and
     `../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted` (the
     `### Hot-unplug while pool is mounted` heading).

### Optional micro-fix (implementer's discretion, can skip)

The "Disk states" table (status.md ~194-203) shows alarming states in caps
(`MISSING`, `OFFLINE`, `LUKS UUID MISMATCH`) though the compact column renders
them lowercase/hyphenated. The new note's "rendered lowercase and hyphenated in
this compact list" clause already resolves this for the reader; rewriting the shared
glossary table is out of scope and not recommended (its bolded terms read as
conceptual labels, and the detail view does use the caps forms).

## Critical files

- `docs/commands/status.md` -- the only file changed; only the "Drives (compact
  listing)" section.

Reference only (no changes; cited for accuracy):
- `cli/src/status.rs#build_compact_drives` -- column rules (device `-` for
  unpooled; `devid = member.devid.filter(|d| alert_devids.contains(d))`).
- `cli/src/types.rs#PoolState::alert_missing_devids` -- the live missing set the
  compact column filters against (btrfs-MISSING devids unioned with
  null-underlying devids). Method-qualified because `cli/src/probe.rs` has a
  same-named method on a different type (`AlertPoolState`) used by the alert
  pipeline, not by the compact column.
- `cli/src/remove_missing.rs#validate_missing_id_target` -- the null-underlying
  refusal branch the hot-unplug caveat documents; pinned by
  `validate_missing_id_target_null_underlying_only_rejected`.
- `docs/guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted` --
  cross-link target; states `remove-missing` and `replace` refuse a hot-unplug
  devid "with or without `--missing-id`".
- `docs/commands/status.md` JSON `missing_devids` bullet -- the existing
  null-underlying-rejection caveat the new prose mirrors.
- `docs/commands/remove-missing.md`, `docs/commands/replace.md` -- the
  `--missing-id` required-vs-optional contract (referenced, not restated).

## Verification

- `mdbook build docs` -- must pass; this exercises `mdbook-linkcheck2` and
  confirms BOTH new link anchors resolve -- `#per-disk-detail` in this file and
  `../guides/recovery-scenarios.md#hot-unplug-while-pool-is-mounted` (a bad
  anchor fails CI).
- Visual read of the rendered "Drives (compact listing)" section: example's
  missing row shows `devid=3`; note reads cleanly and matches surrounding terse
  style.
- No Rust tests to run -- behavior is unchanged and already pinned by the three
  tests named in Context. Optionally re-read those tests to confirm the
  documented `devid=N` form is the asserted one.
