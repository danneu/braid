# Phase 4: Apply Engine

## Context

Phases 1–3.5 are complete: config, parsing, types, planner, probe, and `braid plan` all work end-to-end with VM tests. `braid apply` in main.rs prints "not yet implemented". This phase implements the typed action executor, durable checkpoint/resume, confirmation gates, and validates everything in a NixOS VM test.

## Files to modify/create

- `cli/src/cmd.rs` — add mutation `CmdRequest` variants + `RealRunner` dispatch
- `cli/src/apply.rs` — **NEW** — checkpoint I/O, confirmation gates, action executor, apply orchestrator
- `cli/src/lib.rs` — add `pub mod apply`
- `cli/src/main.rs` — wire `apply` subcommand
- `cli/src/config.rs` — add `config_read_raw()` that returns both `Config` and raw text (for hashing)
- `tests/16-braid-apply-rust.nix` — **NEW** — NixOS VM test config
- `tests/braid-apply-rust.py` — **NEW** — VM test script
- `flake.nix` — register `braid-apply-rust`

## Steps

### 1. Add mutation CmdRequest variants to cmd.rs

New variants needed for apply's action handlers:

| Variant | Command | Notes |
|---|---|---|
| `CryptsetupLuksOpen { device, mapper, passphrase }` | `echo -n {passphrase} \| cryptsetup luksOpen --key-file=- {device} {mapper}` | Pipe passphrase via stdin |
| `CryptsetupIsLuks { device }` | `cryptsetup isLuks {device}` | Exit 0 = LUKS, non-zero = not |
| `CryptsetupClose { mapper }` | `cryptsetup close {mapper}` | |
| `BtrfsDeviceAdd { device, mount_point }` | `btrfs device add -f {device} {mount_point}` | `-f` handles stale metadata |
| `BtrfsDeviceRemove { device, mount_point }` | `btrfs device remove {device} {mount_point}` | |
| `BtrfsDeviceRemoveMissing { mount_point }` | `btrfs device remove missing {mount_point}` | |
| `BtrfsDeviceScan { device }` | `btrfs device scan {device}` | For returning-member detection |
| `BtrfsBalanceRaid1 { mount_point }` | `btrfs balance start -dconvert=raid1 -mconvert=raid1 {mount_point}` | |
| `BtrfsBalanceSingle { mount_point }` | `btrfs balance start -dconvert=single -mconvert=single -f {mount_point}` | Pre-remove conversion |
| `MkfsBtrfs { device }` | `mkfs.btrfs -f {device}` | First-disk bootstrap only |
| `Mount { device, mount_point }` | `mount {device} {mount_point}` | After mkfs for bootstrap |
| `MountpointCheck { path }` | `mountpoint -q {path}` | For verify_health |

`RealRunner` dispatch: `CryptsetupLuksOpen` is special — it needs stdin piping. Use `Command::new("sh").args(["-c", "echo -n ... | cryptsetup ..."])` or use `Command.stdin(Stdio::piped())` and write passphrase to stdin. Prefer the latter (no shell injection risk).

**Critical:** All mutation commands still return `Ok(RawCommandOutput)` for non-zero exits. Callers decide what non-zero means.

### 2. Add config_read_raw() to config.rs

```rust
pub fn config_read_raw(path: &Path) -> Result<(Config, String), ConfigError> {
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read { ... })?;
    let cfg: Config = serde_json::from_str(&raw).map_err(|source| ConfigError::Parse { ... })?;
    validate(&cfg)?;
    Ok((cfg, raw))
}
```

Apply needs the raw config text to compute `config_hash` for checkpoint staleness detection. The existing `config_hash(raw: &str) -> String` already exists.

### 3. Create apply.rs

This is the main new module. Organized into sections:

#### 3a. Constants & types

```rust
const CHECKPOINT_DIR: &str = "/var/lib/braid";
const CHECKPOINT_FILE: &str = "/var/lib/braid/apply-state.json";
const HISTORY_DIR: &str = "/var/lib/braid/history";
const HISTORY_KEEP: usize = 20;
```

