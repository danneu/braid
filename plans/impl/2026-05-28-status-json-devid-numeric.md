# Unify `devid` on a JSON number in `braid status --json`

> On promotion, conventional name: `2026-05-28-status-json-devid-numeric.md`.

## Context

Documenting the `braid status --json` schema (commit `7240ed7`) surfaced a type
asymmetry the docs now explain away rather than fix: the same conceptual value
-- a btrfs device id -- crosses the JSON contract as a **string** in one field
and a **number** in two others.

- `disks[].devid` -> `DiskReport.devid: Option<String>` (`cli/src/status.rs:204`) serializes as `"1"`.
- `alert_causes[].devid` -> `AlertCause::{BtrfsDeviceErrors,MissingDevice} { devid: u64 }` (`cli/src/alert.rs:28-29`) serializes as `1`.
- `missing_devids[]` -> `StatusReport.missing_devids: Vec<u64>` (`cli/src/status.rs:86`) serializes as `[1]`.

`docs/commands/status.md:321-322` currently carries a note telling monitoring
authors to special-case it ("Note `devid` here is a JSON **number**, unlike the
string `devid` in `disks[]`"). That note is the smell.

### Root cause (investigated)

devid is fundamentally a `u64` everywhere it originates:

- The kernel models it as `u64 devid`; btrfs-progs prints it with `%4llu`.
- The parser yields `u64`: `parse_devid_line` uses `parse_u64` (`cli/src/parse/btrfs_filesystem_show.rs:69`); `RawDeviceStatsEntry.devid: u64` via serde (`cli/src/parse/btrfs_device_stats.rs`).
- `PoolDevice.devid: u64` (`cli/src/types.rs`).
- All three JSON fields derive from that same `u64`. The string only appears at the `DiskReport`/`HumanDisk` construction boundary, which calls `pd.devid.to_string()` (`cli/src/status.rs:1009,1020`).

The string form is **not load-bearing**: every consumer either re-`format!`s it
back into text or operates on the `u64` elsewhere. Decisive precedent inside the
same file: `CompactDrive.devid` is **already `Option<u64>`**
(`cli/src/status.rs:237`) and renders identically. `DiskReport` and `HumanDisk`
are the outliers. The shared test helper `status_disk_report_named(name, devid: u64)`
(`cli/src/test_fixtures/status.rs:656`) even takes a `u64` and stringifies it.

`None`/`null` **is** meaningful and must be preserved: non-present disks
(missing / LUKS-unreadable / damaged / unpooled) carry `devid: None`
(`cli/src/status.rs:1071,1082`), and `docs/commands/status.md:304` documents
`"devid": null` for them.

### Constraints checked

- **Decision 024** (`docs/design/decisions/024-luks-uuid-identity.md`): devid is the
  fallback identity binding for `null_underlying`/`missing_devids` members. It
  constrains devid's *role*, never its wire representation. Unchanged by this work.
- **No backwards compatibility** (`AGENTS.md`): braid is unreleased; the JSON shape
  may change freely with no migration shim.

## Decision

**Unify on a JSON number: change `DiskReport.devid` and `HumanDisk.devid` from
`Option<String>` to `Option<u64>`, dropping the `.to_string()` conversions.**
`None` stays `None` (serializes as `null` where a disk is not a live member).

Rejected alternatives: (a) make `alert_causes`/`missing_devids` strings -- worse;
contradicts the `CompactDrive` precedent and forces monitoring authors to parse
strings for numeric comparison. (b) Document-only -- the task is to fix, not
re-describe, the asymmetry.

## Changes

### 1. Production / shared rendering -- `cli/src/status.rs`

- `:204` `pub devid: Option<String>` -> `pub devid: Option<u64>` (`DiskReport`).
- `:372` `devid: Option<String>` -> `devid: Option<u64>` (`HumanDisk`).
- `:1009` `devid: Some(pd.devid.to_string())` -> `devid: Some(pd.devid)` (present `DiskReport`).
- `:1020` `devid: Some(pd.devid.to_string())` -> `devid: Some(pd.devid)` (present `HumanDisk`).
- `:1352-1356` drop `.as_deref()` in the verbose-text consumer -- it becomes
  `d.devid.map(|id| format!("devid {id}")).unwrap_or_default()`. `Option<u64>` is
  `Copy`, so `.map()` on the borrowed field compiles; output is byte-identical
  (`u64`'s `Display` of `1` == `"1"`). This makes it structurally identical to the
  already-`u64` `CompactDrive` consumer at `:1244-1247` (no change there).
- No change: `:1071,:1082` (`devid: None`).

### 2. Shared test fixture -- `cli/src/test_fixtures/status.rs`

- `:663` `devid: Some(devid.to_string())` -> `devid: Some(devid)` (param is already `u64`).
- No change: `:678` (`devid: None`).

### 3. Unit tests -- `cli/src/status.rs` (`#[cfg(test)]`)

- Flip every string-literal construction to numeric: `:1896, :2087, :2568, :2627, :2844`
  `devid: Some("1".to_owned())` -> `devid: Some(1)`.
- `:1971` `assert_eq!(d0["devid"], "1")` -> `assert_eq!(d0["devid"], 1)` (the behavioral
  pin on JSON shape, in `status_json_verbose_disks`).
- No change: `:1983` (`d1["devid"].is_null()` -- null is null); `:2607`
  (`human.contains("devid 1")` -- text output unchanged).

### 4. Docs -- `docs/commands/status.md`

- `:280` "btrfs device ID **as a string** (e.g. `"1"`)" -> "**as a number** (e.g. `1`)".
- `:296` JSON example `"devid": "1",` -> `"devid": 1,`.
- `:321-322` **delete** the asymmetry note -- the smell it described is gone.
- No change: `:304` (`"devid": null`); `:81,:116-117,:147` (plain-text output examples,
  unaffected by the JSON type).

### 5. End-to-end test -- `tests/cli/braid-status-rust.py`

Tighten the present-disk contract loop (currently `:120` only checks key presence)
to pin the numeric type end-to-end against live btrfs output:

```python
assert isinstance(d["devid"], int), f"devid must be a JSON number: {d}"
```

The loop already asserts `d["status"] == "present"`, so every iterated disk carries
a devid. This is the structure-insensitive behavioral check that locks the fix in
the VM path (the Rust unit test pins the in-process serializer).

### Explicitly out of scope (do **not** change)

- **`cli/src/alert.rs:185`** `dev.devid.to_string()` builds the `AckedStats`
  `BTreeMap<String, AckedDisk>` **map key** (alert.rs:40-42), parsed back via
  `key.parse::<u64>()` at `:271`. JSON object keys are always strings; this is the
  persisted ack-state file, a separate contract from `status --json`.
- **Design docs** (`024-luks-uuid-identity.md`, `principles.md`): they own devid's
  identity *role*, not its JSON wire type. No invariant changes.
- **`README.md`**: contains no `status --json` devid contract (verified). Nothing to sync.
- **`plans/impl/2026-05-28-document-status-json-array-fields.md`**: landed point-in-time
  record whose "Out of scope" section *anticipated* this fix. Leave as history.
- **No parser/fixture refresh**: devid parsing (`parse_u64`) and all parser-critical
  tool versions are untouched; this is a downstream serialization-type change only.

## Verification

1. `just test-rust` -- `status_json_verbose_disks` and every `DiskReport`/`HumanDisk`
   construction compile and pass with numeric devid. (Compiler will flag any missed
   string-literal site.)
2. Regression sweep -- after the change:
   `git ls-files '*.rs' | xargs rg 'devid:\s*Option<String>|\.devid\.to_string\(\)'`
   must return **only** `cli/src/alert.rs:185` (the intentional `AckedStats` map key).
   No `DiskReport`/`HumanDisk`/fixture hits remain.
3. `mdbook build docs` -- status.md builds; `mdbook-linkcheck` passes (no link churn,
   but it's the standard docs gate).
4. `just test-vm braid-status-rust` -- live end-to-end JSON emits numeric devid; the new
   `isinstance(..., int)` assertion passes against real btrfs output.
5. `just test-vm braid-monitor monitor-hot-unplug` -- confirm `alert_causes[].devid` /
   `missing_devids[]` paths still pass (already numeric; guards against collateral
   regressions in the status/alert JSON path).

Scope is localized to the status/alert JSON surface (no systemd lifecycle, pool lock,
or mount/unmount blast radius), so the focused runs above suffice. Hand back to the
user for a full-suite `just test-vm` rerun rather than running it autonomously.
