# Plan: Replace disk-map.json with enriched pool.json and add a pending-operation journal

## Context

braid currently maintains two state files with overlapping concerns:

- **`pool.json`** — authoritative membership (`name → by_id` only)
- **`disk-map.json`** — advisory metadata (`name → { by_id, luks_uuid, devid, added_at }`)

This split creates problems: `remove-missing` must load disk-map.json to resolve a devid back to a membership name, making the "advisory" file load-bearing. The pre-commit persist pattern (write pool.json before disk ops) leaves stale entries on failure — two WIP plans (`composed-popping-pond.md`, `robust-booping-island.md`) patch symptoms of this.

This plan eliminates disk-map.json by enriching pool.json with the metadata fields, adds a pending-operation journal for crash-safe post-commit persist, and extracts the `braid-<name>` mapper convention into a single derivation point.

## Summary of changes

1. **Enriched pool.json** — `DiskMember` struct replaces bare `ByIdPath`, carrying `luks_uuid`, `devid`, `added_at` alongside `by_id`. Delete `disk_map.rs` entirely.
2. **Pending-operation journal** — `/var/lib/braid/pending-op.json` records intent + pre/target membership snapshots before mutations. All mutations become post-commit persist. When journal exists, braid enters **recovery mode**: only `status` and `recover` are permitted. `braid recover` rebuilds membership from the live mounted pool — not from LUKS label scanning.
3. **Mapper name extraction** — add `name_from_mapper()` next to existing `mapper_name()`, replace all scattered `strip_prefix("braid-")` calls.

Supersedes: `composed-popping-pond.md` (add post-commit), `robust-booping-island.md` (replace rollback). Post-commit persist with journal eliminates both problems those plans address.

---

## Phase A: Mapper name extraction

### `cli/src/config.rs` — add reverse derivation

Add adjacent to `mapper_name()` (line 33):

```rust
/// Extract the disk name from a mapper name, if it has the braid- prefix.
pub fn name_from_mapper(mapper: &str) -> Option<&str> {
    mapper.strip_prefix("braid-")
}
```

### Replace all scattered `strip_prefix("braid-")` calls

| File | Line | Current | Replacement |
|---|---|---|---|
| `status.rs` | 166 | `.strip_prefix("braid-").unwrap_or(...)` | `config::name_from_mapper(&pd.mapper.0).unwrap_or(...)` |
| `status.rs` | 696 | `.strip_prefix("braid-").unwrap_or(...)` | `config::name_from_mapper(&pd.mapper.0).unwrap_or(...)` |
| `unlock.rs` | 216 | `dev.mapper.0.strip_prefix("braid-").unwrap_or(...)` | `config::name_from_mapper(&dev.mapper.0).unwrap_or(...)` |
| `discover.rs` | 64 | `label.strip_prefix("braid-")` | `config::name_from_mapper(&label)` |
| `tui/probe.rs` | 52 | `.strip_prefix("braid-")` | `config::name_from_mapper(&d.mapper.0)` |
| `tui/probe.rs` | 120 | `child.name.strip_prefix("braid-")` | `config::name_from_mapper(&child.name)` |

**Not changed:** `probe.rs:146` strips `/dev/mapper/` to get the full mapper name (e.g., `braid-toshiba`), not the disk name. Different operation.

### Tests

```rust
#[test]
fn name_from_mapper_strips_prefix() {
    assert_eq!(name_from_mapper("braid-toshiba"), Some("toshiba"));
    assert_eq!(name_from_mapper("braid-ironwolf"), Some("ironwolf"));
}

#[test]
fn name_from_mapper_returns_none_for_non_braid() {
    assert_eq!(name_from_mapper("luks-something"), None);
    assert_eq!(name_from_mapper(""), None);
}
```

---

## Phase B: Enriched pool.json

### `cli/src/membership.rs` — new `DiskMember` type

