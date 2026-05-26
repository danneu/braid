# Plan: dissolve the "redundant present-disk probe" finding in `braid status`

## Context

An ultrareview finding (Low / Simplicity) flagged that `build_status` runs
`probe_config_disk` (cryptsetup `isLuks`/`luksDump`/`status` + backing-path
I/O) for **every** membership member, including members already resolved as
live present pool devices, whose probe results are then only used as a
name/by-id fallback in the present-disk naming path
(`cli/src/status.rs:915-936`).

Investigation confirmed the finding is **half right**:

- The probe's **return value** (`ConfigDisk { name, by_id_path, state }`) *is*
  redundant for present disks. `probe_config_disk` copies `name`/`by_id_path`
  straight from its arguments (`cli/src/probe.rs:163-221`), and `build_status`
  passes `&member.name`/`&member.by_id`. DiskNames are unique under the
  four-axis membership invariant, so for any present disk
  `matched_config.name == matched_member.name` and
  `matched_config.by_id_path == matched_member.by_id` **always** -- the
  `matched_config` preference is a provable no-op. The probe `state` is never
  read for present disks (the only reader is the unpooled/missing loop, which
  `continue`s past live-UUID disks).

- The probe's **error path** is **not** redundant. `probe_config_disk` can
  return `Err(MapperConflict | MapperBackingMismatch | MapperBackingResolveError
  | UnsupportedLuksVersion | luksDump-exit-nonzero)` for a present member, and
  `build_status` propagates it via `?` (`cli/src/status.rs:516`). This is the
  **only** non-mutating surface that surfaces a config-side mapper hijack /
  backing mismatch / non-LUKS2 header on a *live* pool member: `doctor` never
  calls `probe_config_disk`, and the TUI already skips live members
  (`cli/src/tui/probe.rs:342`). The behavior is deliberately pinned by
  `status_surfaces_mapper_conflict` (`cli/src/status.rs:5603`, intent comment
  at 5588-5602: it exists specifically to stop a future change from hiding
  probe errors from the non-mutating command boundary).

**Decision (chosen by maintainer): Option A.** Keep probing every member (the
I/O is the accepted price of the live-member fault check, not waste), remove
only the genuinely redundant `matched_config` consumption, and add a doc
comment that explains *why* the full probe sweep is intentional. This removes
the maintenance smell the finding correctly identified, changes no behavior
(byte-identical output, no test changes), and makes the code self-explaining so
the pattern is not re-flagged. We explicitly do **not** reduce cryptsetup I/O,
because that I/O backs a real diagnostic.

Rejected alternatives:
- *Skip live members* (the finding's literal fix): cuts I/O but silently drops
  the live-member fault diagnostic and requires deleting the very test written
  to catch that change. Low-value saving (latency-only, on a human-invoked
  command; the fault is essentially unreachable for a braid-assembled pool).
- *Surgical (preserve detection, thread the pool's known UUID into a slimmed
  mapper-ownership check)*: bifurcates `probe_config_disk` for a partial I/O
  saving -- a net simplicity *loss* on a Simplicity finding.

## Changes

All edits are in `cli/src/status.rs`. No other files change.

### Edit 1 -- drop `matched_config` from the present-disk naming path (~915-936)

In `build_disk_reports`, source present-disk `name`/`by_id` solely from the
UUID-keyed `matched_member`, keeping the existing foreign-device fallback
(`pd.mapper.0` / `/dev/mapper/<mapper>`) verbatim. Remove the `matched_config`
binding and its `.map(...).or_else(...)` preference entirely. Replace the
"keep the member-name fallback ... for partial probes or future callers" doc
comment with one stating the new contract: present-disk identity is the
UUID-keyed membership join (decision 024); `config_disks` is intentionally not
consulted here because for a present member it carries the same name/by-id as
`matched_member`.

Output is byte-identical (proven above). The only present-disk test where
`matched_config` is non-`None` (`disk_report_pairs_stats_by_devid` at 5753,
`build_disk_reports_routes_foreign_mapper_errors_to_doctor` at 4874, and
`build_disk_reports_skips_unpooled_row_when_membership_uuid_live_for_present_not_luks`
at 4703) all build the config disk's `by_id` equal to the membership member's
`by_id`, so the rendered rows are unchanged.

### Edit 2 -- document why every member is probed (~500-516)

Add a comment above the `config_disks` probe loop in `build_status` explaining
that the sweep is intentional: the probe's return value is only consumed for
unpooled/missing disks, but the probe's error path is the sole status-side
surface for config-side mapper/backing/LUKS-version faults on live members,
and that error propagates through `build_status` via `?`. Reference
`status_surfaces_mapper_conflict`, and note that `doctor` does not probe config
disks and the TUI skips live members, so dropping the present-member probe
would silently remove the diagnostic. State plainly that the redundant
cryptsetup I/O on a healthy pool is the accepted cost of that fault check.

The probe loop code itself is unchanged. `config_disks` remains consumed by the
unpooled/missing loop (`~984-1043`), so the parameter and the loop stay as-is.

## What deliberately stays the same

- `build_status` still probes every member (no UUID pre-filter). The I/O is not
  cut.
- The unpooled/missing loop's `membership_uuid_live` skip (~985-990) is
  unchanged -- still a correct defensive invariant exercised by the direct-call
  unit test at 4703.
- `status_surfaces_mapper_conflict` and all `build_disk_reports`/`build_status`
  tests are unchanged.
- `build_compact_drives` / `build_devid_names` (`status.rs:497-498`) do not take
  `config_disks` and are unaffected.

## Verification

The behavior claim is "byte-identical output, no behavior change," so the
primary verification is that the **existing** test suite passes **without any
test edits**:

1. `just test-rust` -- runs the CLI crate (`braid-cli`) unit tests, including:
   - `status_surfaces_mapper_conflict` (5603) -- still probes all members,
     still returns `Err(MapperConflict)`. If this passes, the live-member fault
     contract is intact.
   - `build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`
     (4779) and `..._foreign_config_uuid_...` (4824) -- UUID-keyed identity
     preserved (foreign pool UUID -> `by_uuid` None -> mapper-basename
     fallback).
   - `build_disk_reports_routes_foreign_mapper_errors_to_doctor` (4874),
     `disk_report_pairs_stats_by_devid` (5753),
     `build_disk_reports_skips_unpooled_row_when_membership_uuid_live_for_present_not_luks`
     (4703) -- present-disk rows unchanged after dropping `matched_config`.
2. `cargo clippy --manifest-path cli/Cargo.toml --tests` -- confirm no
   unused-variable/import warning from removing the `matched_config` binding
   (none expected; `config_disks` is still used by the unpooled loop).

No NixOS VM tests are needed: the change is CLI-internal, touches no systemd
unit / mount / pool-lock path, and produces identical output. If a sanity check
is wanted, `braid status` on a healthy pool renders identically to before.

## Critical files

- `cli/src/status.rs` -- both edits (present-disk loop ~915-936; probe-sweep
  doc comment ~500-516). Tests referenced for verification at 4703, 4779, 4824,
  4874, 5603, 5753.
- `cli/src/probe.rs:156-222` -- `probe_config_disk` (reference: confirms
  name/by_id are copied from args; error variants that justify keeping the
  probe).
- `cli/src/tui/probe.rs:342` and `cli/src/doctor.rs` -- reference only
  (establish that no other surface covers the live-member fault).
