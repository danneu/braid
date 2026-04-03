# Fix: discover.rs isLuks gate checks .is_err() instead of exit status

## Context

`discover_from_dir` in `cli/src/discover.rs` uses `cryptsetup isLuks` to filter
non-LUKS devices. But `RealRunner::exec` returns `Ok(RawCommandOutput)` for any
process that runs — even with non-zero exit. `.is_err()` only catches execution
failures (binary not found, etc.), so the isLuks gate is a no-op in production.

Non-LUKS devices leak through to the `luksDump` call, where they're silently
dropped by the parser. The net effect is harmless, but the code doesn't implement
its stated intent, and the test mock (`LabelMap`) is bug-compatible — it returns
`Err` for non-LUKS devices instead of `Ok` with non-zero exit status.

## Files to modify

- `cli/src/discover.rs`

## Changes

### Step 1: Make `LabelMap` realistic for both commands

Update `LabelMap` (`cli/src/discover.rs:167`) so unknown devices return
`Ok(RawCommandOutput { exit_status: 1, .. })` — not `Err` — for **both**
`CryptsetupIsLuks` and `CryptsetupLuksDumpText`. This eliminates mock drift
across the board. Add a `call_log` field (`Mutex<Vec<(String, String)>>`) to
`LabelMap` that records each `(command, device)` pair, so tests can assert
which commands were called. `Mutex` (not `RefCell`) is required because
`CommandRunner: Sync`.

### Step 2: Write the red test

```
/*
 * Intent: the isLuks gate must prevent non-LUKS devices from reaching luksDump.
 * Why it exists: the gate checked .is_err() instead of exit status, making it
 *   a no-op — non-LUKS devices leaked through to luksDump and were only caught
 *   downstream by the parser.
 * Scenario: a NAS has both LUKS-encrypted braid drives and a non-LUKS device
 *   (e.g. a USB stick) in /dev/disk/by-id/. Discovery should never call
 *   luksDump on the non-LUKS device.
 */
```

Test: temp dir with one LUKS device and one non-LUKS device. After
`discover_from_dir`, assert `CryptsetupLuksDumpText` was called only for the
LUKS device. With the realistic mock and the buggy `.is_err()` gate, the
non-LUKS device passes the gate → `luksDump` is called for it → test fails (red).

### Step 3: Fix the gate (green)

```rust
// Before (line 51-58):
if runner
    .run(&CmdRequest::CryptsetupIsLuks { device: path_str.clone() })
    .is_err()
{
    continue;
}

// After:
match runner.run(&CmdRequest::CryptsetupIsLuks { device: path_str.clone() }) {
    Ok(raw) if raw.exit_status != 0 => continue,
    Err(_) => continue,
    _ => {}
}
```

### Step 4: Confirm existing tests pass

The `LabelMap` mock change from step 1 now returns realistic exit-status values.
With the fixed gate from step 3, all existing tests continue to pass.

## Verification

```
just test-rust
```
