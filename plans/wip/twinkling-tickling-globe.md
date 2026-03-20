# Plan: kernel journal alert source for `braid monitor`

## Context

braid currently detects disk health issues from three sources: btrfs device stat counters, missing-device detection, and a smartd flag file. This misses a class of real storage failures that the Linux kernel observes and logs to the journal — write EIOs, bad-sector reads, btrfs subsystem errors — before (or instead of) those counters updating. Two repro VMs (`repro-kernel-journal-write-error`, `repro-kernel-journal-bad-sector`) confirm the pinned NixOS/kernel stack produces `BTRFS error` lines in `journalctl -k -o json` that reference the braid mapper device by name.

This plan adds a journal-backed alert source that fits braid's existing alert model: monitor detects, status/TUI show causes, ack silences and prevents replay.

NixOS-only: journald, udev, and systemd are always present. No portability layer. `journalctl` is already on the wrapper PATH (`pkgs.systemd` in `wrapper.nix:10`).

---

## Key architectural insight: the latch as an append/refresh log

The current latch write (`monitor.rs:107`) overwrites the latch with each cycle's live state. This works for existing causes only because they're all persistent/pollable (counters stay elevated, flag files persist, missing devices remain missing). But it's subtly wrong even today: if two causes are latched on cycle N, and one resolves on cycle N+1 while the other persists, the latch is overwritten with only the surviving cause — violating the ADR invariant that "alerts persist until `braid ack`, even if the triggering condition disappears."

Journal entries are events, not pollable state — the cursor advances past them, so they can't be re-detected. Adding journal as a live input to `compute_alert_state_with_devid_map` would lose journal causes on the very next cycle.

Fix: the latch becomes an **append/refresh log** of all unacked causes from all sources. Each monitor cycle loads the existing latch, computes new causes, and merges. Previously-latched causes that aren't re-detected are carried forward. Newly-detected causes replace their latched counterpart (same key = fresher evidence). This fixes the global invariant for all cause types and naturally accommodates event-based sources like journal.

---

## 1. Alert model changes

**File: `cli/src/alert.rs`**

New variant:

```rust
KernelJournalError {
    message: String,
    cursor: String,
    disk_name: Option<String>,
}
```

- `message`: excerpt from the kernel journal `MESSAGE` field (truncated to 200 chars at a safe char boundary).
- `cursor`: the journald `__CURSOR` for this entry — the journal's per-entry unique identity. Used as the deduplication key when `disk_name` is `None`.
- `disk_name`: braid disk name when resolvable (e.g., `"toshiba"` from `/dev/mapper/braid-toshiba`). `None` when the entry cannot be attributed to a braid disk.

`disk_name` may legitimately be `None` because:
- The MESSAGE may mention only a generic dm name like `dm-0` (no `braid-` prefix).
- `_UDEV_DEVNODE` / `_UDEV_DEVLINK` may be absent on the entry.
- The entry may reference an underlying block device instead of `/dev/mapper/braid-<name>`.
- The error may be filesystem-level with no device path at all.
- The entry may describe btrfs activity outside braid's managed pool.

`compute_alert_state_with_devid_map()` is **unchanged** — it stays focused on live/pollable sources. Journal causes go directly into the latch via the merge.

### Device name extraction

The braid mapper naming convention (`braid-<name>`) makes this a regex extraction, no reverse dm-mapper lookups:

1. Check MESSAGE for `/dev/mapper/braid-([a-zA-Z0-9_-]+)` — extract the name.
2. Check `_UDEV_DEVNODE` and `_UDEV_DEVLINK` fields for the same pattern.
3. No match → `disk_name = None`.

---

## 2. Latch merging

**File: `cli/src/alert.rs`**

New function:

```rust
pub fn merge_into_latch(
    existing_latch: Option<&AlertState>,
    live_causes: &[AlertCause],
    journal_causes: &[AlertCause],
) -> AlertState
```

Algorithm:

1. Start with all causes from the existing latch (carried forward).
2. For each cause in `live_causes`: if an existing cause matches by key, replace it; otherwise append.
3. For each cause in `journal_causes`: same — replace by key or append.
4. Result: `AlertState { active: !causes.is_empty(), causes }`.

**Cause key matching** (`same_cause_key`):

