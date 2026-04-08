# Plan: Require `--allow-degraded` for degraded mounts

## Context

When btrfs RAID1 mounts in degraded mode (missing device), all new block group
allocations silently use `single` profile — one copy, zero redundancy. This is
the #1 data-loss footgun in btrfs RAID1. Currently `braid unlock` auto-mounts
with `-o degraded` whenever disks are missing, giving users no opportunity to
understand the risk before proceeding.

The project's own research doc (`research/how-to-handle-degraded.md`)
recommended **Option A: refuse by default, require `--allow-degraded`** — but
the implementation never followed through. This change aligns the code with that
recommendation.

**Key design constraint:** `PresentNotLuks` covers both bricked former pool
members AND genuinely uninitialized disks. Only confirmed pool members (tracked
in the disk-map at `/var/lib/braid/disk-map.json`) are eligible for degraded
mount. Uninitialized disks remain a hard error regardless of `--allow-degraded`.

## Changes

### 1. CLI args — add `--allow-degraded` flag

**File:** `cli/src/main.rs`

Add to `UnlockArgs` (after line 138):
```rust
/// Allow mounting with missing devices (degraded mode — new writes have no redundancy)
#[arg(long)]
allow_degraded: bool,
```

Load the disk map and pass both new args at lines 295-316:
```rust
Commands::Unlock(args) => {
    let config = match config_read(Path::new(&config_path)) { ... };
    let disk_map = braid_cli::disk_map::load_disk_map();
    let runner = RealRunner;
    let fs = RealFilesystem;
    match braid_cli::unlock::cmd_unlock(
        &runner, &fs, &config, &disk_map,
        args.passphrase_stdin,
        args.passphrase_file.as_deref(),
        args.key_file.as_deref(),
        args.allow_degraded,
    ) {
        Ok(()) => {}
        Err(braid_cli::unlock::UnlockError::DegradedRefused(msg)) => {
            print_cli_error(&msg);
            std::process::exit(2);  // dedicated exit code for auto-unlock detection
        }
        Err(e) => {
            print_cli_error(&e.to_string());
            std::process::exit(1);
        }
    }
}
```

### 2. Unlock function — split pool-member vs uninitialized, gate degraded

**File:** `cli/src/unlock.rs`

Add error variant:
```rust
#[error("{0}")]
DegradedRefused(String),
```

Add parameters to `cmd_unlock` signature:
```rust
pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    disk_map: &DiskMap,      // new — for pool-member verification
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    key_file: Option<&std::path::Path>,
    allow_degraded: bool,    // new
) -> Result<(), UnlockError>
```

**Separate tracking** (replaces the single `any_not_luks` bool):

In the probe loop (lines 54-83), replace the `PresentNotLuks` and `Absent`
arms with disk-map aware logic:

```rust
ConfigDiskState::Absent => {
    if disk_map.disks.contains_key(name) {
        // Known pool member, confirmed missing → degradable
        eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), name);
        any_missing_member = true;
    } else {
        // Never added — config error, not a degraded scenario
        eprintln!(
            "{}  disk: {:<10}not found and never added to pool, run `braid add {}`",
            tag("skip"), name, name
        );
        any_uninitialized = true;
    }
}
ConfigDiskState::PresentNotLuks => {
    if disk_map.disks.contains_key(name) {
        // Was a pool member, LUKS header now bricked → degradable
        eprintln!(
            "{}  disk: {:<10}LUKS header damaged (was pool member)",
            tag("skip"), name
        );
        any_missing_member = true;
    } else {
        // Never was a pool member — genuinely uninitialized
        eprintln!(
            "{}  disk: {:<10}not initialized, run `braid add {}`",
            tag("skip"), name, name
        );
        any_uninitialized = true;
    }
}
```

**Gate the mount decision** (replaces lines 161-192):

```rust
// Uninitialized disks are always a hard error — not a degraded scenario
if any_uninitialized {
    return Err(UnlockError::Failed(
        "some disks are not initialized (run `braid add`)".into(),
    ));
}

if any_missing_member && !allow_degraded {
    return Err(UnlockError::DegradedRefused(
        "pool has missing devices — refusing to mount degraded\n\
         new writes would have ZERO redundancy (single-profile chunks)\n\
         hint: braid unlock --allow-degraded".into(),
    ));
}

let mount_result = if any_missing_member {
    // --allow-degraded was passed and we have confirmed missing pool members
    runner.run(&CmdRequest::MountWithOptions {
        // ... existing degraded mount code ...
        options: vec!["degraded".to_owned()],
    })?
} else {
    // All disks present — normal mount
    runner.run(&CmdRequest::Mount { ... })?
};
```

