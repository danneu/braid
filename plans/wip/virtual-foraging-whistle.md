# Plan: Add --dry-run to enroll, unlock, lock, recover

## Context

Four mutating commands lack `--dry-run`: `enroll`, `unlock`, `lock`, and `recover`. The shared `Step` infrastructure (`cmd.rs`) and `$`-prefixed shell command rendering are already in place from the previous work on add/remove/replace. This plan extends dry-run to the remaining commands.

## Design principle

Dry-run must run the same read-only validation as execution. The plan is derived from the same probe + validate code path — not a separate, weaker approximation. Dry-run diverges only at the point where the first mutation would happen.

Pattern (same as add/remove/replace):
1. Probe + validate (read-only, same errors as execution)
2. Compile steps → `Vec<Step>`
3. If `--dry-run`: `Step::print_dry_run()` and return
4. Execute

Dry-run must not prompt for a passphrase — the plan is derivable from pool.json + filesystem state alone.

---

## Shared helper: `plan_open_pool()` in `mount.rs`

### Problem

`open_and_mount_pool()` (mount.rs:45-220) interleaves read-only validation with mutations. Both `unlock` and `recover` need the read-only part (probe disks, UUID mismatch check, "no unlockable disks" error, degraded refusal) separated from execution.

### Solution

Extract the read-only probe + validate logic from `open_and_mount_pool()` into a new `plan_open_pool()` function. This returns a validated `OpenPlan` that both dry-run and execution consume.

```rust
/// Result of the read-only probe + validate phase.
pub struct OpenPlan {
    /// Disks that need LUKS open (name, by_id pairs).
    pub to_unlock: Vec<(String, ByIdPath)>,
    /// At least one mapper was already open.
    pub any_open: bool,
    /// At least one membership disk was absent/damaged.
    pub any_missing_member: bool,
    /// First mapper device path to use for mount (from to_unlock or existing open mapper).
    pub mount_device: String,
}

/// Probe membership disks, validate UUIDs, check degraded policy.
/// Returns the same errors that open_and_mount_pool() would.
/// No mutations — safe for dry-run.
pub fn plan_open_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    command_hint: &str,
) -> Result<Option<OpenPlan>, MountError>
```

Returns `Ok(None)` when pool is already mounted.

Errors raised (same as current `open_and_mount_pool`):
- LUKS UUID mismatch → `MountError::Failed("disk '...' LUKS UUID mismatch ...")`
- No unlockable disks and none open → `MountError::Failed("no unlockable disks found")`
- Missing members + `!allow_degraded` → `MountError::DegradedRefused(...)`

Then refactor `open_and_mount_pool()` to call `plan_open_pool()` internally, consuming the `OpenPlan` for execution. This guarantees dry-run and execution share the identical validation code — zero drift.

### compile_open_steps()

Takes a validated `OpenPlan` and produces `Vec<Step>`:

```rust
pub fn compile_open_steps(
    plan: &OpenPlan,
    mount_point: &MountPoint,
    key_file: Option<&Path>,
) -> Vec<Step>
```

Steps produced:
- Per `to_unlock` entry: `CryptsetupLuksOpen` or `CryptsetupLuksOpenKeyFile`
- `BtrfsDeviceScanAll`
- `Mount` or `MountWithOptions { options: ["degraded"] }` (if `any_missing_member`)

Used by both `unlock` and `recover` for dry-run rendering.

---

## 1. enroll (`cli/src/enroll_key_file.rs`)

### Current structure

Clean phase separation:
- `discover_enrollment_candidates()` → probes pool.json, skips absent/non-LUKS, errors if none eligible
- `plan_enrollment()` → verifies passphrase, checks slots, classifies as `NeedsEnroll`/`AlreadyEnrolled`
- `generate_key_file()` → writes keyfile (if `--generate`)
- `apply_enrollment()` → runs `luksAddKey` + header backup per disk

### Dry-run approach

Keep all existing read-only validation in the dry-run path:
1. `preflight::check_no_pending_operation()` — same as execution
2. Keyfile path validation — same as execution (exists check for `--generate`, metadata check otherwise)
3. `discover_enrollment_candidates()` — same as execution (probes disks, errors on zero candidates)
4. If dry-run: compile steps from discovered candidates and return (skip passphrase + plan_enrollment + apply)

Since dry-run skips `plan_enrollment()` (which needs a passphrase), it assumes all discovered candidates need enrollment (worst-case). Disks already enrolled will be skipped at execution time — this is a safe over-approximation.

