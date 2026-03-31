# Fix: recover.rs should error on unknown by_id, not fabricate one

## Context

`cli/src/recover.rs:71-79` — when rebuilding membership from a live pool, if a device exists in neither the pre nor target membership snapshot, the code fabricates a `by_id` of `"unknown-{mapper}"`. This isn't a valid `/dev/disk/by-id/` path and would produce a corrupt `pool.json` entry. Recovery is a safety-critical path and must not silently write invalid state.

## Approach (TDD)

### Step 1: Write failing test in `cli/src/recover.rs`

Add a `#[cfg(test)] mod tests` block with a test `recover_fails_when_device_missing_from_both_snapshots` that:

1. Creates a `tempfile::TempDir` + `StatePaths::custom(tmp.path().into())`
2. Builds a `Journal` where:
   - `pre_membership` has one disk `"toshiba"`
   - `target_membership` has the same disk `"toshiba"`
   - `op` is `OpKind::Add` with a second disk `"mystery"`
3. Writes the journal via `journal::write_journal(&paths, &journal)`
4. Sets up a `MockRunner` so `probe_pool` returns a `PoolState` with two devices: `braid-toshiba` (known) and `braid-mystery` (absent from both snapshots)
5. Calls `cmd_recover(&runner, &config, &paths)`
6. Asserts:
   - Returns `Err` matching `RecoverError::Failed` with message containing `"braid-mystery"`
   - `pool.json` was **not** written (file doesn't exist)
   - `pending-op.json` was **not** cleared (file still exists)

MockRunner setup: seed `FindmntJson` and `BtrfsFilesystemShow` responses (from existing helpers in `probe.rs` tests — replicate the helper pattern), plus `CryptsetupStatus` and `CryptsetupLuksUuid` for both devices.

Config: `Config::new(MountPoint("/mnt/storage".into())).unwrap()`

### Step 2: Confirm test fails

Run `cargo test -p braid-cli recover` — expect failure because current code writes `"unknown-braid-mystery"` instead of erroring.

### Step 3: Fix `cli/src/recover.rs:71-79`

Replace `.unwrap_or_else(...)` with `.ok_or_else(...)? `:

```rust
let by_id = union
    .disks
    .get(name)
    .map(|m| m.by_id.clone())
    .ok_or_else(|| {
        RecoverError::Failed(format!(
            "device {} is in the live pool but has no by-id path in either \
             the pre-operation or target membership snapshot.\n\
             This must be resolved manually — provide the correct \
             /dev/disk/by-id/ path and re-run recovery.",
            dev.mapper.0
        ))
    })?;
```

### Step 4: Confirm test passes

Run `cargo test -p braid-cli recover`.

## Files to modify

- `cli/src/recover.rs` — add test module + change fallback to error

## Verification

```
cargo test -p braid-cli recover
```