Note: the existing early-exit at lines 85-94 (no unlockable disks AND none
open) needs updating too. With the new `any_uninitialized` tracking, the
`any_not_luks` check there can become `any_uninitialized`.

### 3. NixOS option — `autoUnlock.allowDegraded`

**File:** `modules/braid/options.nix` (inside `autoUnlock` block, after line 61)

```nix
allowDegraded = lib.mkOption {
  type = lib.types.bool;
  default = false;
  description = "Mount degraded when devices are missing during auto-unlock. New writes will have zero redundancy.";
};
```

### 4. Auto-unlock service — detect degraded refusal explicitly

**File:** `modules/braid/storage.nix` (lines 170-174)

Replace the `if/else` with exit-code-aware handling. NixOS `script` attributes
run with `set -e`, so a bare `braid unlock ...` followed by `ret=$?` would
never reach the `ret=$?` on failure. The existing `if ... then ... else ... fi`
pattern suppresses errexit for the condition — keep that structure but capture
the exit code:

```nix
if braid unlock --key-file "$resolved"${lib.optionalString cfg.autoUnlock.allowDegraded " --allow-degraded"}; then
  echo "braid-auto-unlock: pool unlocked successfully" >&2
else
  ret=$?
  if [ $ret -eq 2 ]; then
    echo "braid-auto-unlock: pool has missing devices — not mounted" >&2
    echo "braid-auto-unlock: set braid.autoUnlock.allowDegraded = true to allow degraded mount" >&2
  else
    echo "braid-auto-unlock: unlock failed (exit $ret), skipping" >&2
  fi
fi
```

This ensures degraded refusal (exit 2) produces a dedicated, actionable log
message rather than being hidden behind "wrong keyfile?".

### 5. Manual unlock service — no change needed

`braid-unlock` (line 85) calls `braid unlock --passphrase-stdin` without
`--allow-degraded`. If the pool is degraded, the user sees the refusal error
via `systemd-ask-password` output and can manually run
`braid unlock --passphrase-stdin --allow-degraded`.

### 6. Unit tests

**File:** `cli/src/unlock.rs` tests

All `cmd_unlock` calls gain a `&disk_map` and `allow_degraded` parameter.

**Update `unlock_bricked_disk_uses_degraded_mount`** (line 277):
- Construct a `DiskMap` with disk3 present (it was a pool member)
- Pass `allow_degraded: true`
- Keep existing assertions (mount with `-o degraded` succeeds)

**Add `unlock_bricked_disk_refuses_without_flag`:**
- Same setup, disk3 in disk-map
- Pass `allow_degraded: false`
- Remove btrfs-device-scan and mount mocks (never reached)
- Assert `Err(UnlockError::DegradedRefused(_))`
- Assert message contains "refusing to mount degraded" and "--allow-degraded"

**Add `unlock_uninitialized_disk_hard_error_even_with_allow_degraded`:**
- PresentNotLuks disk NOT in disk-map
- Pass `allow_degraded: true`
- Assert `Err(UnlockError::Failed(_))` with "not initialized"
- Proves uninitialized disks can't be bypassed with the flag