### Steps to show

**If `--generate`:**

| Step | Risk | Commands |
|------|------|----------|
| generate keyfile → {path} | safe | (no CmdRequest — file I/O) |

**Per discovered candidate:**

| Step | Risk | Commands |
|------|------|----------|
| enroll keyfile → LUKS slot 1 on {by_id} | safe | `CryptsetupLuksAddKeyFile { device, key_file_path }` |
| LUKS header backup → {backup_path} | safe | `CryptsetupLuksHeaderBackup { device, backup_path }` |

### Changes

- `main.rs`: Add `--dry-run` to `EnrollKeyFileArgs` clap struct, pass to `cmd_enroll_key_file`
- `enroll_key_file.rs`:
  - Add `dry_run: bool` parameter to `cmd_enroll_key_file`
  - Add `compile_enroll_steps()` that takes candidates + key_file_path + generate + paths → `Vec<Step>`
  - Insert dry-run check after discovery (step 3), before passphrase read
  - Keep preflight, keyfile validation, and discovery in dry-run path

### Tests

- `dry_run_render_enroll_generate_3_disks` — happy path, shows generate + 3× (enroll + backup)
- `dry_run_enroll_no_candidates_errors` — all disks absent → same error as execution
- `dry_run_enroll_generate_keyfile_exists_errors` — keyfile already present → same error as execution

---

## 2. unlock (`cli/src/unlock.rs`)

### Current structure

Calls `mount::open_and_mount_pool()` which interleaves probing and mutating.

### Dry-run approach

1. `preflight::check_no_pending_operation()` — same as execution
2. `plan_open_pool()` — shared read-only probe + validate (UUID check, degraded refusal, etc.)
3. If dry-run: `compile_open_steps()` → `Step::print_dry_run()` and return
4. Otherwise: proceed to `open_and_mount_pool()` as before

No passphrase required for dry-run.

### Steps to show

| Step | Risk | Commands |
|------|------|----------|
| LUKS open {by_id} → {mapper} (per closed disk) | safe | `CryptsetupLuksOpen` or `CryptsetupLuksOpenKeyFile` |
| btrfs device scan | safe | `BtrfsDeviceScanAll` |
| mount → {mount_point} | safe | `Mount` or `MountWithOptions` (if degraded) |

If pool already mounted: `plan_open_pool()` returns `None` → "pool already mounted" message.

### Changes

- `main.rs`: Add `--dry-run` to `UnlockArgs` clap struct, pass to `cmd_unlock`
- `unlock.rs`:
  - Add `dry_run: bool` parameter to `cmd_unlock`
  - Call `plan_open_pool()` before credential handling
  - If dry-run: compile + print + return
  - If not dry-run: `open_and_mount_pool()` as before (which now calls `plan_open_pool()` internally)

### Tests

- `dry_run_render_unlock_2_closed_disks` — happy path, shows 2× LUKS open + scan + mount
- `dry_run_unlock_already_mounted` — plan_open_pool returns None → "already mounted"
- `dry_run_unlock_uuid_mismatch_errors` — same UUID mismatch error as execution
- `dry_run_unlock_degraded_refused` — missing disk + no `--allow-degraded` → same error
- `dry_run_unlock_no_unlockable_disks_errors` — all absent → same error

---

## 3. lock (`cli/src/lock.rs`)

### Current structure

Simple: check mounted → umount → scan forget → close each mapper → close orphans.

### Dry-run approach

1. `MountpointCheck` — same as execution
2. If mounted: `preflight::check_no_exclusive_op()` — same as execution
3. Probe which mappers are open (via `fs.exists`) — same as execution
4. Scan for orphaned braid-* mappers — same as execution
5. Compile steps from this state
6. If dry-run: print and return

### Steps to show

| Step | Risk | Commands |
|------|------|----------|
| unmount {mount_point} (if mounted) | safe | `Umount { mount_point }` |
| btrfs device scan --forget (if mounted) | safe | `BtrfsDeviceScanForget` |
| close LUKS mapper {mapper} (per open disk) | safe | `CryptsetupClose { mapper }` |
| close LUKS mapper {mapper} (per orphan) | safe | `CryptsetupClose { mapper }` |

If pool not mounted and no mappers open: "nothing to do."

### Changes

