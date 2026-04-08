# Unified Confirmation Prompts

## Context

braid's four destructive commands (`add`, `remove`, `remove-missing`, `replace`) use three different confirmation styles: plain `yes`, command-echo phrases (`remove missing`), and danger-echo phrases (`remove without redundancy`). Additionally, the prompts show minimal device info — just a name and by-id path, not enough for the user to sanity-check they're operating on the right disk (e.g., catching that a by-id path points to a 2TB Seagate instead of the intended 12TB Toshiba).

**Goal:** Unify all confirmations to show a rich device-info context block (model, size, serial via lsblk) followed by `Type 'yes' to continue:`. Keep degraded-path warnings as informational text, but remove the special-phrase requirement.

## Plan

### Step 1: Create `cli/src/confirm.rs` — shared confirmation module

New module. Keeps `cmd.rs` as transport-only.

**1a. Move `format_bytes` from `status.rs:1111-1128` into `confirm.rs`.** In `status.rs`, re-export as `pub use crate::confirm::format_bytes;` so existing callers (`preflight.rs`, `doctor.rs`, `status.rs` internals) don't change imports.

**1b. `pub fn get_lsblk_field`** — same logic currently private in `status.rs:743-755`. Calls `CmdRequest::LsblkField` + `parse_lsblk_field`, returns `Option<String>`.

**1c. `DiskHwInfo` struct + `query_disk_hw_info`:**
```rust
pub struct DiskHwInfo {
    pub model: Option<String>,
    pub serial: Option<String>,
    pub size: Option<u64>,
}

pub fn query_disk_hw_info<R: CommandRunner>(runner: &R, device: &str) -> DiskHwInfo { ... }
```

**1d. `pub fn format_hw_info_line(info: &DiskHwInfo) -> Option<String>`** — renders `"Toshiba MN07ACA12T · 12.00 TiB · serial 1234ABCD"` or `None` if no info available. Uses `format_bytes` for size.

**1e. `pub fn confirm_yes() -> Result<(), String>`** — prints `Type 'yes' to continue: ` to stderr, reads stdin, returns `Err("aborted by user")` on mismatch.

Register `mod confirm` in `cli/src/lib.rs` (where all module declarations live).

### Step 2: Add `Size` to `LsblkFieldKind` + `--bytes` flag

**File:** `cli/src/cmd.rs`

- Add `Size` variant to `LsblkFieldKind` enum (line 13-16)
- Map `Size` to `"SIZE"` in the `to_argv` match (line 360-368)
- Add `--bytes` flag to `LsblkField` args unconditionally. Split combined short opts into separate args (`-n`, `-d`, `-b`, `-o`) for readability. This makes SIZE return raw bytes; it doesn't affect MODEL/SERIAL output

### Step 3: Update `status.rs` to use shared helpers

**File:** `cli/src/status.rs`