Replace `PoolMembership { disks: BTreeMap<String, ByIdPath> }` with:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolMembership {
    pub disks: BTreeMap<String, DiskMember>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskMember {
    pub by_id: ByIdPath,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub luks_uuid: Option<LuksUuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub devid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
}
```

Convenience constructors:

```rust
impl DiskMember {
    /// Minimal member — used by discover, initial journal write.
    pub fn from_by_id(by_id: ByIdPath) -> Self {
        DiskMember { by_id, luks_uuid: None, devid: None, added_at: None }
    }

    /// Fully enriched — used after disk operations succeed.
    pub fn enriched(by_id: ByIdPath, luks_uuid: LuksUuid, devid: u64) -> Self {
        DiskMember { by_id, luks_uuid: Some(luks_uuid), devid: Some(devid), added_at: Some(now_iso()) }
    }
}
```

Move `now_iso()` from `disk_map.rs` to `membership.rs`.

### New pool.json format

```json
{
  "disks": {
    "toshiba": {
      "by_id": "/dev/disk/by-id/ata-TOSHIBA_...",
      "luks_uuid": "aaaa-bbbb-cccc-dddd",
      "devid": 1,
      "added_at": "2026-03-27T12:00:00Z"
    }
  }
}
```

Optional fields omitted when `None` (e.g., after `discover --write`).

### `refresh_pool_metadata()` — replaces all `update_disk_map_best_effort` calls

```rust
/// Enrich pool.json with metadata from the live pool state.
/// Best-effort: logs warning on failure, never fails the caller.
pub fn refresh_pool_metadata(pool: &PoolState, paths: &StatePaths) {
    let mut membership = match load_membership(paths) {
        Ok(m) => m,
        Err(e) => { eprintln!("Warning: failed to load membership for metadata refresh: {e}"); return; }
    };
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else { continue };
        if let Some(member) = membership.disks.get_mut(name) {
            member.luks_uuid = Some(dev.luks_uuid.clone());
            member.devid = Some(dev.devid);
            if member.added_at.is_none() {
                member.added_at = Some(now_iso());
            }
        }
    }
    if let Err(e) = save_membership(&membership, paths) {
        eprintln!("Warning: failed to save enriched membership: {e}");
    }
}
```

Uses `name_from_mapper` from Phase A — this is why the extraction matters.

### Impact on each consumer of disk-map

| File | Current disk-map usage | Replacement |
|---|---|---|
| `unlock.rs:213-224` | `update_disk_map_best_effort` records each device | `refresh_pool_metadata(&pool_after, paths)` |
| `add.rs:514-524` | `update_disk_map_best_effort` records new devices | Handled by post-commit pool.json write (Phase C) |
| `replace.rs:329-338` | `update_disk_map_best_effort` records new device | Handled by post-commit pool.json write (Phase C) |
| `remove.rs:168-170` | `update_disk_map_best_effort` removes entry | Delete — entry already removed from membership |
| `remove_missing.rs:152-157` | Loads disk-map to resolve devid→name | Read `member.devid` from enriched pool.json directly |
| `remove_missing.rs:181-184` | Removes disk-map entry by devid | Delete — handled by membership removal above |

### `validate_no_conflicts` update

All access to membership disk values changes from `ByIdPath` to `DiskMember`. Update `.0` references to `.by_id.0` throughout `membership.rs` and all callers that destructure membership entries.

### `discover --write` update (`main.rs`)

Convert `BTreeMap<String, ByIdPath>` from `discover_pool_members()` to `DiskMember::from_by_id()` at the call site. The discover function's return type stays the same.

### Delete

- `cli/src/disk_map.rs` — entire file
- `cli/src/lib.rs` — remove `pub mod disk_map;`
- `cli/src/state_paths.rs` — remove `disk_map_json()` method + test assertion
- Remove `use crate::disk_map;` from: `add.rs`, `remove.rs`, `remove_missing.rs`, `replace.rs`, `unlock.rs`

---

## Phase C: Pending-operation journal

### New file: `cli/src/journal.rs`

```rust
/// A pending-operation journal records the full context of a mutation in progress.
/// When this file exists, braid enters recovery mode: only `status` and `recover`
/// are permitted. All other commands (unlock, add, remove, replace, etc.) hard-fail.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Journal {
    pub started_at: String,
    pub op: OpKind,
    /// Snapshot of pool.json at journal write time — known-good state before the mutation.
    pub pre_membership: PoolMembership,
    /// What pool.json should become if the mutation succeeds.
    pub target_membership: PoolMembership,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op")]
