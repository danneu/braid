# Plan: Close orphaned braid-* mappers during `braid lock`

## Context

If a crash occurs after `cryptsetup open` but before journal/pool.json is written, a `braid-*` mapper exists that isn't in pool.json. `braid lock` only iterates `membership.disks.keys()`, so it won't close the orphan. A supplementary scan of `/dev/mapper/` for `braid-*` entries closes this gap.

## Changes

### 1. Add `list_dir` as a required method on `Filesystem` trait (`cli/src/probe.rs`)

```rust
fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
```

Required (not default) so every MockFs must consciously decide what to return — no silent stub hiding missing coverage.

Implement on `RealFilesystem`: use `std::fs::read_dir`, return entry names. Treat `NotFound` as empty vec (graceful in containers without device-mapper).

### 2. Update all 6 MockFs implementations

Each must add `list_dir`. Five of them (probe.rs, unlock.rs, status.rs, enroll_key_file.rs, add.rs) return `Ok(vec![])` since they don't exercise lock's orphan scan. The lock.rs MockFs derives entries from its `paths` vec.

Files:
- `cli/src/probe.rs:218`
- `cli/src/unlock.rs:257`
- `cli/src/status.rs:1038`
- `cli/src/enroll_key_file.rs:272`
- `cli/src/add.rs:778`
- `cli/src/lock.rs:155`

### 3. Add orphan scan to `cmd_lock` (`cli/src/lock.rs`)

After step 3 (membership loop, line 124), add step 3b:

- `fs.list_dir("/dev/mapper")` → filter for `braid-*` via `name_from_mapper` → skip names in `membership.disks` → close remaining with warning.
- Scan I/O failure: warn and continue (non-fatal).
- Orphan close failure: propagate error (fatal, same as membership mappers).

Add `name_from_mapper` to the import on line 5.

### 4. Update lock.rs `MockFs` to override `list_dir`

Derive entries from the existing `paths` vec — extract filenames under the requested directory prefix. Existing tests work unchanged since their paths already contain the right `/dev/mapper/braid-*` entries.

### 5. Add tests

Each test includes the required block comment (Intent / Why it exists / Scenario per test conventions).

- **`lock_closes_orphaned_mapper`**: membership has `aaa`+`bbb`, MockFs also has `/dev/mapper/braid-ccc`. Assert all three closed.
  - Intent: orphaned braid-* mappers from prior crashes are cleaned up during lock.
  - Why: crash between cryptsetup open and journal write leaves mapper outside pool.json.
  - Scenario: power loss during `braid add` after LUKS open but before pool.json write; next `braid lock` must still close the orphan.

- **`lock_orphan_scan_failure_is_nonfatal`**: custom MockFs returns `Err` from `list_dir`. Assert lock still succeeds for known mappers.
  - Intent: I/O errors scanning /dev/mapper don't prevent closing known mappers.
  - Why: /dev/mapper may be unreadable in degraded environments; the safety-net scan shouldn't break the primary lock path.
  - Scenario: containerized environment where /dev/mapper has restricted permissions.

- **`lock_orphan_close_failure_is_fatal`**: orphan mapper exists but `cryptsetup close` fails with a non-busy error. Assert `cmd_lock` returns an error.
  - Intent: if an orphan mapper is detected but can't be closed, lock must fail rather than silently leaving LUKS open.
  - Why: a stray open LUKS mapper is a security concern — reporting success while leaving it open is worse than failing.
  - Scenario: orphan mapper is held open by a leaked process; lock must surface the failure.

## Files to modify

- `cli/src/probe.rs` — trait method + RealFilesystem impl
- `cli/src/lock.rs` — orphan scan logic, MockFs update, new tests
- `cli/src/unlock.rs` — MockFs: add `list_dir` returning `Ok(vec![])`
- `cli/src/status.rs` — MockFs: add `list_dir` returning `Ok(vec![])`
- `cli/src/enroll_key_file.rs` — MockFs: add `list_dir` returning `Ok(vec![])`
- `cli/src/add.rs` — MockFs: add `list_dir` returning `Ok(vec![])`

## Verification

`just test-rust` — all existing + new unit tests pass.