**`Checkpoint` struct** (serializable to/from the checkpoint JSON):
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub plan_id: String,
    pub mount_point: String,
    pub status: PlanStatus,
    pub config_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_completed_action_id: String,
    pub actions: Vec<Action>,
    pub warnings: Vec<Warning>,
    pub confirmations: Vec<Confirmation>,
}
```

**`ApplyError` enum:**
```rust
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("plan is blocked: {0}")]
    Blocked(String),
    #[error("checkpoint exists at {path}. Use --resume to continue.")]
    CheckpointExists { path: String },
    #[error("no checkpoint found. Run 'braid apply' first.")]
    NoCheckpoint,
    #[error("config has changed since checkpoint was created")]
    StaleCheckpoint,
    #[error("confirmation required: BRAID_CONFIRM='{phrase}'")]
    ConfirmationMissing { phrase: String },
    #[error("action {action_id} ({action_type}) failed: {detail}")]
    ActionFailed { action_id: String, action_type: String, detail: String },
    #[error("target absent for pending action {action_id}: {target}")]
    ResumeTargetMissing { action_id: String, target: String },
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Probe(#[from] crate::probe::ProbeError),
}
```

#### 3b. Checkpoint I/O

```rust
fn checkpoint_write(cp: &Checkpoint) -> Result<(), ApplyError>
fn checkpoint_read() -> Result<Checkpoint, ApplyError>
fn checkpoint_finalize(cp: &Checkpoint) -> Result<(), ApplyError>
```

- `checkpoint_write`: atomic write via `.tmp` + rename. Creates `CHECKPOINT_DIR` if needed.
- `checkpoint_read`: read + deserialize from `CHECKPOINT_FILE`.
- `checkpoint_finalize`: copy to `HISTORY_DIR/{plan_id}.json`, remove checkpoint, prune history to `HISTORY_KEEP` newest.

#### 3c. Confirmation gates

```rust
fn check_confirmations(confirmations: &[Confirmation]) -> Result<(), ApplyError>
```

- Read `BRAID_CONFIRM` env var.
- Split on `;`, trim whitespace.
- For each required phrase in confirmations, check it exists in provided set.
- Return `Err(ApplyError::ConfirmationMissing)` on first unmatched phrase.

#### 3d. Action executor — one handler per ActionType

Each handler is a function: `fn execute_X(runner, fs, target, config) -> Result<(), ApplyError>`.

**Handlers and their logic (matching bash semantics exactly):**

| Handler | Logic |
|---|---|
| `execute_open_luks(target)` | Read `BRAID_PASSPHRASE` env. Check device exists (`fs.exists`). Check `isLuks`. Idempotent: if mapper already open with same UUID, skip. Otherwise `cryptsetupLuksOpen`. |
| `execute_btrfs_add(target, mount_point)` | Check pool exists (`btrfs filesystem show`). If 0 devices: `mkfs.btrfs` + `mount`. Otherwise: `btrfs device scan` + check if returning member; if not in pool, `btrfs device add -f`. |
| `execute_balance_raid1(mount_point)` | Check if already RAID1 with no missing → skip. Otherwise `btrfs balance start -dconvert=raid1 -mconvert=raid1`. |
| `execute_remove_graceful(target, mount_point)` | Count current devices. If remaining < 2: convert to single first (`btrfs balance start -dconvert=single -mconvert=single -f`). Then `btrfs device remove`. |
| `execute_remove_missing(mount_point)` | Count present devices. If remaining < 2: convert to single first. Then `btrfs device remove missing`. |
| `execute_close_luks(target)` | `cryptsetup close`. Non-fatal on failure (warn, don't error). |
| `execute_verify_health(mount_point)` | Check mounted (`mountpoint -q`). `btrfs filesystem show` — warn if missing, don't fail. |
| `execute_verify_diskset(config, mount_point)` | For each config disk: check present, check LUKS, check UUID in pool. Warn on mismatches, don't fail. |

**Idempotency in `execute_open_luks`** (critical for resume): Before opening, iterate `/dev/mapper/*` entries, check if any has the same LUKS UUID as the target device. If found, skip. This prevents double-open errors on resume.

**`execute_btrfs_add` first-disk detection**: Run `btrfs filesystem show {mount_point}`. If it fails or returns 0 `devid` lines → first disk → `mkfs.btrfs -f {device}` then `mount`. Otherwise → `btrfs device add -f`. This is the ONLY code path that calls `mkfs.btrfs`, and only when the pool has zero devices (bootstrap).

#### 3e. Action dispatch

```rust
fn execute_action(
    runner: &RealRunner,
    fs: &RealFilesystem,
    action: &Action,
    config: &Config,
) -> Result<(), ApplyError>
```

Match on `action.action_type`, dispatch to the corresponding handler. The target is `action.target`.

#### 3f. Resume target validation

On `--resume`, before executing any pending action, check that device-targeting actions (OPEN_LUKS, ADD_DISK_BTRFS_ADD, REMOVE_DISK_GRACEFUL) have their target present. If not → `Err(ApplyError::ResumeTargetMissing)`. This matches the bash `RESUME_TARGET_MISSING` behavior.

```rust
fn validate_resume_targets(
    fs: &impl Filesystem,
    actions: &[Action],
) -> Result<(), ApplyError>
```

Only check actions that are still `Pending` or `InProgress`. Only check action types that reference a physical device or mapper.

#### 3g. Main orchestrator

```rust
pub fn cmd_apply(
    config_path: &Path,
    args: &ApplyArgs,
) -> Result<(), ApplyError>
```

**Fresh apply flow:**
1. Check no checkpoint exists → error if it does (must use `--resume`)
2. `config_read_raw` → `(config, raw_text)`
3. Probe (same as plan): `probe_config_disk` for each disk, `probe_pool`
4. `compute_plan` → if blocked, `return Err(ApplyError::Blocked)`
5. Print warnings
6. Count mutation actions — if 0, print "Nothing to do", return
7. `check_confirmations`
8. Build `Checkpoint` from plan + `config_hash(raw_text)` + timestamps
9. `checkpoint_write`
10. Execute loop (shared with resume)

**Resume flow:**
1. Check checkpoint exists → error if not
2. `config_read_raw` → compute hash → compare with `checkpoint.config_hash` → stale error if different
3. `validate_resume_targets`
4. Execute loop (shared with fresh)

**Execute loop (shared):**
```rust
for action in &mut checkpoint.actions {
    if action.status == ActionStatus::Completed {
        println!("[{}] {} — already completed, skipping.", action.id, ...);
        continue;
    }

    println!("[{}] {} target={}", action.id, ..., action.target);
    action.status = ActionStatus::InProgress;
    checkpoint.updated_at = now_utc();
    checkpoint_write(&checkpoint)?;

    match execute_action(runner, fs, action, config) {
        Ok(()) => {
            action.status = ActionStatus::Completed;
            checkpoint.last_completed_action_id = action.id.clone();
        }
        Err(e) => {
            action.status = ActionStatus::Failed;
            checkpoint_write(&checkpoint)?;
            return Err(e);
        }
    }

    checkpoint.updated_at = now_utc();
    checkpoint_write(&checkpoint)?;

    // Test hook: BRAID_TEST_FAIL_AFTER_ACTION
    if env::var("BRAID_TEST_FAIL_AFTER_ACTION").ok().as_deref() == Some(&action.id) {
        checkpoint_write(&checkpoint)?;
        return Err(ApplyError::ActionFailed {
            action_id: action.id.clone(),
            action_type: "test_hook".into(),
            detail: "simulated failure".into(),
        });
    }
}
```

**Footer output:**
```
Applied {mutation_completed} actions, skipped {warnings_skipped} with warnings, blocked 0
```
Where `mutation_completed` counts completed non-VERIFY actions; `warnings_skipped` counts DISK_ABSENT_SKIPPED + INIT_REQUIRED warnings.

Then: `checkpoint_finalize`.

### 4. Wire apply in main.rs

```rust
Commands::Apply(args) => {
    if let Err(e) = braid_cli::apply::cmd_apply(Path::new(&config_path), &args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
```

Move `ApplyArgs` to lib (or pass individual fields). The simplest approach: define an `ApplyFlags` struct in types.rs (like `PlanFlags`) and pass it in.

### 5. Update lib.rs

Add `pub mod apply;`.

### 6. NixOS VM test

**`tests/16-braid-apply-rust.nix`** — 4 virtual disks. Both bash `braid` (for init-disk setup) and Rust `braid-rust` (for apply validation). Same pattern as `15-braid-plan-rust.nix`.

**`tests/braid-apply-rust.py`** — Subtests matching the bash test coverage:

| # | Subtest | Validates |
|---|---|---|
| 0 | Setup: init 2 disks via bash | Pool bootstrap |
| 1 | Fresh apply builds 2-disk RAID1 | `braid-rust apply` creates pool, mounts, RAID1 profile |
| 2 | No-op apply | "Nothing to do" or "no actions" message |
| 3 | Add disk3 | Pool gains disk3, RAID1 maintained |
| 4 | Data intact after add | precious.txt readable |
| 5 | Checkpoint removed after success | `/var/lib/braid/apply-state.json` absent |
| 6 | History file written | `/var/lib/braid/history/` has entry |
| 7 | Remove disk3 | Pool loses disk3, LUKS mapper closed |
| 8 | Redundancy refusal without confirmation | Exit 1 when removing to single disk |
| 9 | Redundancy acceptance with phrase | Exit 0 with correct BRAID_CONFIRM |
| 10 | Absent disk warns but continues | DISK_ABSENT_SKIPPED in output, other actions proceed |
| 11 | Replace dead disk (degraded + --allow-remove-missing) | ADD + REMOVE_MISSING with confirmation |
| 12 | Blocked plan (ambiguous) exits 1 | Absent disk + removal → blocked |
| 13 | --allow-remove-ambiguous with confirmation | Unblocks and executes |
| 14 | Semicolon multi-confirmation | Both ambiguity + redundancy phrases |
| 15 | Interrupted apply leaves checkpoint | BRAID_TEST_FAIL_AFTER_ACTION=a1, checkpoint file exists |
| 16 | Resume continues from checkpoint | --resume completes remaining actions |
| 17 | Stale checkpoint refuses resume | Fake checkpoint with wrong hash → exit 1 |
| 18 | Resume target absent → exit 1 | Hide device, --resume fails, checkpoint preserved |
| 19 | Apply never calls luksFormat | No luksFormat/mkfs path reachable from apply |

Note: bootstrap (mkfs.btrfs for first disk) is the one legitimate mkfs path. The test in #19 verifies there's no `LuksFormat` action type — the same compile-time guarantee from `plan_no_format_action_exists` in plan.rs.

### 7. Register test in flake.nix

```nix
braid-apply-rust = pkgs.testers.nixosTest (import ./tests/16-braid-apply-rust.nix);
```

## Safety invariants

1. **No luksFormat in apply path.** `ActionType` enum has no format variant. `mkfs.btrfs` only runs inside `execute_btrfs_add` when pool has 0 devices (fresh bootstrap). The `MkfsBtrfs` CmdRequest is only reachable from that one code path.

2. **Config hash staleness.** On `--resume`, checkpoint's `config_hash` must match `config_hash(current_raw_config)`. Any config edit between interrupt and resume → hard reject.

3. **Confirmation gates before execution.** All phrases from `confirmations[]` must be provided in `BRAID_CONFIRM` before any action runs. No partial execution without full confirmation.

4. **Atomic checkpoint writes.** Write `.tmp` + rename. No partial JSON on crash.

5. **Resume idempotency.** `execute_open_luks` checks if already open (by UUID). `execute_btrfs_add` checks if device already in pool. Completed actions are skipped.

## Key decisions

- **Mutation commands as CmdRequest variants** — keeps all command construction in `cmd.rs`, testable via `MockRunner`.
- **`CryptsetupLuksOpen` uses stdin pipe** — no shell injection risk from passphrase.
- **Checkpoint is a superset of PlanReport** — adds `config_hash`, `created_at`, `updated_at`, `last_completed_action_id`. Actions are the same `Vec<Action>` with status updated in place.
- **BRAID_TEST_FAIL_AFTER_ACTION** — test hook for simulating interruption. Checked after each action completion. Same mechanism as bash.
- **Verify actions warn, don't fail** — `execute_verify_health` and `execute_verify_diskset` print warnings but return `Ok`. Matches bash behavior.
- **`execute_close_luks` is non-fatal** — prints warning if close fails. Device will close on reboot.

## Verification

```bash
cd cli && cargo test                        # All unit tests
make test-one t=braid-apply-rust            # New VM test
make test-one t=braid-apply                 # Existing bash test (no regression)
make test-one t=braid-plan-rust             # Plan test (no regression)
```