pub enum OpKind {
    Add { disks: BTreeMap<String, ByIdPath> },
    Remove { name: String },
    RemoveMissing { devid: Option<u64> },
    Replace { old_name: String, new_name: String, new_by_id: ByIdPath },
}

pub fn write_journal(paths: &StatePaths, journal: &Journal) -> Result<(), JournalError>;
pub fn load_journal(paths: &StatePaths) -> Result<Option<Journal>, JournalError>;
pub fn clear_journal(paths: &StatePaths) -> Result<(), JournalError>;
```

`journal.rs` is **data access only** — load, save, clear. No command-gating logic.

The `pre_membership` and `target_membership` snapshots serve two purposes:
1. **Recovery disambiguation**: `braid recover` can compare live pool state against both snapshots to determine what actually happened (mutation completed, partially completed, or never started).
2. **Safe unlock during recovery**: `braid recover` uses the union of devices from both snapshots to attempt LUKS open, covering any post-mutation state.

The journal is **only cleared by**:
- The mutation command itself, after successful pool.json write (normal completion)
- `braid recover`, after successful reconciliation from live pool state

### `cli/src/preflight.rs` — add journal guard

The journal guard lives in `preflight.rs`, the established home for reusable command-entry gates (alongside `check_no_exclusive_op`, `check_not_read_only`, `check_no_missing_devices`):

```rust
/// Refuse if a pending-operation journal exists.
/// When the journal is present, pool.json may be inconsistent — only
/// `status` and `recover` are safe to run.
pub fn check_no_pending_operation(paths: &StatePaths) -> Result<(), String> {
    match journal::load_journal(paths) {
        Ok(Some(j)) => Err(format!(
            "interrupted operation detected (pending-op.json exists, started {}).\n\
             Pool membership may be inconsistent. Run 'braid recover' to reconcile \
             from live pool state, or 'braid status' to inspect.",
            j.started_at
        )),
        Ok(None) => Ok(()),
        Err(e) => Err(format!(
            "cannot read pending-op.json: {e}. Remove it manually or run 'braid recover'."
        )),
    }
}
```

This follows the `preflight.rs` pattern: returns `Result<(), String>`, fail-closed on read errors.

### `state_paths.rs` — add `pending_op_json()`

```rust
pub fn pending_op_json(&self) -> PathBuf {
    self.root.join("pending-op.json")
}
```

### `lib.rs` — add `pub mod journal;`

### Recovery mode: hard-fail when journal exists

When `pending-op.json` exists, braid enters **recovery mode**. All commands except `status`, `recover`, and `lock` hard-fail immediately via the shared preflight guard:

```rust
// Added to the top of every blocked command entry point
preflight::check_no_pending_operation(paths)
    .map_err(|e| SomeCommandError::Validation(e))?;
