# Migrate exclusive op preflight to sysfs + add --enqueue

## Context

braid's preflight check (`check_no_exclusive_op`) shells out to `btrfs balance status` and parses text output. This only detects balance operations — not device add/remove/replace/resize. The btrfs kernel exposes all exclusive ops via `/sys/fs/btrfs/{fsid}/exclusive_operation` (what btrfs-progs itself reads internally). Migrating to sysfs gives full coverage and removes a parser dependency from the preflight path.

Additionally, all btrfs exclusive-op commands should pass `--enqueue` so that if something grabs the exclusive lock in the TOCTOU window between the preflight check and the btrfs command, the command waits instead of failing with a raw kernel error.

**Behavior:**
- `"none"` → proceed
- `"balance paused"` → hard error ("resume or cancel it") — paused balance never clears on its own, `--enqueue` would hang forever
- Any other active op → `eprintln!` "waiting for in-flight {op} to finish..." and proceed; the btrfs command's `--enqueue` blocks until the slot frees; user can Ctrl-C
- `lock` command (unmount, not an exclusive op): ANY active op → hard error

## Steps

### 1. Extend `Filesystem` trait with `read_to_string`

**File:** `cli/src/probe.rs`

Add to `Filesystem` trait:
```rust
fn read_to_string(&self, path: &str) -> Result<String, std::io::Error>;
```

`RealFilesystem` impl: `std::fs::read_to_string(path)`

### 2. Add `read_to_string` to every `MockFs`

Each test module has its own `MockFs`. Add a `files: HashMap<String, String>` field (or similar) and implement `read_to_string` to look up the path. Default to returning `io::ErrorKind::NotFound` for unknown paths.

**Files with MockFs:** `mount.rs`, `lock.rs`, `status.rs`, `enroll_key_file.rs`, `add.rs`, `recover.rs`, `unlock.rs`, `probe.rs`. Most can return a default/panic since they won't exercise the sysfs path — only the MockFs instances in files that call the new preflight need real sysfs file contents.

### 3. Add `ExclusiveOp` enum and sysfs-based check to `preflight.rs`

**File:** `cli/src/preflight.rs`

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveOp {
    None,
    Balance,
    BalancePaused,
    DeviceAdd,
    DeviceRemove,
    DeviceReplace,
    Resize,
    SwapActivate,
}
```

Add `ExclusiveOp::parse(s: &str) -> Option<ExclusiveOp>` matching the sysfs strings: `"none"`, `"balance"`, `"balance paused"`, `"device add"`, `"device remove"`, `"device replace"`, `"resize"`, `"swap activate"`. These strings follow `exclop_def[]` in `reference/btrfs-progs/common/utils.c:1186-1194`. Add a comment on the `DeviceRemove` arm noting that `btrfs-man5.rst` says "device delete" but the kernel writes "device remove" — so nobody "corrects" the parser to match the docs.

Add `impl fmt::Display for ExclusiveOp` for human-readable messages.

New error type:
```rust
#[derive(Debug, thiserror::Error)]
pub enum ExclusiveOpError {
    #[error("an exclusive operation is already running: {0}")]
    Busy(ExclusiveOp),
    #[error("cannot read exclusive operation status: {0}")]
    Read(std::io::Error),
    #[error("unrecognized exclusive operation: {0:?}")]
    Unrecognized(String),
}
```

Rewrite `check_no_exclusive_op`:
```rust
pub fn check_no_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
) -> Result<(), ExclusiveOpError> {
    let path = format!("/sys/fs/btrfs/{fsid}/exclusive_operation");
    let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
    let op = ExclusiveOp::parse(contents.trim())
        .ok_or_else(|| ExclusiveOpError::Unrecognized(contents.trim().to_owned()))?;
    match op {
        ExclusiveOp::None => Ok(()),
        _ => Err(ExclusiveOpError::Busy(op)),
    }
}
```

Remove the old `check_no_exclusive_op` that takes `CommandRunner + mount_point`.

### 4. Update call sites: add, remove, replace, remove_missing

At each site, replace the old call pattern:
```rust
preflight::check_no_exclusive_op(runner, config.mount_point().as_str())
    .map_err(XxxError::Validation)?;
