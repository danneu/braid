# Plan: sync `docs/commands/doctor.md` with the actual doctor check set

## Context

`braid doctor` registers 16 checks unconditionally in `run_doctor`
(`cli/src/doctor.rs:1452-1469`), but the operator-facing "What it checks" table
in `docs/commands/doctor.md:60-75` lists only 14. Two always-registered checks
are missing, and one is under-described:

- **`foreign_luks_uuid`** (human label "foreign uuids") -- omitted. This is the
  *only* missing check that can return **`Fail`** and drive the whole command to
  exit 1 (`overall_status`, `doctor.rs:1422`). A script or operator reading the
  table has no way to learn this hard-failure source exists.
- **`system_profile_mismatch`** (human label "system profiles") -- omitted.
  Warn-ceiling, parallel to the documented `data_profile_mismatch` /
  `metadata_profile_mismatch` rows.
- **`declared_disks`** -- the row describes only the all-healthy requirement. The
  check actually **Warns** on missing / not-a-block-device / unreadable header /
  damaged header / probe failure, and **Fails** on a live-vs-`pool.json` LUKS
  UUID mismatch (`summarize_declared_disks`, `doctor.rs:508-512`).

Git history confirms genuine drift, not a not-yet-caught-up gap: `foreign_luks_uuid`
landed in `9229f2a` and `system_profile_mismatch` in `1ab79c3`, both without
touching the doc; the doc was later edited in `57d1d78` (adding the `enospc_risk`
row) and still skipped both. `AGENTS.md` requires the user-facing surface to stay
in sync with behavior.

Intended outcome: the table covers all 16 checks in CLI-output order with accurate
severity outcomes, and the "under the hood" narrative no longer omits the
foreign-UUID reconciliation.

## Scope

Single file: `docs/commands/doctor.md`. No code, test, or sibling-doc changes
(verified: README / `status.md` / `monitor.md` do not enumerate checks; design doc
`017-runtime-disk-membership.md:119` already references the `foreign_luks_uuid`
check as a helper-inventory note and needs no edit).

## Edits

### 1. Add `foreign_luks_uuid` row

Insert immediately **after** the `enospc_risk` row (`doctor.md:67`) and **before**
`data_profile_mismatch` -- this matches the registration and CLI-output order, so
the table reads top-to-bottom like real output.

Proposed cell (Check column = JSON `name`, as every other row uses):

> `foreign_luks_uuid` | **Fail** when the live (mounted) pool contains a btrfs
> device whose LUKS UUID is not declared in `pool.json` (a foreign disk). The
> message names each foreign UUID and its mapper and suggests
> `btrfs device remove /dev/mapper/<mapper> <mount>` then
> `cryptsetup close <mapper>`. Skipped when the pool is not mounted.

Rationale: severity (`Fail`) is the materially important fact for scripts keying on
exit 1. Embedding the remediation command mirrors the existing `smart_self_test`
row's precedent and is compliant with the messaging invariant (it references no
local LUKS-header files). Wording verified against `check_foreign_luks_uuid`
(`doctor.rs:861-910`) and `membership::foreign_luks_uuids` semantics
(`membership.rs:676-684`: returns live-pool UUIDs absent from membership).

### 2. Add `system_profile_mismatch` row

Insert immediately **after** the `metadata_profile_mismatch` row (`doctor.md:69`)
and **before** `metadata_enospc_pressure` -- again matching output order. Keep the
cell terse and parallel to the two sibling profile rows:

> `system_profile_mismatch` | System block groups all use the same RAID profile

Rationale: Warn-ceiling check delegating to the same `check_profile_mismatch`
helper as data/metadata (`doctor.rs:927-931`, `673-744`). Matching the sibling
rows' phrasing keeps the three profile rows visually consistent.

### 3. Expand the `declared_disks` row

Replace the existing cell (`doctor.md:65`) to name the Warn/Fail outcomes:

> `declared_disks` | Every UUID-keyed `pool.json` member is present, is a block
> device, has a readable LUKS header, and its live LUKS UUID matches the
> `pool.json` key. **Warn** if a member is missing, is not a block device, or has
> an unreadable or damaged LUKS header (or a probe failure); **Fail** if a
> member's live LUKS UUID does not match its `pool.json` key.

Rationale: outcomes verified in `summarize_declared_disks` (`doctor.rs:402-513`).
Do **not** inline the per-state remediation strings
(`luks_uuid_mismatch_guidance`, `luks_header_unreadable_guidance`,
`luks_header_damaged_guidance`) -- the CLI emits those at runtime, and duplicating
them risks doc/runtime drift and bloats the cell. Keeping the cell to outcomes also
keeps it clear of the `/var/lib/braid/luks-headers/` messaging invariant.

### 4. Close the "What happens under the hood" gap

In step 3 of the numbered list (`doctor.md:93`), add the foreign-UUID
reconciliation so the narrative matches the new `foreign_luks_uuid` row. Amend the
existing mounted-pool step to read (added clause in **bold** for review only):

> 3. If the pool is mounted, queries `btrfs filesystem df` and
>    `btrfs device usage --raw` to check RAID profile consistency and metadata
>    allocation headroom, probes for missing devices, **reconciles each live pool
>    member's LUKS UUID against `pool.json` to flag foreign devices,** and runs
>    `btrfs balance status` to detect paused balances.

"check RAID profile consistency" already collectively covers data/metadata/system,
so no separate mention of `system_profile_mismatch` is needed here.

## Explicitly out of scope (with rationale)

- **The example-output block (`doctor.md:21-34`).** It is already a non-exhaustive
  snippet -- it omits `enospc risk`, `meta pressure`, `paused balance`, `ups
  daemon`, and `braid-online` rows too. Leaving it avoids asserting a
  `system profile: <X>` value that would need separate verification of braid's
  mkfs profile defaults, and keeps the diff focused on the contract surface (the
  table). If desired later, the healthy-pool lines would be
  `[ok] foreign uuids  no foreign LUKS UUIDs in live pool` and
  `[ok] system profiles  system profile: <profile>`.
- **A doc-vs-code sync test.** Nothing currently guards the table against the
  registered check set (only the Rust check-name *set* is pinned, `7413454`). Such
  a test would be structure-sensitive (markdown-table parsing) and brittle, and the
  repo tests no other doc enumeration. Not worth adding.

## Verification

1. `mdbook build docs` -- confirms the markdown tables still parse and
   `mdbook-linkcheck` passes (no new cross-links are introduced, so linkcheck is
   unaffected; this just guards against table-syntax breakage).
2. Manual 1:1 cross-check of the final table against the authoritative sources:
   - registration order: `run_doctor` (`cli/src/doctor.rs:1452-1469`)
   - JSON `name` <-> human label map: the formatter match arm
     (`doctor.rs:1493-1513`)
   Confirm all 16 JSON names appear exactly once, in output order.
3. No Rust tests, fixtures, or VM tests are affected (docs-only change); no
   parser-critical tool versions touched, so no fixture refresh obligation.
