# Plan: Make `braid recover` self-mount the pool

## Context

There's a chicken-and-egg in braid's recovery flow:
1. `braid unlock` calls `check_no_pending_operation()` first — if `pending-op.json` exists, it hard-fails and tells the user to run `braid recover`.
2. `braid recover` requires the pool to be mounted (`probe_pool()` at `recover.rs:49`). If not mounted, it prints manual `cryptsetup open` + `mount` instructions.
3. The user must manually run low-level LUKS/btrfs commands — the exact thing braid exists to abstract away — during the most stressful scenario (interrupted mutation).

The journal already contains everything needed to mount: `pre_membership` and `target_membership` both have `DiskMember` structs with `by_id` paths. The existing `union_memberships()` function computes the union of all candidate devices.

## Approach: shared mount helper

Extract the "probe, credential, open LUKS, scan, degraded check, mount" sequence (unlock.rs lines 54-209) into a shared `open_and_mount_pool()` function in a new `cli/src/mount.rs` module. Both `cmd_unlock` and `cmd_recover` call it. This keeps mount options, credential verification, degraded handling, and wrapper synchronization aligned in one place rather than creating a second mount implementation that drifts.

## Changes

### 1. New module: `cli/src/mount.rs`

New types:

```rust
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    #[error("{0}")] Probe(#[from] ProbeError),
    #[error("{0}")] Luks(#[from] LuksError),
    #[error("command failed: {0}")] Cmd(#[from] CmdError),
    #[error("{0}")] Failed(String),
    #[error("{0}")] DegradedRefused(String),
}

pub enum Credential<'a> {
    Passphrase { passphrase_stdin: bool, passphrase_file: Option<&'a Path> },
    KeyFile(&'a Path),
}
```

Shared function — contains the logic currently at unlock.rs lines 54-209:

```rust
/// Open LUKS devices from a membership set and mount the btrfs pool.
/// Returns Ok(true) if mount was performed, Ok(false) if already mounted.
pub fn open_and_mount_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R, fs: &F, config: &Config, membership: &PoolMembership,
    credential: Credential<'_>, allow_degraded: bool, command_hint: &str,
) -> Result<bool, MountError>
```

Steps inside: mountpoint check (return `Ok(false)` if mounted) → probe each disk → credential verify + LUKS open → btrfs device scan → degraded check → mkdir + mount → return `Ok(true)`.

The `DegradedRefused` error message includes a hint like `hint: braid <command> --allow-degraded`. The helper accepts a `command_hint: &str` parameter (e.g. `"unlock"` or `"recover"`) so the hint text is correct for each caller.