| Variant | Key |
|---------|-----|
| `BtrfsDeviceErrors` | `devid` |
| `MissingDevice` | `devid` |
| `SmartdAlert` | singleton |
| `KernelJournalError` | `disk_name` if `Some`, otherwise `cursor` |
| `ComputationError` | singleton |

This means:
- A latched `BtrfsDeviceErrors { devid: 1 }` persists even if counters reset, until `braid ack`.
- A latched `KernelJournalError` for disk "toshiba" persists even after the cursor advances past it. A new detection for the same disk replaces the old one (fresher evidence).
- Two anonymous journal errors (`disk_name: None`) with different cursors are distinct causes — they accumulate rather than overwriting each other.
- Different keys accumulate (e.g., journal errors on two different disks = two causes, or two unattributable errors = two causes).

### Write condition

Only write the latch when the merged state has active causes. If the existing latch had causes and no new causes are detected, the merged state is still active (carried forward), so the latch is re-written. If there's nothing in the latch and nothing new, no write. This matches current behavior — monitor never removes the latch.

---

## 3. Journal module

**New file: `cli/src/journal.rs`**

### Constants

```rust
pub const CURSOR_FILE: &str = "/var/lib/braid/journal-cursor";
```

### Core function

```rust
pub fn check_journal(cursor_path: &Path) -> JournalCheckResult
```

1. Load cursor from file (`None` if missing).
2. Run `journalctl -k -o json --no-pager --after-cursor=<cursor>` (or `--boot` if no cursor).
3. Parse JSON lines. Extract `MESSAGE`, `__CURSOR` per entry.
4. Filter: `MESSAGE` contains `"BTRFS error"`.
5. For matches, extract `disk_name` from MESSAGE / `_UDEV_DEVNODE` / `_UDEV_DEVLINK`.
6. Deduplicate: collapse entries with the same `disk_name` (when `Some`) into one cause using the first matching message. Entries with `disk_name: None` are kept individually (each has a unique cursor).

### Return type

```rust
pub struct JournalCheckResult {
    pub causes: Vec<AlertCause>,       // KernelJournalError variants
    pub new_cursor: Option<String>,    // __CURSOR of last entry (matching or not)
}
```

### Cursor persistence

- `save_cursor(path, cursor)` — uses `atomic_write` from `state_io.rs`.
- `load_cursor(path) -> Option<String>`.
- `advance_cursor_to_now(path)` — runs `journalctl -k -o json --no-pager -n 1 --output-fields=__CURSOR`, saves that cursor. Used by ack.

### Error handling

If `journalctl` fails, return `JournalCheckResult { causes: vec![], new_cursor: None }` and print a warning to stderr. Never blocks the rest of monitor.

---

## 4. Monitor integration

**File: `cli/src/monitor.rs`**

Revised flow:

```
 1. Scan kernel journal (BEFORE pool check)
 2. Probe pool
 3. If pool offline:
    a. Load existing latch
    b. Merge: existing latch + journal causes (no live causes)
    c. If merged state active → write latch → save cursor → return PoolOfflineJournalAlert
    d. If no journal causes found → save cursor (if any) → return PoolOffline
 4. Run btrfs device stats
 5. Load acked stats
 6. Check smartd flag
 7. Build devid map
 8. Self-heal stale ack state
 9. Compute live alert state via compute_alert_state_with_devid_map() (unchanged)
10. Load existing latch
11. Merge: existing latch + live causes + journal causes
12. If merged state active → write latch
13. Save journal cursor (ONLY after latch write succeeds)
14. Return result based on merged state
```

Critical ordering: **cursor is saved only after journal causes are durably merged into the latch.** If any step between journal scan (1) and latch write (12) fails (probe error, parse error, unmapped device), the cursor is NOT advanced. The same journal entries will be re-scanned on the next cycle and re-produce the same causes, which merge correctly.

### Error path (unmapped device)

The current `ComputationError` path at `monitor.rs:85-99` needs updating:

```
On unmapped device error:
  a. Load existing latch
  b. Merge: existing latch + journal causes + ComputationError cause
  c. Write merged latch
  d. Save cursor (after latch succeeds)
  e. Return Err
```

This preserves any previously latched causes and any new journal causes even on computation error.

### New MonitorResult variant

```rust
pub enum MonitorResult {
    PoolOffline,
    PoolOfflineJournalAlert(AlertState),  // NEW
    Ok,
    Alert(AlertState),
}
```