- `main.rs`: Add `LockArgs` struct with `--dry-run`, update Lock dispatch
- `lock.rs`:
  - Add `dry_run: bool` parameter to `cmd_lock`
  - Add `compile_lock_steps()` that takes mounted status + open mappers + orphan mappers + mount_point → `Vec<Step>`
  - Insert dry-run check after probing, before execution

### Tests

- `dry_run_render_lock_mounted_2_disks` — happy path, shows umount + scan forget + 2× close
- `dry_run_lock_not_mounted_1_open` — skip umount/scan, show 1× close
- `dry_run_lock_nothing_to_do` — not mounted, all closed → empty/nothing message

---

## 4. recover (`cli/src/recover.rs`)

### Current structure

Reads journal → `open_and_mount_pool()` with union membership → probes live state → writes pool.json → clears journal.

### Dry-run approach

1. Load journal — same as execution (errors if absent)
2. Build union membership — same as execution
3. `plan_open_pool()` — shared read-only probe + validate
4. If dry-run: compile steps → print and return

No passphrase required for dry-run.

Unlike `unlock`, `recover` is not a no-op when the pool is already mounted — execution still probes the live pool, writes `pool.json`, and clears `pending-op.json`. So:

- **`plan_open_pool()` returns `Some(plan)`**: render open steps (LUKS open + scan + mount) *and* state recovery steps.
- **`plan_open_pool()` returns `None` (already mounted)**: skip open/scan/mount steps but still render the state recovery steps (write pool.json, clear journal). The pool is up — recover just needs to reconcile state.

### Steps to show

**Open pool (from `compile_open_steps`, only if pool not already mounted):**

| Step | Risk | Commands |
|------|------|----------|
| LUKS open {by_id} → {mapper} (per closed disk) | safe | `CryptsetupLuksOpen` |
| btrfs device scan | safe | `BtrfsDeviceScanAll` |
| mount → {mount_point} | safe | `Mount` or `MountWithOptions` |

**State recovery (always shown):**

| Step | Risk | Commands |
|------|------|----------|
| write recovered pool.json | safe | (no CmdRequest — file I/O) |
| clear pending-op.json | safe | (no CmdRequest — file I/O) |

### Changes

- `main.rs`: Add `--dry-run` to `RecoverArgs` clap struct, pass to `cmd_recover`
- `recover.rs`:
  - Add `dry_run: bool` parameter to `cmd_recover`
  - Call `plan_open_pool()` after loading journal + building union membership
  - If dry-run: compile open steps + recovery steps → print and return

### Tests

- `dry_run_render_recover_2_disks` — happy path (not mounted), shows open + mount + state recovery
- `dry_run_render_recover_already_mounted` — pool up, shows only state recovery steps (no open/scan/mount)
- `dry_run_recover_no_journal_errors` — no journal → same error as execution
- `dry_run_recover_degraded_refused` — missing disk + no `--allow-degraded` → same error

---

## Files modified

| File | Change |
|------|--------|
| `cli/src/mount.rs` | Extract `plan_open_pool()` + `OpenPlan` struct, add `compile_open_steps()`, refactor `open_and_mount_pool()` to consume `OpenPlan` |
| `cli/src/main.rs` | Add `--dry-run` flag to `EnrollKeyFileArgs`, `UnlockArgs`, `RecoverArgs`; add `LockArgs` struct; pass dry_run to cmd_* functions |
| `cli/src/enroll_key_file.rs` | Add `dry_run` param, `compile_enroll_steps()`, early return after discovery |
| `cli/src/unlock.rs` | Add `dry_run` param, call `plan_open_pool()` + `compile_open_steps()` |
| `cli/src/lock.rs` | Add `dry_run` param, `compile_lock_steps()`, early return |
| `cli/src/recover.rs` | Add `dry_run` param, call `plan_open_pool()` + `compile_open_steps()` + recovery steps |

## Verification — TDD approach

### Step 1: Write failing tests first

For each command, add tests for both happy-path rendering and error branches (listed in each section above). Tests call compile functions + `Step::render_dry_run()` and assert output. Error-branch tests verify dry-run returns the same errors as execution.

### Step 2: Implement

1. Extract `plan_open_pool()` from `open_and_mount_pool()` in mount.rs (refactor, no behavior change)
2. Add `compile_open_steps()` in mount.rs
3. Add clap flags in main.rs
4. Add compile functions and dry-run checks in each command file
5. Make failing tests pass

### Step 3: Run full suite

1. `just test-rust` — all unit tests
2. `just test` — VM integration tests (ensure refactored mount path is unchanged)