```

With (using `pool.fsid` which is already available at all 4 sites):
```rust
let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
match preflight::check_no_exclusive_op(fs, fsid) {
    Ok(()) => {}
    Err(ExclusiveOpError::Busy(ExclusiveOp::BalancePaused)) => {
        return Err(XxxError::Validation(
            "a btrfs balance is paused. Resume or cancel it before proceeding.".into(),
        ));
    }
    Err(ExclusiveOpError::Busy(op)) => {
        eprintln!("  waiting for in-flight {op} to finish...");
    }
    Err(e) => return Err(XxxError::Validation(e.to_string())),
}
```

**Signature changes needed:**
- `cmd_remove` and `cmd_remove_missing` don't currently take `fs: &F`. Add `fs: &F` parameter to both, thread `&RealFilesystem` from `main.rs` (same pattern as add/replace/lock).

**Files:**
- `cli/src/add.rs:309` — `pool.fsid` available, already has `fs`
- `cli/src/remove.rs:58` — `pool.fsid` available, needs `fs` added
- `cli/src/replace.rs:78` — `pool.fsid` available, already has `fs`
- `cli/src/remove_missing.rs:88` — `pool.fsid` available, needs `fs` added
- `cli/src/main.rs` — thread `&RealFilesystem` to `cmd_remove` and `cmd_remove_missing`

### 5. Update call site: lock

**File:** `cli/src/lock.rs:82`

`lock.rs` doesn't currently call `probe_pool`, so has no `pool.fsid`. Add a `probe_pool` call (reusing the canonical FSID discovery path from `probe.rs:145-156`) rather than open-coding `btrfs filesystem show` parsing:

```rust
if pool_was_mounted {
    let pool = probe_pool(runner, mount_point.as_str())
        .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
    let fsid = pool.fsid.as_deref()
        .ok_or_else(|| LockError::Failed("mounted pool has no FSID".into()))?;

    match preflight::check_no_exclusive_op(fs, fsid) {
        Ok(()) => {}
        Err(ExclusiveOpError::Busy(op)) => {
            return Err(LockError::Failed(format!(
                "cannot lock: {op} is in progress. Wait for it to finish first."
            )));
        }
        Err(e) => return Err(LockError::Failed(e.to_string())),
    }
}
```

Lock always hard-errors on ANY busy state (including running balance, not just paused). Unmounting during an exclusive op is unsafe.

### 6. Add `--enqueue` to all exclusive CmdRequest variants

**File:** `cli/src/cmd.rs`

Add `"--enqueue".into()` to the args vec of these 7 variants:

| Variant | Insert position |
|---|---|
| `BtrfsDeviceAdd` | after `"add".into()` |
| `BtrfsDeviceRemove` | after `"remove".into()` |
| `BtrfsReplaceStart` | after `"start".into()` (alongside `-r`, `-f`, `-B`) |
| `BtrfsBalanceRaid1` | after `"start".into()` |
| `BtrfsBalanceRaid1Soft` | after `"start".into()` |
| `BtrfsBalanceSingle` | after `"start".into()` |
| `BtrfsFilesystemResize` | after `"resize".into()` |

### 7. Update tests

**preflight.rs tests:** Rewrite all 4 existing tests to use MockFs with sysfs file contents instead of MockRunner with BtrfsBalanceStatus output. Add new tests for:
- Each ExclusiveOp variant parsed correctly
- `Display` impl
- Unrecognized value → error
- Read failure → error

**cmd.rs tests:** Update existing argv assertion tests that check exact arg lists (e.g., `btrfs_replace_start_includes_read_from_mirrors_flag`, `to_shell_string_simple_args`) to include `--enqueue`.

**Call site tests (add, remove, replace, remove_missing, lock):** Each test's MockRunner currently seeds `BtrfsBalanceStatus` for the old preflight. Replace with MockFs seeding `/sys/fs/btrfs/{fsid}/exclusive_operation` → `"none\n"`. For lock.rs tests, also seed `probe_pool` mocks (BtrfsFilesystemShow, FindmntJson, BtrfsBalanceStatus) in the MockRunner for fsid resolution.

**Caller-policy tests (new):** Add tests that verify the three distinct policy branches introduced by this change. At minimum:

1. **Mutating command + paused balance → hard error:** Seed sysfs file with `"balance paused\n"` in one of add/remove/replace/remove_missing. Assert the command returns a `Validation` error mentioning "paused".

2. **Mutating command + active op → warn and proceed:** Seed sysfs file with `"balance\n"` (or `"device remove\n"`). Assert the command does NOT error — it prints the wait message to stderr and continues to the btrfs command (which would have `--enqueue`).

3. **Lock + any active op → hard error:** Seed sysfs file with `"balance\n"` in lock. Assert `LockError::Failed` is returned. This confirms lock never waits — it always refuses.

**Do NOT remove:** `BtrfsBalanceStatus` CmdRequest, `parse_btrfs_balance_status`, `BalanceState` — these are still used by progress monitoring (`progress.rs`), TUI (`browse/model.rs`), status (`status.rs`), and idle detection (`idle.rs`).

### 8. VM test: active exclusive op → braid waits then succeeds

**New files:** `tests/cli/braid-add-during-balance.nix` + `tests/cli/braid-add-during-balance.py`
**Register in:** `flake.nix` checksFor block (same pattern as `braid-status-during-balance`)

**.nix config:** 3 disks at 4096 MiB (need enough data for balance to take measurable time), 2048 MiB RAM. Packages: braid, cryptsetup, btrfs-progs.

**.py test script — scenario:**
1. Create 2-disk RAID1 pool via `braid add disk1`, `braid add disk2`
2. Write ~512 MiB test data so balance has real work
3. Start a balance in the background (`btrfs balance start -dconvert=single -mconvert=dup -f /mnt/storage &`)
4. **Synchronize on observed busy state:** poll `btrfs balance status` in a tight loop until it reports "running" (same approach as `braid-status-during-balance.py`'s start+pause pattern — drive decisions from observed kernel state, not timing assumptions). Only proceed once the balance is confirmed active.
5. Run `braid add disk3` (which hits the active balance via sysfs check)
6. Assert: stderr contains "waiting for in-flight" message
7. Assert: command eventually succeeds (exit 0) — `--enqueue` waited for balance to finish, then the add proceeded
8. Assert: disk3 is in the pool (`btrfs fi show` contains `braid-disk3`)

**Why this scenario works:** the synchronization loop guarantees `braid add` runs while the balance is active. The balance on 512 MiB finishes in a few seconds, so `--enqueue` doesn't wait forever. The test proves the full end-to-end path: sysfs read → wait message → `--enqueue` blocks → balance finishes → add succeeds.

### 9. VM test: paused balance → braid fails fast, lock refuses

**New files:** `tests/cli/braid-exclop-paused-balance.nix` + `tests/cli/braid-exclop-paused-balance.py`
**Register in:** `flake.nix` checksFor block

**.nix config:** 3 disks at 4096 MiB, 2048 MiB RAM. Packages: braid, cryptsetup, btrfs-progs.

**.py test script — scenario:**
1. Create 2-disk RAID1 pool via `braid add disk1`, `braid add disk2`
2. Write ~512 MiB test data
3. Start balance and pause it — reuse the start+pause retry pattern from `braid-status-during-balance.py:56-102` (start balance in background, tight-loop `btrfs balance pause` until it sticks, retry with alternating conversion targets if balance completes before pause catches it)
4. Verify balance is paused: `btrfs balance status` contains "paused" with remaining work

5. **Subtest: mutating command fails fast on paused balance**
   - Run `braid add disk3` — expect failure
   - Assert: stderr contains "paused" message
   - Assert: exit code ≠ 0

6. **Subtest: lock refuses on paused balance**
   - Run `braid lock` — expect failure
   - Assert: stderr contains "in progress" message
   - Assert: pool is still mounted (`mountpoint -q /mnt/storage`)
   - Assert: LUKS mappers still open

7. Clean up: cancel the paused balance

## Verification

1. `just test-rust` — all unit tests pass
2. `just test-parsers` — parser canary still passes (no parser changes)
3. `just test braid-add-during-balance braid-exclop-paused-balance` — new VM tests pass
4. `just test` — full VM suite passes