```

This follows the existing pattern — `check_no_exclusive_op`, `check_not_read_only`, etc. are already called at command entry points in `preflight.rs`.

**Allowed in recovery mode:**
- `braid status` — read-only, displays journal contents (op type, started_at, pre vs target membership diff)
- `braid recover` — reconciles from live pool state, clears journal (see Phase C.2)
- `braid lock` — safe containment action. If the pool is mounted when recovery mode is entered, the operator must be able to unmount and close LUKS devices. Blocking lock would prevent the safest immediate response.

**Blocked in recovery mode:** `unlock`, `add`, `remove`, `remove-missing`, `replace`, `discover`

### New universal mutation flow

```
1. Validate + check_no_pending_operation (preflight)
2. Build journal (snapshot pre_membership, compute target_membership)
3. Write pending-op.json (atomic) — records intent + both membership snapshots
4. Perform irreversible disk operations
5. Write pool.json (atomic) — target_membership + enriched fields
6. Delete pending-op.json
```

Pool.json is **never written before disk ops succeed**. The journal records intent and both membership states for crash recovery. If any step between 3 and 5 crashes, the journal triggers recovery mode on next invocation.

### Impact on each mutation command

#### `add.rs`

**Current:** validate → pre-commit pool.json → LUKS phase → pool phase → disk-map update

**New:**
```
validate + check_no_pending_operation
  → snapshot pre_membership, compute target_membership (pre + new disks)
  → write_journal(Add, pre, target) → LUKS phase → pool phase
  → probe pool → build enriched target_membership → save_membership → clear_journal
```

- Delete pre-commit persist block (lines 339-344)
- Delete `disk_map::update_disk_map_best_effort` block (lines 514-524)
- Before LUKS phase: build journal with `pre_membership = load_membership()` and `target_membership` = pre + new `DiskMember::from_by_id(...)` entries
- After pool phase: probe pool, enrich target_membership entries with luks_uuid/devid from probe, save, clear journal

#### `remove.rs`

**Current:** validate → pre-commit pool.json (remove entry) → evict device → disk-map update

**New:**
```
validate + check_no_pending_operation
  → snapshot pre_membership, compute target_membership (pre minus name)
  → write_journal(Remove, pre, target) → evict device
  → save target_membership → clear_journal
```

- Replace pre-commit removal (lines 158-162) with journal write
- Delete disk-map update block (lines 168-170)

#### `remove_missing.rs`

**Current:** validate → load disk-map for devid→name → pre-commit pool.json → btrfs remove → disk-map update

**New:**
```
validate + check_no_pending_operation
  → resolve devid→name from enriched pool.json (member.devid field)
  → snapshot pre_membership, compute target_membership (pre minus resolved name)
  → write_journal(RemoveMissing, pre, target) → btrfs remove
  → save target_membership → clear_journal
```

- Delete disk-map load + devid→name resolution (lines 150-169) — devid→name resolution now reads from enriched `devid` field in pool.json, done BEFORE journal write
- Delete disk-map update block (lines 181-184)

#### `replace.rs`

**Current:** validate → pre-commit pool.json (swap entries) → LUKS init → btrfs replace → disk-map update

**New:**
```
validate + check_no_pending_operation
  → snapshot pre_membership, compute target_membership (pre minus old, plus new)
  → write_journal(Replace, pre, target) → LUKS init → btrfs replace (commit point)
  → probe pool → enrich target_membership → save_membership → clear_journal
