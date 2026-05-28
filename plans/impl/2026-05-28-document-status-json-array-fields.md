# Plan: document the `status --json` array fields

## Context

`braid status --json` is the machine-readable contract monitoring tools
integrate against. Decision 024 (LUKS UUID Is Disk Identity) deliberately
exposes `luks_uuid` in machine-readable state as the persistent member
identity, and `tests/cli/braid-status-rust.py:116-125` pins the
`disks[]` element keys (`mapper`, `by_id`, `luks_uuid`, `devid`,
`status`, `errors`) as a real contract.

But the reference doc undersells that contract. `docs/commands/status.md`
documents the JSON `disks[]` field as "array of disk reports with `name`,
`status`, `devid`, `errors`, etc." -- hiding `luks_uuid` (the identity
field) plus `by_id`, `mapper`, and `underlying` behind "etc." Two sibling
bullets in the same section are vague for the same reason: `alert_causes`
("array of alert cause objects") and `allocation` ("array of block group
type entries"). A monitoring author has no documented field contract for
any of the three.

This is primarily a documentation change: it documents the existing,
partially test-pinned schema, with no behavior change. It also adds one
small assertion to an existing Rust unit test to pin the single contract
the docs now state that nothing currently guards (see Verification).

## Approach

The core change edits one section of one file: the **JSON output** section
of `docs/commands/status.md` (the bullet list under `## JSON output`,
currently ~lines 247-314). Rewrite three bullets into explicit field
contracts. House style for JSON in this section is bullets + prose + a
fenced `json` example (mirroring how the existing `profile` field is
documented). No tables. Use `--`, never em-dash (project CLI style). A
single supporting test assertion is described in Verification.

Field facts are confirmed against source:
`DiskReport`/`AllocationEntry` structs in `cli/src/status.rs:198-218,89-95`;
per-field population (present vs non-present) in `build_disk_reports`
(`cli/src/status.rs:947-1094`); `DiskStatus` serde `kebab-case` enum in
`cli/src/status.rs:176-184`; `StatusReport` serde skip attributes in
`cli/src/status.rs:59-87`; `AlertCause` internally-tagged `snake_case`
union in `cli/src/alert.rs:25-32`.

### 1. `disks[]` -- replace the one-liner with a field list + `json` example

Replace:

```
- `disks`: array of disk reports with `name`, `status`, `devid`, `errors`, etc.
```

with a nested field list:

- `disks`: array of per-disk reports -- one element per disk braid knows
  about: present pool members (matched members and foreign live devices),
  plus configured disks that are not currently live pool members (reported
  as `missing`, `unknown`, or `luks-header-*`; see the `status` values
  below). The field list below describes a **present** element (as in the
  example); non-present elements differ as called out per field and in the
  note after the example.
  - `luks_uuid`: the disk's **live-observed** LUKS UUID -- the persistent
    member identity. For a matched present member it equals the `pool.json`
    membership key; a foreign present device carries a live UUID that is
    **not** in membership (paralleling its mapper-basename `name`).
    **Populated for present disks only.** A non-present disk reports `""`,
    because the UUID is read from the live device and is unavailable when
    the device is absent; correlate non-present disks by `name`, not
    `luks_uuid`.
  - `name`: operator-facing name (e.g. `toshiba1`). For a matched present
    member it is resolved via the UUID-keyed membership join; for a foreign
    present device it falls back to the mapper basename; for a non-present
    disk it is the configured name. For display/command selection, not
    identity.
  - `by_id`: stable `/dev/disk/by-id/...` hardware path -- a runtime
    handle, not identity.
  - `mapper`: the **observed** device-mapper name -- normally
    `braid-<name>`, but may differ when a member is open under a drifted
    mapper (decision 024 tolerates mapper drift). A runtime handle, not
    identity; do not reconstruct it as `braid-${name}`.
  - `underlying`: current backing block device (e.g. `/dev/sda`), or
    `null` when the disk is not present.
  - `devid`: btrfs device ID **as a string** (e.g. `"1"`), or `null`
    when the disk is not a live pool member.
  - `status`: one of `present`, `missing`, `luks-header-unreadable`,
    `luks-header-damaged`, `unknown`.
  - `errors`: btrfs I/O error counters (`read`, `write`, `flush`,
    `corruption`, `generation`, all integers). Present when btrfs device
    stats are available; omitted entirely otherwise -- including for
    present disks when `btrfs device stats` fails (which also emits a
    `btrfs device stats failed` advisory).

Then a fenced `json` example of one present element, plus a note covering
the non-present shape. Reuse the human-section example identifiers for
consistency (`toshiba1`, by-id `ata-TOSHIBA_MN07ACA12T_1234`, UUID
`aaaaaaaa-1111-2222-3333-444444444444`, `/dev/sda`, devid `"1"`):

```json
{
  "name": "toshiba1",
  "mapper": "braid-toshiba1",
  "by_id": "/dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234",
  "luks_uuid": "aaaaaaaa-1111-2222-3333-444444444444",
  "devid": "1",
  "underlying": "/dev/sda",
  "status": "present",
  "errors": { "read": 0, "write": 0, "flush": 0, "corruption": 0, "generation": 0 }
}
```

> A non-present disk (`missing`, `unknown`, or `luks-header-*`) reports
> `"luks_uuid": ""`, `"devid": null`, `"underlying": null`, and no
> `errors` key. Correlate it by `name`.

### 2. `alert_causes[]` -- document the tagged union

Replace:

```
- `alert_causes`: array of alert cause objects
```

with:

- `alert_causes`: array of alert cause objects. **Omitted entirely when no
  alert is active** (the key is absent, not `[]`) -- check the
  always-present `alert_active` boolean first, mirroring how `advisories`
  is "omitted when none". When present, each object is tagged by a `type`
  discriminator:
  - `{ "type": "btrfs_device_errors", "devid": <number> }` -- btrfs I/O
    errors on that device.
  - `{ "type": "missing_device", "devid": <number> }` -- a device counted
    as missing.
  - `{ "type": "smartd_alert" }` -- a SMART health warning from smartd.
  - `{ "type": "computation_error", "detail": "<string>" }` -- braid could
    not compute alert state; `detail` explains.

  Note `devid` here is a JSON **number**, unlike the string `devid` in
  `disks[]`.

### 3. `allocation[]` -- document the entry shape

Replace:

```
- `allocation`: array of block group type entries
```

with:

- `allocation`: array of block-group entries, one per allocated type.
  Each entry has `bg_type` (e.g. `Data`, `Metadata`, `System`), `profile`
  (raw btrfs profile name, same vocabulary as `profile` above),
  `used_bytes`, and `allocated_bytes` (both integers). Omitted when the
  pool is not mounted or `btrfs filesystem df` failed.

## Critical files

- `docs/commands/status.md` -- the JSON output section (the main change).
- `cli/src/status.rs` -- add one assertion to the existing
  `status_json_verbose_disks` unit test (see Verification); otherwise
  read-only for field accuracy (`DiskReport`, `DiskStatus`,
  `AllocationEntry`, `build_disk_reports`).
- Read-only references: `cli/src/alert.rs` (`AlertCause`),
  `docs/design/decisions/024-luks-uuid-identity.md` (identity wording),
  `tests/cli/braid-status-rust.py` (existing present-disk `disks[]` pins).

## Out of scope / observations

- **`devid` string-vs-number asymmetry.** `disks[].devid` serializes as a
  string (`"1"`); `alert_causes[].devid` and `missing_devids[]` as numbers
  (`1`). This plan documents reality. The asymmetry is a candidate for a
  future code consistency fix (a separate change, not this docs task).
- **Empty `luks_uuid` for non-present disks is intentional; do not "fix" it
  in code.** `build_disk_reports` sets `luks_uuid: String::new()` for
  non-present disks (`cli/src/status.rs:1070`). Populating it from the
  `pool.json` membership key was considered and rejected: decision 024
  treats `luks_uuid` as the *live-observed* identity and explicitly avoids a
  duplicate value-side UUID (`024:56-57`), assigning `devid` as the fallback
  binding for missing members instead (`024:43,156-158`). It would also
  ripple into human output, where the LUKS line is gated on
  `!luks_uuid.is_empty()` (`cli/src/status.rs:1377`) and would begin
  printing for missing disks. Document the empty string; leave the code.
- The human-readable **Per-disk detail** section (status.md:140-156)
  already names LUKS UUID and is correct; no change there. Its Model/Serial
  lines are human-output only -- the JSON `DiskReport` has no model/serial
  fields, so the new JSON field list must not imply otherwise.
- Confirm `README.md` does not carry a competing JSON schema description
  (it is cookbook-style and should not). If it does not, nothing to sync;
  if it does, align it.

## Verification

- `mdbook build docs` -- confirms the section still builds and
  `mdbook-linkcheck` passes (no new cross-links, but guards markdown/fence
  breakage).
- Field-by-field cross-check of the three documented contracts against the
  source structs cited above (done during planning; re-confirm the example
  values against a real `braid status --json` or `build_disk_reports`
  during implementation).
- One small test addition. The non-present `luks_uuid == ""` contract this
  doc now states is pinned by nothing today: `braid-status-rust.py` pins
  only present-disk keys, and the `status_json_verbose_disks` unit test
  (`cli/src/status.rs:1890`) builds the missing/unreadable/damaged elements
  but asserts only their `status`, null `devid`, and null `errors` -- never
  `luks_uuid`. Add to that existing test an assertion that a non-present
  element serializes `"luks_uuid": ""` (and, while there, `"underlying":
  null`). This is a ~2-line addition to a scenario the test already
  constructs, not a new test, and it is behavioral and structure-insensitive
  (it checks serialized JSON). Run with `just test-rust`.
- Present-disk `disks[]` keys remain pinned by both `status_json_verbose_disks`
  and `braid-status-rust.py`; `AlertCause` and `AllocationEntry` are simple
  derives. No other coverage is owed for this change.
- Proofread pass: `--` not em-dash; kebab-case `status` values exact;
  `devid` string-vs-number distinction stated in both places.
