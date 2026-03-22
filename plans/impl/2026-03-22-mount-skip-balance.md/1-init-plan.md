# Plan: Add `skip_balance` mount option

## Context

btrfs silently resumes interrupted balance operations on mount (kernel default).
This is dangerous in braid because:
- A balance that hit ENOSPC will immediately fail again on resume
- The user may have locked the pool to stop IO-intensive work; silent resume defeats that
- Unlock performing an implicit balance resume violates Principle 3 (safe-by-construction)

Fix: mount with `skip_balance`, warn the user if a paused balance exists.

## Changes

### 1. Add `skip_balance` to mount option assembly

**File:** `cli/src/cmd.rs`

Keep both `Mount` and `MountWithOptions` variants. Factor a shared helper that
prepends the hardcoded options (`noatime`, `skip_balance`) in both `to_argv()`
arms:

```rust
/// Base mount options braid always applies.
fn base_mount_options() -> Vec<String> {
    vec!["noatime".to_owned(), "skip_balance".to_owned()]
}
```

**`Mount` arm (~line 411):**
```rust
CmdRequest::Mount { device, mount_point } => {
    let args = vec![
        "-o".into(),
        base_mount_options().join(","),
        device.clone(),
        mount_point.0.clone(),
    ];
    CmdArgs { program: "mount", args }
}
```

**`MountWithOptions` arm (~line 428):**
```rust
CmdRequest::MountWithOptions { device, mount_point, options } => {
    let mut all_options = base_mount_options();
    all_options.extend(options.iter().cloned());
    let args = vec![
        "-o".into(),
        all_options.join(","),
        device.clone(),
        mount_point.0.clone(),
    ];
    CmdArgs { program: "mount", args }
}
```

No enum changes, no caller changes in `unlock.rs` or `pool.rs`.

### 2. Post-mount balance warning in unlock

**File:** `cli/src/unlock.rs`, after the "mounted" eprintln (~line 240)

Best-effort check using existing `status::get_balance_report()` (already
`pub(crate)` in `status.rs:661`):

```rust
// Best-effort: warn if a paused balance was found on mount.
// skip_balance prevents the kernel from resuming it silently, but the user
// should know so they can resume or cancel explicitly.
match crate::status::get_balance_report(runner, mount_point.as_str()) {
    crate::status::BalanceReport::Paused { .. } => {
        eprintln!("{}  {:<10}paused balance detected — will not auto-resume", tag("warn"), "");
        eprintln!("           resume:  btrfs balance resume {mount_point}");
        eprintln!("           cancel:  btrfs balance cancel {mount_point}");
    }
    _ => {}
}
```

If the command fails, `get_balance_report` returns `Unknown` → caught by `_ =>`.
No exit code change — unlock succeeded.

### 3. fstab defense-in-depth

**File:** `modules/braid/storage.nix` (~line 31)

Add `"skip_balance"` after `"noatime"`:
```nix
"noatime"
# skip_balance: prevent kernel from silently resuming interrupted
# balance on mount. braid manages balance lifecycle explicitly.
"skip_balance"
```

### 4. Update VM test fixtures to mirror production mount options

All 10 `tests/module/*.nix` files override `virtualisation.fileSystems."/mnt/storage"`
with their own options list that diverges from production. Add `"noatime"` and
`"skip_balance"` to each so the test fixtures don't model stale mount behavior.

**Files (all under `tests/module/`):**
- `raid1.nix` (~line 65)
- `single-disk.nix` (~line 52)
- `degraded-raid1.nix` (~line 77)
- `no-silent-degraded.nix` (~line 91)
- `single-disk-dead.nix` (~line 55)
- `bad-config.nix` (~line 36)
- `add-bootstrap.nix` (~line 38)
- `auto-unlock-key-present.nix` (~line 105)
- `auto-unlock-key-wrong.nix` (~line 85)
- `auto-unlock-key-missing.nix` (~line 68)

Each gets `"noatime"` and `"skip_balance"` added to the existing options list.

### 5. VM integration test: verify skip_balance and paused-balance warning

Add a new subtest to `tests/cli/braid-unlock.py` (after setup, before teardown):