```

- Replace pre-commit swap (lines 198-202) with journal write
- Delete disk-map update block (lines 329-338)
- On failure before commit point: journal remains, pool.json unchanged. **No rollback needed** — `braid recover` will probe the live pool and find pre_membership is still correct.

### Impact on read commands

#### `unlock.rs`

- Replace disk-map update block (lines 212-224) with `refresh_pool_metadata(&pool_after, paths)`
- Add `preflight::check_no_pending_operation(paths)` at start of `cmd_unlock`

#### `status.rs`

- If journal exists, display structured info: op type, started_at, which disks differ between pre and target membership

#### `discover` and `discover --write` (`main.rs`)

- Add `preflight::check_no_pending_operation(paths)` — blocked in recovery mode. Discovery scans LUKS labels, not live pool topology, so it cannot safely reconcile after an interrupted mutation.

### New command: `braid recover`

Rebuilds membership from the **live mounted pool**, not from LUKS label scanning. This is the only path out of recovery mode.

**Flow:**

1. Read journal (if absent, error: "nothing to recover")
2. If pool not mounted:
   - Collect devices from union of `journal.pre_membership` and `journal.target_membership`
   - Attempt LUKS open for all devices (using the union covers any post-mutation state)
   - Mount the pool
3. Probe live pool via `probe_pool()` — this is the source of truth
4. Build new membership from live pool state:
   - For each `PoolDevice` in the probe result: derive disk name via `name_from_mapper(&dev.mapper.0)`, get by_id from the union of journal memberships, create `DiskMember::enriched(by_id, luks_uuid, devid)`
5. Write pool.json (atomic)
6. Clear journal
7. Report: show what changed vs pre_membership and target_membership

**Why live pool, not LUKS labels:** `discover` scans `/dev/disk/by-id/*` for any device with a `braid-*` LUKS label. After an interrupted `add`, a disk may have been LUKS-formatted (with a braid label) but never joined the btrfs pool. `discover --write` would include it — wrong. `recover` probes the actual btrfs pool topology, which only includes devices that btrfs knows about.

**Implementation:** New subcommand in `main.rs`, new file `cli/src/recover.rs`. The unlock logic (LUKS open + mount) can reuse existing helpers from `unlock.rs`.

### Journal crash recovery matrix

| Crash point | Journal | pool.json | Recovery |
|---|---|---|---|
| Before journal write | absent | unchanged | Clean — nothing happened |
| After journal, before disk ops | present | unchanged | `braid recover` probes pool, finds it matches pre_membership, writes pre_membership, clears journal |
| After disk ops, before pool.json write | present | stale | `braid recover` probes pool, builds correct membership from live state, writes it, clears journal |
| After pool.json write, before journal clear | present | correct | `braid recover` probes pool, confirms it matches pool.json, clears journal |
| After journal clear | absent | correct | Clean |

### Why this supersedes the two WIP plans

**`composed-popping-pond.md`** (move add persist to post-commit): Fully implemented here — all mutations use post-commit persist, not just add. The journal provides crash safety.

**`robust-booping-island.md`** (rollback on failed replace): Unnecessary — pool.json is never modified before the commit point. A failed replace leaves pool.json unchanged; there is nothing to roll back. The `do_replace_disk_ops` helper, `RollbackFailed` variant, and rollback guard are all eliminated.

---

## Phase D: Tests and docs

### Rust unit tests

- **`membership.rs`** — update all tests to use `DiskMember` instead of bare `ByIdPath`. Add: `DiskMember` serde roundtrip (with/without optional fields), `refresh_pool_metadata` enriches entries, `refresh_pool_metadata` handles missing membership gracefully.
- **`journal.rs`** — new: write/load roundtrip for each variant, clear removes file, clear missing file is ok, load missing returns None, load corrupt returns error.
- **`preflight.rs`** — add: `check_no_pending_operation` returns Ok when journal absent and Err when present; returns Err on read failure (fail-closed).
- **`config.rs`** — add `name_from_mapper` tests (Phase A).
- **`state_paths.rs`** — remove `disk_map_json` assertion, add `pending_op_json`.
- **Command tests** (`add.rs`, `remove.rs`, `remove_missing.rs`, `replace.rs`) — update `PoolMembership` construction to use `DiskMember`.

### NixOS VM tests

13 test files reference `disk-map.json`:
- `tests/cli/braid-unlock.py`
- `tests/cli/braid-remove-disk.py`
- `tests/cli/config-name-immutability.py`
- `tests/cli/replace-*.py` (9 files)
- `tests/hw/test_replace_live_canary.py`

For each: replace `disk-map.json` assertions with pool.json assertions checking enriched fields.

### New VM tests

1. **`add-post-commit-persist`** — failed add does NOT leave stale pool.json entry; journal exists after failure; all commands except `status` and `recover` are blocked.
2. **`replace-post-commit-persist`** — failed replace (undersized disk) leaves pool.json unchanged; no rollback needed; journal triggers recovery mode.
3. **`journal-recovery`** — simulates interrupted add (LUKS formatted but pool.json not updated); `braid unlock` hard-fails; `braid recover` probes live pool, rebuilds correct membership, clears journal; subsequent `braid unlock` succeeds.
4. **`journal-blocks-commands`** — with journal present: verify `unlock`, `add`, `remove`, `replace`, `discover` all hard-fail; verify `status`, `recover`, and `lock` proceed.

### Doc updates

| File | Change |
|---|---|
| `docs/principles.md:16` | "Pre-commit persist" → "Post-commit persist with journal": intent journaled, membership written only after success, recovery via `braid recover` from live pool state |
| `docs/decisions/017-runtime-disk-membership.md:27-33` | Remove disk-map.json reference. Update mutation ordering. Add recovery mode section: journal present → hard-fail, `braid recover` rebuilds from live pool |
| `docs/decisions/012-intent-cli.md` | Update replace safety constraints: post-commit eliminates rollback need |
| `plans/wip/composed-popping-pond.md` | Mark: Superseded by this plan |
| `plans/wip/robust-booping-island.md` | Mark: Superseded by this plan |

---

## Files summary

**New:** `cli/src/journal.rs`, `cli/src/recover.rs`

**Delete:** `cli/src/disk_map.rs`

**Modify:**

| File | Phase | Change |
|---|---|---|
| `cli/src/config.rs` | A | Add `name_from_mapper()` |
| `cli/src/status.rs` | A+C | Use `name_from_mapper` (2 sites), display journal info in recovery mode |
| `cli/src/preflight.rs` | C | Add `check_no_pending_operation()` journal guard |
| `cli/src/unlock.rs` | A+C | Use `name_from_mapper`, replace disk-map update with `refresh_pool_metadata`, add `check_no_pending_operation` |
| `cli/src/discover.rs` | A+C | Use `name_from_mapper`, add `check_no_pending_operation` |
| `cli/src/tui/probe.rs` | A | Use `name_from_mapper` (2 sites) |
| `cli/src/membership.rs` | B | `DiskMember`, `refresh_pool_metadata()`, `now_iso()` |
| `cli/src/state_paths.rs` | B+C | Remove `disk_map_json()`, add `pending_op_json()` |
| `cli/src/lib.rs` | B+C | Remove `disk_map`, add `journal`, add `recover` |
| `cli/src/add.rs` | C | Journal-based post-commit persist, `check_no_pending_operation` |
| `cli/src/remove.rs` | C | Journal-based post-commit persist, `check_no_pending_operation` |
| `cli/src/remove_missing.rs` | C | Journal-based, devid lookup from enriched pool.json, `check_no_pending_operation` |
| `cli/src/replace.rs` | C | Journal-based post-commit persist, no rollback, `check_no_pending_operation` |
| `cli/src/main.rs` | B+C | Update discover --write for `DiskMember`, add `recover` subcommand, `check_no_pending_operation` on discover |
| 13 VM test `.py` files | D | Replace disk-map.json assertions with enriched pool.json assertions |

## Verification

1. After Phase A: `just test-rust` — pure refactor, no behavior change
2. After Phase B: `just test-rust` — enriched types, disk_map.rs deleted
3. After Phase C: `just test-rust` — journal + post-commit persist + recovery mode
4. After Phase D: `just test` — full VM test suite
5. Smoke: `braid add disk1=... disk2=...` → pool.json has enriched fields → `braid unlock` → `refresh_pool_metadata` populates any missing fields → `braid lock` → `braid unlock` round-trip
6. Failure: simulate failed add → pool.json unchanged, pending-op.json present → `braid unlock` hard-fails → `braid recover` probes live pool, writes correct membership, clears journal → `braid unlock` succeeds
7. Recovery mode: with journal present → verify `unlock`, `add`, `remove`, `replace`, `lock` all hard-fail → `braid status` shows journal info → `braid recover` reconciles → all commands work again
8. Crash: kill braid mid-replace after commit point → journal present, pool.json stale → `braid recover` probes live pool (sees new device), rebuilds membership, clears journal