- Remove private `get_lsblk_field` (lines 743-755)
- Replace call sites (lines 793-794) with `crate::confirm::get_lsblk_field`
- Add `pub use crate::confirm::format_bytes;` (re-export so `preflight.rs`, `doctor.rs` don't change)

### Step 4: Rewrite `add.rs` confirmation

**File:** `cli/src/add.rs`

- **Always confirm** (unless `--yes`), not just when disks need LUKS format. Adding to the pool changes topology even for already-LUKS disks.
- Move confirmation before passphrase read (confirm what you're doing before asking for credentials)
- Replace `add_confirm_message_multi` with a pure formatter that takes pre-queried `DiskHwInfo` values:

```rust
struct AddConfirmDisk<'a> {
    name: &'a str,
    by_id: &'a str,
    hw: DiskHwInfo,
    needs_luks_format: bool,
}
fn format_add_confirm(disks: &[AddConfirmDisk]) -> String { ... }
```

- Thin wrapper calls `query_disk_hw_info` per disk, then calls the pure formatter
- Use `confirm::confirm_yes()` for the prompt

For `add`, hardware info comes from lsblk on the by-id path provided by the user. This is the actual device being operated on — no membership indirection, no stale-data risk.

**Prompt format:**
```
Add to pool:
  toshiba  /dev/disk/by-id/ata-TOSHIBA_MN07ACA12T_1234
           Toshiba MN07ACA12T · 12.00 TiB · serial 1234ABCD
           Will be LUKS-formatted (existing data will be inaccessible)

Type 'yes' to continue:
```

Update existing tests (`add_confirm_message_single_disk`, `add_confirm_message_multi_disk`) to call the pure formatter with synthetic `DiskHwInfo`.

### Step 5: Rewrite `remove.rs` confirmation

**File:** `cli/src/remove.rs`

Hardware info comes from `PoolDevice.underlying` (the live block device, e.g. `/dev/sda`) via lsblk. No membership needed for confirmation — membership stays where it is (line 154, after confirmation, for journal building).

- Replace the two-branch confirmation (normal vs degraded) with a single flow: always show disk info, add `WARNING` line when `remaining == 1`, always use `confirm_yes()`
- Keep the `remaining == 0` → hard error

Pure formatter:
```rust
struct RemoveConfirmDisk<'a> {
    name: &'a str,
    hw: Option<DiskHwInfo>,  // from lsblk on PoolDevice.underlying
    devid: u64,              // from live PoolDevice
}
fn format_remove_confirm(disk: &RemoveConfirmDisk, remaining: usize, total: usize) -> String
```

**Prompt format (normal):**
```
Remove from pool:
  toshiba  Toshiba MN07ACA12T · 12.00 TiB · serial 1234ABCD
           devid 2 · data will migrate to remaining disks

Pool: 3 disks → 2 disks

Type 'yes' to continue:
```

**Prompt format (degraded):**
```
Remove from pool:
  toshiba  Toshiba MN07ACA12T · 12.00 TiB · serial 1234ABCD
           devid 2 · data will migrate to remaining disk

WARNING: Pool will have 1 disk — no RAID1 redundancy.

Type 'yes' to continue:
```

### Step 6: Rewrite `remove_missing.rs` confirmation

**File:** `cli/src/remove_missing.rs`

- **Move `membership::load_membership` and `resolve_removal_target`** from lines 172-176 to before the confirm block. This resolves devid → disk name before the prompt. Remove the duplicate at line 172.
- If `resolve_removal_target` fails (no membership match for devid), **keep this as a fatal error** — current code intentionally treats this as a hard failure because the post-mutation membership reconciliation requires a valid name mapping. Do not add a fallback execution path.
- No lsblk query — missing disks have no hardware to probe
- Use `confirm_yes()`

Pure formatter (name is always available since resolve_removal_target is a hard error on failure):
```rust
fn format_remove_missing_confirm(
    name: &str,
    devid: u64,
    remaining_present: usize,
    missing_count: u64,
) -> String
```

**Prompt format:**
```
Remove missing device from pool:
  toshiba (devid 2)  missing — no hardware info available
  Data on remaining disks will be rebalanced.

Pool: 2 present + 1 missing → 2 disks

Type 'yes' to continue:
```

### Step 7: Rewrite `replace.rs` confirmation

**File:** `cli/src/replace.rs`

For the old disk (Live): resolve `underlying` from the already-probed `PoolDevice` (`pool.devices.iter().find(|d| d.mapper == old_mn).map(|d| &d.underlying)`), then pass that path directly to `query_disk_hw_info`. If the pool lookup returns `None` (should not happen since `resolve_replace_source` already validated presence), degrade to `hw: None` — never panic. For `Missing`, no hw info (disk is dead). Membership load stays at line 195 (after confirmation, for journal building).

For the new disk: by-id comes directly from CLI args. Hw info from lsblk on the by-id path.

- Query hw info for old disk (Live only) and new disk
- Replace `replace_confirm_message` with new pure formatter
- Replace two-branch confirmation with single flow + WARNING line
- Use `confirm_yes()`

Pure formatter:
```rust
struct ReplaceConfirmOld<'a> {
    name: &'a str,
    hw: Option<DiskHwInfo>,  // from lsblk on live underlying; None for Missing
    source: &'a ReplaceSource,
}
struct ReplaceConfirmNew<'a> {
    name: &'a str,
    by_id: &'a str,
    hw: DiskHwInfo,
    needs_luks_format: bool,
    is_rebuild: bool,  // true when old is Missing
}
fn format_replace_confirm(
    old: &ReplaceConfirmOld,
    new: &ReplaceConfirmNew,
    total_devices: u64,
) -> String
```

**Prompt format (missing old):**
```
Replace disk:
  old: toshiba (devid 2)  missing — no hardware info available
  new: ironwolf           /dev/disk/by-id/ata-ST12000VN0008_5678
                          Seagate ST12000VN0008 · 12.00 TiB · serial 5678EFGH
                          Will be LUKS-formatted (existing data will be inaccessible)
                          Data will be rebuilt from RAID redundancy.

Pool: 3 disks → 3 disks

Type 'yes' to continue:
```

**Prompt format (live old):**
```
Replace disk:
  old: toshiba   Toshiba MN07ACA12T · 12.00 TiB · serial 1234ABCD
                 devid 2 · will be replaced in-place
  new: ironwolf  /dev/disk/by-id/ata-ST12000VN0008_5678
                 Seagate ST12000VN0008 · 12.00 TiB · serial 5678EFGH
                 Will be LUKS-formatted (existing data will be inaccessible)

Pool: 3 disks → 3 disks

Type 'yes' to continue:
```

Update existing tests (`replace_confirm_warns_about_luks_format_for_non_luks_disk`, `replace_confirm_missing_shows_rebuild_message`, `replace_confirm_live_does_not_say_dead`) to call the pure formatter.

### Step 8: Update documentation

**File:** `README.md`

- Line 187: Update remove section — confirmation is now always `yes` with device info shown; mention the degraded-path warning is informational
- No changes needed for add/replace/remove-missing sections (they don't document specific confirmation phrases)

**File:** `docs/decisions/012-intent-cli.md`

- Line 47: Update point 3 — all destructive operations now use unified `yes` confirmation with rich device-info context; the old "calibrated to risk" phrasing is replaced by "informational warnings + uniform confirmation"

**Leave `docs/decisions/007-disk-pool-management.md` untouched** — it is superseded and should not be updated.

## Testing

**Pure formatter tests** (no MockRunner needed) — one test per format variant per command:
- `add`: single disk, multi disk, already-LUKS disk (no format warning)
- `remove`: normal (≥2 remaining), degraded (1 remaining), hw info unavailable (lsblk fails)
- `remove-missing`: with resolved name (only path — unresolved is a fatal error)
- `replace`: live old, missing old, new disk needs LUKS format, degraded (1 device)

**Shared helper tests:**
- `format_hw_info_line`: all fields present, some missing, all missing
- `format_bytes`: already tested (moved from status.rs)
- `CmdRequest::LsblkField { field: Size }`: assert argv includes `-n`, `-d`, `-b`, `-o`, `SIZE`
- `query_disk_hw_info`: test that size string parses to `u64` correctly; test non-numeric SIZE yields `size: None` (not error)
- `format_remove_confirm` / `format_replace_confirm` with `hw: None`: verify prompt is readable (explicit snapshot)

**`confirm_yes` testing:** Existing command-level tests overwhelmingly use `yes: true`, so the confirmation read path is not implicitly covered. Make `confirm_yes` testable by accepting a reader: `pub fn confirm_yes_from<R: BufRead>(reader: &mut R) -> Result<(), String>`. The public `confirm_yes()` wraps this with `stdin().lock()`. Add unit tests for `confirm_yes_from`: accepts "yes\n", rejects "no\n", rejects empty input.

**End-to-end:** `just test-rust` runs all unit tests. `just test-parsers` validates lsblk field parsing still works with `--bytes` flag.

## Files changed

| File | Changes |
|------|---------|
| `cli/src/confirm.rs` | **New.** `format_bytes`, `get_lsblk_field`, `DiskHwInfo`, `query_disk_hw_info`, `format_hw_info_line`, `confirm_yes_from`, `confirm_yes` |
| `cli/src/lib.rs` | Register `mod confirm` |
| `cli/src/cmd.rs` | `LsblkFieldKind::Size`, `--bytes` flag |
| `cli/src/status.rs` | Remove private `get_lsblk_field`, `pub use crate::confirm::format_bytes;`, use `confirm::get_lsblk_field` |
| `cli/src/add.rs` | Always-confirm flow, new pure formatter, updated tests |
| `cli/src/remove.rs` | New pure formatter using live PoolDevice.underlying for hw info, `confirm_yes` |
| `cli/src/remove_missing.rs` | Move membership load + resolve earlier (hard error on failure), new pure formatter, `confirm_yes` |
| `cli/src/replace.rs` | New pure formatter using live underlying for hw info, updated tests |
| `README.md` | Update remove section confirmation wording |
| `docs/decisions/012-intent-cli.md` | Update safety model point 3 |