**Update `passphrase_mismatch_names_failing_disk`** (line 398):
- Pass empty `DiskMap` and `allow_degraded: false` (all disks present, so
  degraded path isn't reached)

### 7. Integration tests

**`tests/cli/braid-unlock.py` — Test 4 (line 114):**

Split into three subtests:

**Test 4a: missing disk — refuses degraded by default**
```python
with subtest("Test 4a: missing disk — refuses degraded by default"):
    close_all()
    machine.succeed("rm -f /dev/disk/by-id/virtio-disk3")
    ret = machine.execute(unlock_cmd(passphrase) + " 2>&1")
    assert ret[0] != 0, "Expected non-zero exit for degraded refusal"
    assert "refusing to mount degraded" in ret[1]
    assert "--allow-degraded" in ret[1]
    machine.fail("mountpoint -q /mnt/storage")
```

**Test 4b: missing disk — `--allow-degraded` mounts degraded**
```python
with subtest("Test 4b: missing disk — --allow-degraded mounts degraded"):
    machine.succeed(unlock_cmd(passphrase, extra="--allow-degraded"))
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "persistent data"
    close_all()
    machine.succeed("udevadm trigger && udevadm settle")
    machine.succeed("test -e /dev/disk/by-id/virtio-disk3")
```

**Test 4c: uninitialized disk — hard error even with `--allow-degraded`**

This is a variant of the existing Test 6 but with `--allow-degraded` to prove
it doesn't bypass the uninitialized check. (Or just add an assertion to the
existing Test 6.)

**`tests/module/degraded-raid1.py` (line 5):**

Add `--allow-degraded`:
```python
machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin --allow-degraded")
```

**`tests/module/no-silent-degraded.py` (lines 30-34):**

Split into two subtests:

```python
with subtest("braid unlock refuses degraded mount without --allow-degraded"):
    ret = machine.execute(
        "echo -n 'testpassphrase' | braid unlock --passphrase-stdin 2>&1"
    )
    assert ret[0] != 0, "Expected refusal"
    assert "refusing to mount degraded" in ret[1]
    machine.fail("mountpoint -q /mnt/storage")

with subtest("braid unlock --allow-degraded mounts degraded"):
    machine.succeed(
        "echo -n 'testpassphrase' | braid unlock --passphrase-stdin --allow-degraded"
    )
    machine.succeed("mountpoint -q /mnt/storage")
```

### 8. Docs

**`docs/principles.md` line 7:**
> Data drives never block boot. LUKS devices use `nofail` + bounded timeouts.
> btrfs-device-scan uses `wants`, not `requires`. The mount uses `nofail`.
> Degraded mounts require explicit `--allow-degraded` — braid refuses to
> silently run with zero redundancy. [Why →](decisions/resilient-boot.md)

**`docs/decisions/resilient-boot.md`:**
- Update lines 15, 27: remove `degraded` from "everywhere" language
- Line 35: update "one drive dead" tier to say `braid unlock` refuses by
  default; user must pass `--allow-degraded` or configure
  `autoUnlock.allowDegraded`
- Add note about `autoUnlock.allowDegraded` for unattended use

**`README.md` line 224:**
> One passphrase prompt opens all available LUKS devices and mounts the pool.
> Works from TTY, SSH, or scripted. If disks are missing, use
> `--allow-degraded` to mount with reduced redundancy.

**`docs/decisions/disk-pool-management.md` line 66:**
Update reference to degraded mounting.

## Files modified (summary)

| File | Change |
|------|--------|
| `cli/src/main.rs` | Add `--allow-degraded` arg, exit code 2 for `DegradedRefused` |
| `cli/src/unlock.rs` | Add `disk_map` + `allow_degraded` params, split pool-member vs uninitialized, gate degraded mount |
| `modules/braid/options.nix` | Add `autoUnlock.allowDegraded` option |
| `modules/braid/storage.nix` | Exit-code-aware auto-unlock logging, pass `--allow-degraded` when configured |
| `tests/cli/braid-unlock.py` | Split Test 4 into 4a (refusal) + 4b (allow) |
| `tests/module/degraded-raid1.py` | Add `--allow-degraded` flag |
| `tests/module/no-silent-degraded.py` | Add refusal subtest, add `--allow-degraded` to existing |
| `cli/src/unlock.rs` (tests) | Update existing + add refusal + uninitialized tests |
| `docs/principles.md` | Update Principle 1 |
| `docs/decisions/resilient-boot.md` | Update degraded references |
| `README.md` | Update unlock and "resilient boot" descriptions |
| `docs/decisions/disk-pool-management.md` | Update degraded reference |

## Verification

1. `just test-rust` — unit tests: refusal, allow, uninitialized-still-errors
2. `just test no-silent-degraded` — refusal + explicit allow
3. `just test braid-module-degraded-raid1` — degraded mount with flag
4. `just test braid-unlock` — Test 4a (refusal), 4b (allow), existing tests
5. `just test` — full suite green