**File: `cli/src/main.rs`** — Map `PoolOfflineJournalAlert` → exit 1 (starts beeper).

---

## 5. Ack integration

**File: `cli/src/ack.rs`**

### Count from latch

The latch is the authoritative alert state. Ack reads it for the count instead of recomputing from live sources (which can't see latched journal or resolved-but-unacked causes).

Revised online ack flow:

```
1. Read latch for count
2. Probe pool, run btrfs device stats, build devid map (still needed for snapshot)
3. Snapshot btrfs device stats to acked-stats.json (existing)
4. Advance journal cursor to now
5. Remove smartd flag + alert latch (existing)
6. Stop beeper (existing)
7. Print confirmation using latch count
```

### Offline ack

Add `journal::advance_cursor_to_now()` alongside existing `remove_alert_latch()` and `remove_smartd_alert_flag()`.

---

## 6. Status display

**File: `cli/src/status.rs`**

### Human output

Add match arm in `format_status_human` (around line 876):

```rust
AlertCause::KernelJournalError { message, disk_name, .. } => {
    match disk_name {
        Some(name) => out.push_str(&format!("  - kernel storage error ({name}): {message}\n")),
        None => out.push_str(&format!("  - kernel storage error: {message}\n")),
    }
}
```

(`cursor` is not displayed — it's an internal dedup key.)

### JSON output

Handled by serde:
```json
{"type": "kernel_journal_error", "message": "BTRFS error ...", "cursor": "s=...", "disk_name": "toshiba"}
```

### `resolve_alert_state()`

No changes. Journal causes are in the latch, which this function already reads. The smartd bridge remains as-is for between-cycle smartd fires.

---

## 7. TUI

No changes. The cause-neutral red alert banner already covers this.

---

## 8. NixOS module

No changes. `journalctl` already on PATH, monitor runs as root.

---

## 9. Parser scope (v1)

**Match**: `MESSAGE` contains `"BTRFS error"`. Confirmed by both repros.

**Device extraction**: regex for `/dev/mapper/braid-([a-zA-Z0-9_-]+)` in MESSAGE, `_UDEV_DEVNODE`, `_UDEV_DEVLINK`.

**Deferred** (requires additional repro tests): `"I/O error"`, `"Buffer I/O error"`, `"blk_update_request"`, `"critical medium error"`.

---

## 10. Test strategy

### Rust unit tests (`just test-rust`)

**`cli/src/journal.rs` tests:**

1. Parse matching "BTRFS error" entries from synthetic JSON.
2. Non-matching entries skipped.
3. `__CURSOR` from last entry returned as `new_cursor`.
4. Empty output → no causes, no cursor.
5. Message truncation at 200 chars.
6. Disk name extraction: `/dev/mapper/braid-toshiba` → `Some("toshiba")`.
7. Disk name extraction: no `braid-` pattern → `None`, cursor preserved as key.
8. Deduplication: multiple entries for same disk_name → one cause per disk.
9. Multiple entries for different disks → separate causes.
10. Anonymous entries (disk_name: None) with different cursors → separate causes.

**`cli/src/alert.rs` tests:**

10. `merge_into_latch`: live causes + journal causes → combined state.
11. `merge_into_latch`: no new causes → all latched causes carried forward.
12. `merge_into_latch`: new journal cause for same disk_name → replaces latched one.
12a. `merge_into_latch`: new anonymous journal cause (different cursor) → accumulates alongside existing anonymous causes.
13. `merge_into_latch`: live cause for same devid → replaces latched one.
14. `merge_into_latch`: live causes missing a previously-latched devid → latched cause preserved (the key invariant fix).
15. `merge_into_latch`: empty latch + journal causes → active alert from journal alone.
16. `same_cause_key`: all variant combinations tested, including KernelJournalError with same disk_name, different disk_name, same cursor, different cursor.

### Test fixture

**New file: `cli/tests/fixtures/nixos-25.11/journalctl-btrfs-error.jsonl`**

Synthetic journal JSON lines modeled on repro test output.

### NixOS VM integration test

**New files:**
- `tests/cli/braid-journal-alert.nix`
- `tests/cli/braid-journal-alert.py`

Registered in `flake.nix`.

Test flow:
1. Create a 2-disk RAID1 pool with braid config.
2. Use dm-flakey to inject a write failure on one disk.
3. `braid monitor` → exit 1.
4. `braid status` → banner includes "kernel storage error" with disk name.
5. `braid status --json` → `alert_causes` contains `kernel_journal_error` with `disk_name`.
6. `braid monitor` again → exit 1 (journal cause persists in latch even though cursor advanced past the entries).
7. `braid ack` → clears. Reports correct count.
8. `braid monitor` → exit 0.
9. `/var/lib/braid/journal-cursor` exists.

### Existing repro tests

Remain as-is.

---

## 11. ADR update

**File: `docs/decisions/alerts.md`**

- Add `KernelJournalError { message, cursor, disk_name }` to "Alert causes" list.
- Rename "Two detection sources" → "Three detection sources."
- Add section "Latch as append/refresh log" explaining the invariant fix: all causes persist until ack, even if the triggering condition resolves. This applies to all cause types, not just journal.
- Add section "Kernel journal monitoring":

```markdown
### Kernel journal monitoring

braid reads the kernel journal (`journalctl -k -o json`) for btrfs error messages
as a supplementary alert source. This catches errors that btrfs device stats counters
may not reflect immediately, such as transient write failures.

Journal entries are events, not pollable state. The cursor advances past them after
each scan. To honor the "latched until ack" invariant, journal-derived causes are
merged directly into the alert latch and persist until `braid ack`. The cursor is
only advanced after causes are durably merged into the latch.

Journal scanning runs before the pool-mounted check so that boot-time and
unlock-time btrfs errors are caught.

Device identity is extracted from journal MESSAGE and structured fields by matching
the `braid-<name>` mapper naming convention.

`braid ack` advances the cursor to the present, preventing replay.

Cursor stored at `/var/lib/braid/journal-cursor`. v1 matches only `BTRFS error`
in the MESSAGE field. Broader patterns deferred until confirmed by repro tests.
```

---

## Implementation order

| Step | File(s) | What |
|------|---------|------|
| 1 | `cli/src/journal.rs` (new) | Journal reading, parsing, cursor management, disk name extraction. Unit tests. |
| 2 | `cli/src/lib.rs` | Add `pub mod journal;` |
| 3 | `cli/src/alert.rs` | Add `KernelJournalError` variant. Add `merge_into_latch()` + `same_cause_key()`. Unit tests. |
| 4 | `cli/src/monitor.rs` | Journal scan before pool check. Latch merge replaces direct latch write. Cursor saved after latch. Error path merges. Add `PoolOfflineJournalAlert`. |
| 5 | `cli/src/main.rs` | Map `PoolOfflineJournalAlert` → exit 1. |
| 6 | `cli/src/ack.rs` | Derive count from latch. Add `journal::advance_cursor_to_now()` in both online and offline paths. |
| 7 | `cli/src/status.rs` | Add `KernelJournalError` match arm in `format_status_human`. |
| 8 | `cli/tests/fixtures/nixos-25.11/journalctl-btrfs-error.jsonl` (new) | Fixture for unit tests. |
| 9 | `docs/decisions/alerts.md` | ADR update. |
| 10 | `tests/cli/braid-journal-alert.nix` + `.py` (new), `flake.nix` | Integration test. |

---

## Critical files

| File | Role |
|------|------|
| `cli/src/journal.rs` (new) | Journal reading, parsing, cursor, device extraction |
| `cli/src/alert.rs` | `KernelJournalError` variant, `merge_into_latch()`, `same_cause_key()` |
| `cli/src/monitor.rs` | Journal scan before pool check, latch merge, cursor-after-latch ordering |
| `cli/src/ack.rs` | Latch-based count, cursor advance in both paths |
| `cli/src/status.rs` | Human display of new cause |
| `cli/src/main.rs` | Exit code for `PoolOfflineJournalAlert` |
| `cli/src/state_io.rs` | Reuse `atomic_write` for cursor persistence |
| `docs/decisions/alerts.md` | ADR |

---

## Verification

1. `just test-rust` — all unit tests pass (journal parsing, latch merging, cause key matching, existing tests unbroken).
2. `just test braid-journal-alert` — full integration: dm-flakey → journal → monitor → status → re-monitor (latch persists after cursor advance) → ack → re-monitor (clear).
3. `just test repro-kernel-journal-write-error` — existing repro still passes.
4. `just test repro-kernel-journal-bad-sector` — existing repro still passes.