The helper does NOT: check pending-op (caller's job), refresh pool.json metadata (unlock-specific), warn about paused balances (unlock-specific).

### 2. Refactor `cli/src/unlock.rs`

- Remove inline mount flow (lines 54-209) and `tag()` helper (moved to mount.rs)
- Add `Mount(#[from] MountError)` variant to `UnlockError`, remove `DegradedRefused` variant
- `cmd_unlock` becomes:
  1. `check_no_pending_operation(paths)`
  2. Build `Credential` from args (`KeyFile` or `Passphrase`)
  3. `mount::open_and_mount_pool(runner, fs, config, membership, credential, allow_degraded, "unlock")?`
  4. If returned `false` → already mounted, return `Ok(())`
  5. `refresh_pool_metadata` (best-effort)
  6. Warn about paused balance (best-effort)
- Update existing tests: `UnlockError::DegradedRefused(..)` → `UnlockError::Mount(MountError::DegradedRefused(..))`

### 3. Update `cli/src/main.rs`

Add `RecoverArgs` (passphrase-only, no `--key-file` per Principle 4):

```rust
#[derive(Debug, Args)]
struct RecoverArgs {
    #[arg(long)] passphrase_stdin: bool,
    #[arg(long)] passphrase_file: Option<PathBuf>,
    #[arg(long)] allow_degraded: bool,
}
```

Change `Commands::Recover` → `Recover(RecoverArgs)`. Update match arm to pass new args and handle `DegradedRefused` with exit code 2 (matching unlock's pattern at main.rs:369-372).

Update unlock's `DegradedRefused` match from `UnlockError::DegradedRefused` to `UnlockError::Mount(MountError::DegradedRefused(..))`.

### 4. Extend `cli/src/recover.rs`

New signature:

```rust
pub fn cmd_recover<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R, fs: &F, config: &Config, paths: &StatePaths,
    passphrase_stdin: bool, passphrase_file: Option<&Path>,
    allow_degraded: bool,
) -> Result<(), RecoverError>
```

No `key_file` parameter — Principle 4: keyfiles are for unattended auto-unlock only, not manual recovery.

Add `Mount(#[from] mount::MountError)` to `RecoverError`.

Replace the error branch at lines 47-59 with:

```
let union = union_memberships(&journal);
let credential = Credential::Passphrase { passphrase_stdin, passphrase_file };

mount::open_and_mount_pool(runner, fs, config, &union, credential, allow_degraded, "recover")?;
// MountError::DegradedRefused propagates as RecoverError::Mount(MountError::DegradedRefused(..))
// so main.rs can match it for exit code 2.

let pool = probe::probe_pool(runner, mount_point)?;
// ... continue with existing recovery logic (build membership, write pool.json, clear journal)
```

### 5. Wrapper: `modules/braid/braid-wrapper.sh` line 33

```bash
# Before:
unlock|add)
# After:
unlock|add|recover)
```

This ensures `braid-online.service` activates after recover mounts the pool, maintaining the invariant "braid-online active ⟺ pool mounted" and registering the shutdown hook.

### 6. Add `pub mod mount;` to `cli/src/lib.rs`

### 7. Doc updates

**`docs/decisions/018-systemd-lifecycle.md` line 98:**
- Change "On successful `unlock` or `add`:" → "On successful `unlock`, `add`, or `recover`:"
- Add: "`recover` follows the same post-mount path because it may self-mount the pool when recovering from an interrupted operation."

**`docs/decisions/017-runtime-disk-membership.md` lines 47 and 60:**
- Update recovery mode description: "`braid recover` opens LUKS devices, mounts the pool (with `--allow-degraded` if needed), rebuilds membership from the live mounted btrfs pool topology, and clears the journal."

**`docs/principles.md` line 7 (Principle 1):**
- Currently says "The pool is unlocked and mounted by explicit CLI commands (`braid unlock` or `braid-auto-unlock`)". Update to include `braid recover` as a recovery-only mount path: "The pool is unlocked and mounted by explicit CLI commands (`braid unlock`, `braid-auto-unlock`, or `braid recover` during recovery)."

**`README.md` line 201 (pending-op.json description):**
- Update to: "`braid recover --passphrase-stdin` opens LUKS, mounts the pool, rebuilds membership from live state, and clears the journal. If devices are missing, pass `--allow-degraded`."

## Unit tests

### In `cli/src/mount.rs` (new)

| Test | Verifies |
|------|----------|
| `mount_already_mounted_returns_false` | MountpointCheck succeeds → `Ok(false)`, no LUKS commands |
| `mount_two_disk_happy_path` | 2 disks present+closed → open, scan, mount → `Ok(true)` |
| `mount_degraded_with_flag` | 1 absent, `allow_degraded=true` → `MountWithOptions` with "degraded" |
| `mount_degraded_refused` | 1 absent, `allow_degraded=false` → `MountError::DegradedRefused` |
| `mount_passphrase_mismatch_names_disk` | Verified on disk1, rejected on disk2 → error names both |
| `mount_no_unlockable_disks` | All absent, none open → `MountError::Failed` |
| `mount_skip_already_open` | All mappers open → no passphrase, just scan + mount |

### In `cli/src/unlock.rs` (update existing)

All 4 existing tests stay but update error type assertions (`DegradedRefused` → `Mount(MountError::DegradedRefused(..))`).

### In `cli/src/recover.rs` (update + new)

Update existing `recover_fails_when_device_missing_from_both_snapshots` for new signature.

| Test | Verifies |
|------|----------|
| `recover_self_mounts_when_pool_not_mounted` | Pool unmounted, 2 disks present. Opens LUKS, mounts, recovers pool.json, clears journal. |
| `recover_self_mounts_degraded` | 1 disk absent, `allow_degraded=true` → mounts degraded, recovers. |
| `recover_refuses_degraded_without_flag` | 1 disk absent, `allow_degraded=false` → `RecoverError::Mount(MountError::DegradedRefused(..))` with "braid recover --allow-degraded" hint. |
| `recover_skips_mount_when_already_mounted` | Pool already mounted → no passphrase needed, probes and recovers. |

## NixOS VM tests

### New: `tests/cli/braid-recover.nix` + `.py`

1. Build 2-disk RAID1 pool, write test data, lock pool
2. Write `pending-op.json` simulating interrupted add
3. `braid unlock --passphrase-stdin` → fails (journal exists)
4. `braid recover --passphrase-stdin` → exit 0
5. Verify: pool mounted, `pool.json` valid, `pending-op.json` gone, test data intact
6. `braid lock` then `braid unlock --passphrase-stdin` → normal ops resume

### Update: `tests/module/systemd-lifecycle.py`

New subtest 8: "braid recover activates braid-online.service"
- Pool offline, write fake `pending-op.json`
- `braid recover --passphrase-stdin` through wrapper
- Assert `systemctl is-active braid-online.service` and `mountpoint -q /mnt/storage`

## Files to modify

| File | Change |
|------|--------|
| `cli/src/mount.rs` | **New** — shared `open_and_mount_pool()` helper, `MountError`, `Credential` |
| `cli/src/lib.rs` | Add `pub mod mount;` |
| `cli/src/unlock.rs` | Refactor to call shared helper, update error types |
| `cli/src/recover.rs` | Self-mount via shared helper, `--allow-degraded`, new signature, new tests |
| `cli/src/main.rs` | `RecoverArgs`, update `Commands::Recover`, DegradedRefused exit code 2 |
| `modules/braid/braid-wrapper.sh` | Add `recover` to `unlock|add)` case arm |
| `docs/principles.md` | Add recover as recovery-only mount path in Principle 1 |
| `docs/decisions/018-systemd-lifecycle.md` | Add recover to wrapper doc |
| `docs/decisions/017-runtime-disk-membership.md` | Update recovery mode description |
| `README.md` | Update pending-op.json description |
| `tests/cli/braid-recover.nix` | **New** — VM test harness |
| `tests/cli/braid-recover.py` | **New** — VM test script |
| `tests/module/systemd-lifecycle.py` | New subtest for recover lifecycle |
| `flake.nix` | Register braid-recover test |

## Implementation sequence

1. Create `cli/src/mount.rs` with helper + unit tests
2. Add `pub mod mount;` to `cli/src/lib.rs`
3. Refactor `cli/src/unlock.rs` to call shared helper; update error types + tests
4. `cargo test` — all existing unlock tests pass
5. Extend `cli/src/recover.rs` — new signature, self-mount, `--allow-degraded`, tests
6. Update `cli/src/main.rs` — `RecoverArgs`, match arms
7. `cargo test` — all unit tests pass
8. Update wrapper (`braid-wrapper.sh`)
9. Update docs
10. Write NixOS VM tests + register in `flake.nix`
11. `just test braid-recover` + `just test systemd-lifecycle`

## Verification

1. `just test-rust` — all existing + new unit tests pass
2. `just test braid-recover` — new NixOS VM test passes
3. `just test systemd-lifecycle` — lifecycle test passes with new subtest
4. `just test braid-unlock` — no regression