**Part A — Assert `skip_balance` appears in mounted options:**
```python
with subtest("skip_balance: mount options include skip_balance"):
    opts = machine.succeed("findmnt -o OPTIONS -n /mnt/storage").strip()
    assert "skip_balance" in opts, f"Expected skip_balance in mount options, got: {opts}"
```

This can run after any successful `braid unlock` (e.g. after Test 1 happy path).

**Part B — Assert paused balance survives unlock and warning is emitted:**

After the existing setup (3-disk pool with data):
```python
with subtest("skip_balance: paused balance survives unlock"):
    # Write enough data to create multiple chunks so balance takes time
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/balancedata bs=1M count=50")
    machine.succeed("sync")

    # Start a converting balance in background (nohup keeps it alive after
    # the shell exits, matching the pattern in braid-lock-umount-busy.py)
    machine.execute(
        "nohup btrfs balance start -dconvert=single /mnt/storage "
        "> /tmp/balance.log 2>&1 & echo $!"
    )

    # Wait until balance is actually running before pausing
    retry_count = 0
    while retry_count < 30:
        status = machine.execute("btrfs balance status /mnt/storage")
        if "running" in status[1].lower():
            break
        retry_count += 1
        import time; time.sleep(0.2)
    else:
        raise Exception("Balance never entered running state")

    machine.succeed("btrfs balance pause /mnt/storage")

    # Verify balance is paused
    status = machine.succeed("btrfs balance status /mnt/storage")
    assert "paused" in status.lower(), f"Expected paused balance, got: {status}"

    # Lock and re-unlock
    close_all()
    ret = machine.execute(unlock_cmd(passphrase) + " 2>&1")
    assert ret[0] == 0, f"Unlock failed: {ret[1]}"

    # Balance must still be paused (not resumed by kernel)
    status = machine.succeed("btrfs balance status /mnt/storage")
    assert "paused" in status.lower(), \
        f"Expected balance still paused after unlock, got: {status}"

    # Warning text must have been emitted
    assert "paused balance" in ret[1], \
        f"Expected paused balance warning, got: {ret[1]}"

    # Clean up: cancel the paused balance and remove test data
    machine.succeed("btrfs balance cancel /mnt/storage")
    machine.succeed("rm /mnt/storage/balancedata")
```

The test polls `btrfs balance status` until it reports "running", then pauses.
If the balance somehow never enters running state (e.g. completes instantly),
the test fails rather than silently skipping — this ensures the safety behavior
is always verified.

### 6. Update Rust unit tests

**`cli/src/cmd.rs` tests:**
- Add/update argv tests to verify `Mount` produces `-o noatime,skip_balance`
  and `MountWithOptions` with `["degraded"]` produces `-o noatime,skip_balance,degraded`.

**`cli/src/unlock.rs` tests:**
- All tests that mock `Mount` or `MountWithOptions` and reach mount:
  the mock expectations don't check argv directly (they match on the `CmdRequest`
  enum variant), so the `Mount`/`MountWithOptions` mocks remain valid.
  But any test that reaches mount now also calls `BtrfsBalanceStatus` — add a
  mock returning `"No balance found on '/mnt/storage'"` to each.
- New test `unlock_warns_on_paused_balance`: mock `BtrfsBalanceStatus` with
  paused output, assert `cmd_unlock` returns `Ok(())`.

### 7. Update docs

**`docs/principles.md`** — Principle 3 (Safe-by-construction): add that mounts
always include `skip_balance` to prevent hidden balance resumption.

**`README.md`** — "Pool unlock" section (~line 262): add a note that `unlock`
mounts with `skip_balance` so interrupted balances are never silently resumed,
and that a warning is printed if a paused balance is detected with instructions
to resume or cancel.

## Verification

1. `just test-rust` — all unit tests pass
2. `just test braid-unlock` — VM integration test passes (includes skip_balance assertions)
3. `just test no-silent-degraded` — module test passes with updated fixture

## Files touched

- `cli/src/cmd.rs` — `base_mount_options()` helper, both mount arms updated
- `cli/src/unlock.rs` — post-mount balance warning + test updates
- `modules/braid/storage.nix` — fstab option
- `tests/module/*.nix` (10 files) — add `noatime` + `skip_balance` to fixtures
- `tests/cli/braid-unlock.py` — skip_balance + paused-balance integration tests
- `README.md` — user guide Pool unlock section
- `docs/principles.md` — invariant docs
