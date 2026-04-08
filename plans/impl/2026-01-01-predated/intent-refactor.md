# Migration: Plan/Apply → Intent CLI + NixOS-Native Automation

## Context

Braid's current plan/apply reconciliation engine is over-engineered for NAS drives, which have ~4 events in their lifetime (create pool, add disk, maybe add another, replace a dead one). The generic reconciler creates problems: risk flattening (routine reboot looks like adding a disk), combinatorial complexity (`--allow-remove-missing`, `--allow-remove-ambiguous`, `BRAID_CONFIRM='phrase1;phrase2'`), and ceremony for routine operations (`braid apply` after every reboot).

This migration replaces plan/apply with:
1. **Four intent commands** (`add`, `remove`, `replace`, `status`) — each does one thing with risk-appropriate UX
2. **Named disks** — attrset config with human-friendly names used everywhere
3. **NixOS-native automation** — systemd handles routine pool unlock/mount; `nixos-rebuild` prints advisory guidance
4. **Lightweight resumability** — per-command checkpoint for long-running operations (balance, remove)
5. **Clean break** — no backwards compat, no v1 migration. Project is unreleased.

---

## Phase 1: Named Disk Config

Change `braid.disks` from a list of strings to a named attrset.

### 1a. NixOS module options

**`modules/braid/options.nix`** — Replace `disks` option:

```nix
disks = lib.mkOption {
  type = lib.types.attrsOf (lib.types.submodule {
    options.byId = lib.mkOption {
      type = lib.types.str;
      description = "Full /dev/disk/by-id/ path for this disk.";
    };
  });
  default = {};
  description = "Named disks for the LUKS + btrfs pool.";
};
```

User config becomes:
```nix
braid.disks = {
  toshiba  = { byId = "/dev/disk/by-id/ata-Toshiba_MN07_XXXX"; };
  ironwolf = { byId = "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY"; };
};
```

Add eval-time assertions (fire at `nixos-rebuild` time, before any disk is touched):
- At least 1 disk when `enable = true`
- All `byId` paths start with `/dev/disk/by-id/`
- No duplicate `byId` values across different names

### 1b. NixOS module storage

**`modules/braid/storage.nix`** — Update to use disk names as mapper names:

- Mapper names become `braid-<name>` (e.g., `braid-toshiba`) instead of the by-id basename
- LUKS entries: `boot.initrd.luks.devices."braid-<name>".device = disk.byId`
- `fileSystems` entry: `noauto` — not authoritative for mounting. `braid-unlock.service` (stage-2) and initrd LUKS+mount handle the actual mount using the first available opened mapper. The `fileSystems` entry exists so NixOS knows about the mount point for `systemctl` targets, but does not hardcode which device to mount from.
- `btrfs-device-scan` after/wants: use new mapper unit names
- Systemd unit escaping: `braid\x2d<name>` for cryptsetup generator names

### 1c. Config JSON bridge

**`modules/braid/cli.nix`** — Write config.json. JSON uses **snake_case** to match Rust field names (serde derives work without rename attributes):

```json
{
  "disks": {
    "toshiba":  { "by_id": "/dev/disk/by-id/ata-Toshiba_MN07_XXXX" },
    "ironwolf": { "by_id": "/dev/disk/by-id/ata-Ironwolf_ST12_YYYY" }
  },
  "mount_point": "/mnt/storage"
}
```

The Nix option names (`byId`, `mountPoint`) are camelCase per NixOS convention. The JSON bridge translates to snake_case. This is a one-line mapping in the Nix `builtins.toJSON` expression.

### 1d. CLI config.rs

**`cli/src/config.rs`** — New Config struct. Field names match JSON snake_case, no `#[serde(rename)]` needed:

```rust
#[derive(Deserialize)]
struct DiskConfig {
    by_id: ByIdPath,
}

#[derive(Deserialize)]
struct Config {
    disks: BTreeMap<String, DiskConfig>,  // name → config
    mount_point: String,
}
```

Helpers: `disk_by_name()`, `mapper_name(name) -> MapperName` (returns `braid-<name>`), `names()`.

### 1e. CLI probe.rs

**`cli/src/probe.rs`** — Update `probe_config_disk` to accept `(name, &DiskConfig)`. `ConfigDisk` gains a `name: String` field. `mapper_name_for_by_id` → `mapper_name_for_disk(name)` returning `braid-<name>`.

### 1f. CLI types.rs

**`cli/src/types.rs`** — Delete all plan/apply types: `ActionState`, `ActionType`, `Action`, `PlannedCommand`, `RunCertainty`, `PlanOutcome`, `Plan<S>`, `ApplicablePlan`, `PlanStatus`, `PlanSummary`, `PlanReport`, `WarningCode`, `BlockedReasonCode`, `Warning`, `BlockedReason`, `Confirmation`, `PlanFlags`.

Keep: `ByIdPath`, `LuksUuid`, `MapperName`, `PoolState`, `PoolDevice`, `ConfigDisk`, `ConfigDiskState`.

---

## Phase 2: Intent Commands

Replace `plan`, `apply`, `init-disk` with `add`, `remove`, `replace`. Every command supports `--dry-run` and `--yes`. TUI is commented out (builds but not wired up).

**Implementation order**: 2h (shared helpers) → 2d (checkpoint) → 2b (passphrase) → 2e (add) → 2f (remove) → 2g (replace) → 2a (wire up main.rs) → 2c (dry-run integration) → 2i (delete old files) → 2j (status/doctor) → 2k (comment out TUI). Build foundations before consumers — luks.rs and pool.rs must exist before add/remove/replace can use them.

### 2a. CLI main.rs — new command structure

```rust
enum Commands {
    Add(AddArgs),
    Remove(RemoveArgs),
    Replace(ReplaceArgs),
    Status(StatusArgs),
    Doctor(DoctorArgs),
    // Tui — commented out, revisit later
}

// Shared across mutation commands
struct CommonArgs {
    #[arg(long)] dry_run: bool,
    #[arg(long)] yes: bool,
    #[arg(long)] passphrase_file: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t = ProgressMode::Auto)]
    progress: ProgressMode,
}

struct AddArgs {
    #[arg(add = ArgValueCandidates::new(disk_name_candidates))]
    name: String,
    #[command(flatten)] common: CommonArgs,
}

struct RemoveArgs {
    #[arg(add = ArgValueCandidates::new(disk_name_candidates))]
    name: String,
    #[arg(long)] missing_id: Option<u64>,
    #[command(flatten)] common: CommonArgs,
}

struct ReplaceArgs {
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    old: String,
    #[arg(long, add = ArgValueCandidates::new(disk_name_candidates))]
    new: String,
    #[arg(long)] missing_id: Option<u64>,
    #[command(flatten)] common: CommonArgs,
}
```

Tab completion returns disk names from config.json keys.

### 2b. Passphrase input

**`cli/src/luks.rs`** — Flexible passphrase sourcing:

```rust
fn read_passphrase(common: &CommonArgs) -> Result<String> {
    if let Some(path) = &common.passphrase_file {
        // Strip only trailing newline(s), not all whitespace — leading/trailing
        // spaces may be intentional passphrase characters.
        return Ok(fs::read_to_string(path)?.trim_end_matches('\n').into());
    }
    if common.yes {
        return std::env::var("BRAID_PASSPHRASE")
            .map_err(|_| err("--yes requires BRAID_PASSPHRASE or --passphrase-file"));
    }
    prompt_passphrase_tty()
}
```

### 2c. Per-command dry-run

Every mutation command probes state and compiles a conditional step list. `--dry-run` prints steps and exits. Steps reflect actual disk state — not a fixed template:

```
$ braid add ironwolf --dry-run
[destructive] LUKS format /dev/disk/by-id/ata-Ironwolf_ST12_YYYY
[safe]        LUKS open → braid-ironwolf
[safe]        btrfs device add /dev/mapper/braid-ironwolf /mnt/storage
[long]        btrfs balance to RAID1

$ braid add ironwolf --dry-run    # already LUKS-formatted
[safe]        LUKS open → braid-ironwolf
[safe]        btrfs device add /dev/mapper/braid-ironwolf /mnt/storage
[long]        btrfs balance to RAID1

$ braid replace --old ironwolf --new seagate --dry-run   # new disk already initialized
[safe]        LUKS open → braid-seagate
[safe]        btrfs device add /dev/mapper/braid-seagate /mnt/storage
[long]        btrfs balance to RAID1
[safe]        btrfs device remove missing
```

### 2d. Per-command checkpoint (resumability)

Long operations (btrfs balance, btrfs device remove) can be interrupted by power loss, SSH drop, or reboot.

**`cli/src/checkpoint.rs`** — New file:

```rust
#[derive(Serialize, Deserialize)]
struct OpCheckpoint {
    op: String,           // "add", "remove", "replace"
    disk: String,         // primary disk name
    step: u8,             // which step we're on
    started_at: String,   // ISO timestamp
    config_hash: String,  // sha256 of config.json at start
    args_hash: String,    // hash of command args (name, old, new, missing_id)
    pool_fingerprint: PoolFingerprint,  // topology at checkpoint time
    // replace-specific
    old_disk: Option<String>,
    new_disk: Option<String>,
}

/// Pool topology snapshot for checkpoint staleness detection.
/// Sorted by devid for deterministic comparison.
#[derive(Serialize, Deserialize, PartialEq)]
struct PoolFingerprint {
    devices: Vec<PoolFingerprintDevice>,  // sorted by devid
    missing_count: u32,
    total_devices: u32,
    mounted: bool,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct PoolFingerprintDevice {
    devid: u64,
    luks_uuid: Option<String>,
    mapper: String,
}

const CHECKPOINT_FILE: &str = "/var/lib/braid/op-state.json";
```

**Staleness rules** — checkpoint is invalidated (deleted with warning, reason printed) when:
- Config hash changed (user edited braid.disks and rebuilt)
- Pool topology changed (`pool_fingerprint` differs — device added/removed/failed since checkpoint)
- Different command or different args than what's checkpointed

**Behavior:**
- Written before starting each long step (balance, device remove)
- Cleared on successful completion
- On next invocation: if valid checkpoint exists (same op + config_hash + args_hash + pool_fingerprint), prompt "Previous `braid add ironwolf` was interrupted at step 3 (balance). Resume? [Y/n]"
- If stale checkpoint: auto-invalidate, start fresh, print reason ("config changed", "pool topology changed", "different command")
- btrfs balance and device remove are inherently resumable (btrfs tracks internal progress), so "resume" = re-invoke the same btrfs command

### 2e. `braid add <name>` — cli/src/add.rs

**New file.** Core logic extracted from current `init_disk.rs` and `apply.rs`.

Flow:
1. Read config, validate `name` exists in `braid.disks`
2. Probe disk state: absent → error. Present, check LUKS header.
3. **If LUKS header exists**: attempt open with provided passphrase
   - **luksOpen fails** (wrong passphrase) → error exit: "Failed to open LUKS device. Wrong passphrase?" Do **not** offer to reformat — the device has a LUKS header the user (or someone) created. Reformatting requires the user to re-run `add` after wiping the header explicitly.
   - **luksOpen succeeds** → check for btrfs superblock:
     - Superblock matches existing pool → "Already a pool member. Nothing to do." Exit 0.
     - Superblock for different pool → error, refuse
     - No superblock → prompt "Add to pool? [Y/n]", proceed to step 5
4. **If no LUKS header**: display disk info (model, serial, size), prompt destructive confirmation
   - Read passphrase
   - If pool exists: verify passphrase against existing pool member (reuse `init_disk.rs` ~140-175)
   - `cryptsetup luksFormat`
   - Backup LUKS header
   - `cryptsetup luksOpen`
5. **Pool operations** — the critical distinction:
   - **Pool exists**: never `mkfs.btrfs`. Just `btrfs device add`, then balance to RAID1 if 2+ disks (with checkpoint + progress).
   - **No pool (bootstrap)**: check btrfs superblock on mapper. If none → `mkfs.btrfs`. Then `mount`.
6. Print summary

**The `mkfs.btrfs` gate**: only runs during bootstrap when the opened device has no btrfs superblock. An existing pool never triggers mkfs — `btrfs device add` is the only mutation path. This prevents over-formatting a returning member.

**Safety argument** (addresses the collapsed init-disk/apply boundary): The old architecture used a structural code boundary — `luksFormat` literally unreachable from `apply`. The new architecture replaces this with a **superblock guard**: before any `mkfs.btrfs`, the code opens the LUKS device and checks for an existing btrfs superblock matching the pool. If found, the device is a returning member and `add` becomes a no-op. The btrfs superblock check is effectively an idempotent "is this already formatted?" primitive — the exact "revisit trigger" described in `safe-by-construction-reconciliation.md`. Combined with the explicit operator intent (user names a specific disk + confirms), this provides equivalent safety to the structural boundary. The decision doc (`intent-cli.md`) must argue this explicitly.

**Reuse from existing code:**
- `init_disk.rs`: passphrase verification, LUKS format, header backup, LUKS opts → extracted to `luks.rs`
- `apply.rs`: `execute_btrfs_add()` logic, bootstrap mount, balance with progress → extracted to `pool.rs`
- `probe.rs`: `probe_config_disk()`, `probe_pool()`
- `progress.rs`: balance progress display
- `cmd.rs`: all CmdRequest variants unchanged

### 2f. `braid remove <name>` — cli/src/remove.rs

**New file. Pool-authoritative**: resolves `<name>` against the pool (mapper `braid-<name>` or missing device), not config membership. Config may or may not still contain the disk — doesn't matter.

**Canonical workflow**: `braid remove <name>` → then edit `braid.disks` → `nixos-rebuild switch`. The remove command does the pool mutation; config cleanup is a separate step (advisory reminds you).

**Tab completion**: union of config disk names + pool mapper names (strip `braid-` prefix from `/dev/mapper/braid-*`). If pool probe fails, fall back to config names only.

**Flow:**
1. Probe pool state. Check if `braid-<name>` mapper exists in pool, or if pool has missing devices.
2. **Disk present and in pool** (mapper `braid-<name>` exists):
   - Removal leaves 0 disks → error, refuse
   - Removal leaves 1 disk → "WARNING: No redundancy. Type 'remove without redundancy':"
   - Otherwise → "Remove? Data will migrate off this disk. [y/N]"
   - Checkpoint → `btrfs device remove /dev/mapper/braid-<name>` with progress
   - `cryptsetup close braid-<name>`
3. **Disk absent, pool has missing device(s)**:
   - `--missing-id <devid>` provided → confirm → `btrfs device remove <devid> /mnt/storage` (targets exactly that device)
   - `--missing-id` **not** provided:
     - 1 missing → confirm → `btrfs device remove missing /mnt/storage` (safe: only one candidate)
     - Multiple missing → hard block: "Multiple missing devices; pass `--missing-id <devid>` (see `braid status --verbose`)"

   **Why require `--missing-id` with multiple missing**: `btrfs device remove missing` evicts **all** missing devices at once. With multiple missing, the user might intend to evict only one. `--missing-id` forces explicit targeting via `btrfs device remove <devid>`. With exactly 1 missing, `remove missing` is unambiguous.
4. Print "Done. If not already done: remove from braid.disks and run nixos-rebuild switch."

### 2g. `braid replace --old <disk> --new <disk>` — cli/src/replace.rs

**New file.** Single transactional intent: add new first, then evict dead. Redundancy never drops.

**Config requirement:** `--new` must be in `braid.disks` (config-first for the incoming disk). `--old` is resolved against the pool — it's dead/absent, so we identify it by its mapper name or via `--missing-id`.

**Flow:**
1. Validate `--new` exists in `braid.disks`. Probe `--new` disk state.
2. Resolve `--old`:
   - If mapper `braid-<old>` exists in pool and disk is present → error: "Disk is alive. Use `braid remove` + `braid add` separately."
   - If mapper `braid-<old>` is registered in pool but device missing → this is the target to evict
   - If `--old` not found in pool → check missing device count:
     - 0 missing → error: "No dead disk to replace"
     - 1 missing → use that as eviction target (confirm with user)
     - Multiple missing → require `--missing-id <devid>`
3. Compile step list (conditional on `--new` disk state):
   - If `--new` needs LUKS format: show "[destructive] LUKS format <new>"
   - If `--new` already LUKS-formatted: show "[safe] LUKS open <new>"
   - Always: "btrfs device add", "btrfs balance to RAID1"
   - Eviction: if `--missing-id <devid>` → "btrfs device remove <devid>"; otherwise → "btrfs device remove missing"
4. Confirm, read passphrase
5. Execute with checkpoints: init new if needed → open → add to pool → balance → evict dead (using `pool_remove_devid` or `pool_remove_missing` per step 3)
6. Print summary

Ordering guarantee: new disk added and data rebalanced **before** dead disk evicted.

### 2h. Shared helpers

**`cli/src/luks.rs`** — extracted from `init_disk.rs`:
- `luks_format(runner, device, passphrase, opts)` — luksFormat + header backup
- `verify_passphrase(runner, existing_device, passphrase)` — test against pool member
- `read_passphrase(common_args)` — TTY / env / file (see 2b)
- `device_has_btrfs_superblock(runner, mapper_path)` — check btrfs membership
- `ensure_luks_open(runner, disk, passphrase)` — open if closed, skip if open

**`cli/src/pool.rs`** — extracted from `apply.rs`:
- `pool_add_device(runner, device, mount_point)` — btrfs device add
- `pool_balance_raid1(runner, mount_point, progress)` — balance with progress
- `pool_remove_device(runner, device, mount_point, progress)` — graceful remove with progress
- `pool_remove_missing(runner, mount_point)` — `btrfs device remove missing <mount>` (evicts **all** missing devices)
- `pool_remove_devid(runner, mount_point, devid: u64)` — `btrfs device remove <devid> <mount>` (targets one specific device)
- `pool_bootstrap_mount(runner, device, mount_point)` — mount for first disk (mkfs gated on no superblock)

**`--missing-id` command mapping**: When `--missing-id <devid>` is provided, the btrfs command is `btrfs device remove <devid> <mount_point>` — this targets exactly one device. Without `--missing-id`, the command is `btrfs device remove missing <mount_point>` — this evicts **all** missing devices at once. These are distinct btrfs operations and must not be conflated. `pool_remove_missing` and `pool_remove_devid` are separate functions to make the distinction unambiguous at call sites.

**`cli/src/checkpoint.rs`** — per-command checkpoint (see 2d)

### 2i. Delete old files

- **Delete `cli/src/plan.rs`** (~1000+ lines)
- **Delete `cli/src/apply.rs`** (~2000+ lines)
- **Delete `cli/src/init_disk.rs`** — absorbed into luks.rs + add.rs

### 2j. Update status.rs and doctor.rs

- Status displays named disks: `toshiba  12TB  Toshiba MN07ACA12T  healthy  0 errors`
- Status `--verbose` includes btrfs devid per disk (needed for `--missing-id`)
- Doctor checks updated for new config format

### 2k. Comment out TUI

**`cli/src/main.rs`** — remove `Tui` from Commands enum. TUI module files stay in tree but aren't compiled into the binary. Revisit after intent CLI is stable.

---

## Phase 3: NixOS Advisory Activation

**`modules/braid/cli.nix`** — Add activation script that prints guidance on `nixos-rebuild switch`.

**Matching by LUKS UUID** (not mapper name) to avoid false advice during any naming transitions:

```nix
system.activationScripts.braidAdvisory.text = ''
  if [ -e ${cfg.mountPoint} ] && command -v btrfs >/dev/null 2>&1; then
    # 1. Get pool members: btrfs filesystem show → extract /dev/mapper/* paths
    # 2. For each mapper, get LUKS UUID via cryptsetup luksUUID on underlying device
    # 3. For each config disk, get LUKS UUID via cryptsetup luksUUID on byId path
    # 4. Compare UUID sets:
    #    - Config UUID not in pool → "braid: new disk: <name> → run: sudo braid add <name>"
    #    - Pool UUID not in config → "braid: disk removed: <name> → run: sudo braid remove <name>"
    #    - Config disk not LUKS-formatted → "braid: uninitialized: <name> → run: sudo braid add <name>"
    # 5. All match → "braid: pool healthy, config matches."
  fi
'';
```

Best-effort: failures silenced, never mutates. UUID comparison is robust across mapper name changes.

---

## Phase 4: systemd-native Pool Unlock

For the "missed initrd window" scenario. `systemctl start braid-pool.target` brings the pool online from a normal SSH session.

**Design choice: single orchestrator service** instead of per-disk services. This guarantees one passphrase prompt, avoids relying on `systemd-ask-password` cache sharing behavior (which depends on `--id` matching and agent timing).

**`modules/braid/storage.nix`** — Add stage-2 units:

```nix
# Single service that opens all LUKS and mounts pool
systemd.services.braid-unlock = {
  description = "Open LUKS and mount braid pool";
  serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
  unitConfig.ConditionPathIsMountPoint = "!${cfg.mountPoint}";
  script = ''
    passphrase=$(${pkgs.systemd}/bin/systemd-ask-password \
      --id=braid "LUKS passphrase for braid pool:")

    opened=""

    # Open each disk — tolerate missing/dead disks (degraded is fine)
    ${lib.concatMapStringsSep "\n" (name: let disk = cfg.disks.${name}; in ''
      if [ -e /dev/mapper/braid-${name} ]; then
        opened="$opened /dev/mapper/braid-${name}"
      elif [ -e ${disk.byId} ]; then
        if echo "$passphrase" | ${cfg.packages.cryptsetup}/bin/cryptsetup \
            luksOpen --key-file=- ${disk.byId} braid-${name} 2>/dev/null; then
          opened="$opened /dev/mapper/braid-${name}"
        else
          echo "braid-unlock: WARNING: failed to open ${name} — skipping" >&2
        fi
      else
        echo "braid-unlock: WARNING: ${name} not present — skipping" >&2
      fi
    '') (builtins.attrNames cfg.disks)}

    if [ -z "$opened" ]; then
      echo "braid-unlock: ERROR: no disks opened, cannot mount pool" >&2
      exit 1
    fi

    # Assemble pool from whatever devices are available
    ${cfg.packages.btrfsProgs}/bin/btrfs device scan

    # Mount using first successfully-opened mapper (not first configured name)
    first_mapper=$(echo $opened | ${pkgs.coreutils}/bin/cut -d' ' -f1)
    ${cfg.packages.utilLinux}/bin/mount -o degraded "$first_mapper" ${cfg.mountPoint} || {
      # Try without -o degraded in case all members are present
      ${cfg.packages.utilLinux}/bin/mount "$first_mapper" ${cfg.mountPoint}
    }
  '';
};

systemd.targets.braid-pool = {
  description = "Braid storage pool online";
  wants = [ "braid-unlock.service" ];
  after = [ "braid-unlock.service" ];
};
```

**Degraded recovery**: The service tolerates missing/dead disks — it opens whatever is available, then mounts with `-o degraded` if needed. The mount device is the first *successfully opened* mapper, not the first configured name (which might be the dead disk). If no disks can be opened at all, the service fails explicitly.

Usage: `systemctl start braid-pool.target` → one passphrase prompt → all available LUKS open → pool mounted (possibly degraded). Works from TTY, SSH, or scripted.

---

## Phase 5: Tests

### 5a. Rust unit tests

Update tests in `config.rs`, `probe.rs`, `types.rs` for named-disk config. Add unit tests for add.rs, remove.rs, replace.rs, luks.rs, pool.rs, checkpoint.rs using MockRunner.

### 5b. NixOS VM tests

| Test | Validates |
|------|-----------|
| `braid-add-first-disk` | `braid add toshiba` creates pool from scratch |
| `braid-add-second-disk` | `braid add ironwolf` joins pool, converts to RAID1 |
| `braid-add-idempotent` | `braid add toshiba` on existing pool member = no-op |
| `braid-add-refuses-unknown` | `braid add unknown` errors with helpful message |
| `braid-add-no-overformat` | `braid add` on existing pool never runs mkfs.btrfs |
| `braid-add-dry-run` | `--dry-run` prints conditional steps without executing |
| `braid-add-already-luks` | `braid add` on pre-formatted disk skips luksFormat |
| `braid-remove-graceful` | `braid remove ironwolf` migrates data, detaches |
| `braid-remove-redundancy-warning` | 2→1 disk requires typed confirmation |
| `braid-remove-multi-missing-blocks` | Multiple missing devices blocks without `--missing-id` |
| `braid-replace-dead-disk` | `braid replace --old ironwolf --new seagate` full flow |
| `braid-replace-ordering` | New disk added + rebalanced before dead evicted |
| `braid-replace-already-init` | Replace with pre-formatted new disk skips luksFormat |
| `braid-replace-multi-missing` | Requires `--missing-id` when >1 device missing |
| `braid-status-healthy` | All disks healthy, shows names + devids |
| `braid-status-degraded` | Degraded pool shows missing disk info |
| `braid-reboot-unlock` | Reboot → initrd SSH → passphrase → pool online |
| `braid-pool-target` | `systemctl start braid-pool.target` → single passphrase prompt → pool online |
| `braid-pool-target-degraded` | `systemctl start braid-pool.target` with one disk dead → pool mounts degraded |
| `braid-advisory` | `nixos-rebuild switch` prints UUID-based guidance |
| `braid-named-disks` | Mapper names are `braid-<name>` |
| `braid-checkpoint-resume` | Interrupted balance resumes on re-run |
| `braid-checkpoint-stale` | Config change invalidates stale checkpoint |
| `braid-non-interactive` | `--yes` + `BRAID_PASSPHRASE` works for scripting |

### 5c. Tests to retire

Plan/apply-specific: `15-braid-plan-rust.nix`, `16-braid-apply-rust.nix`, `braid-apply-rust.py`, `braid-plan-rust.py`.

Fundamental tests (LUKS, btrfs, degraded boot, remote unlock, samba, shell completion) kept and updated for named disks.

---

## Phase 6: Docs

### 6a. Decision doc

**New: `docs/decisions/intent-cli.md`** — Status: Active. Supersedes `docs/decisions/unified-cli.md` and `docs/decisions/two-phase-apply.md` (both exist in repo today).

Must address head-on:
- Why plan/apply was replaced (usage pattern mismatch, complexity budget)
- Why collapsing init-disk into add is safe (superblock guard = the "idempotent format primitive" revisit trigger from `safe-by-construction-reconciliation.md`)
- The new safety model: explicit operator intent + btrfs superblock membership check + confirmation calibrated to risk
- Resumability: per-command checkpoint with staleness rules

### 6b. Update principles.md

- **Principle 2** (Config-first): update workflow — `edit config → rebuild → braid add`
- **Principle 3** (Safe-by-construction): reframe from "structural code boundary" to "explicit operator intent + mkfs gated on bootstrap-only + superblock guard"
- **Principle 5** (Stable identifiers): update — mapper names are now `braid-<user-chosen-name>` instead of by-id basenames. Rationale: human-friendly names are more debuggable in `lsblk`, systemd logs, and error messages than `ata-Toshiba_MN07ACA12TEA_XXXXXXXXXXXX`. Still deterministic (derived from config), just user-controlled.

### 6c. Update README.md

Full rewrite of "Managing drives" with named disk examples. Document `--dry-run`, `--yes`, `--passphrase-file`, `--missing-id`. Document `systemctl start braid-pool.target` for post-boot unlock.

### 6d. Update AGENTS.md

Commands section, test conventions for new test names.

---

## Files Modified/Created/Deleted

### Modified
- `modules/braid/options.nix` — disks: listOf str → attrsOf submodule
- `modules/braid/storage.nix` — named mappers, braid-unlock.service, braid-pool.target
- `modules/braid/cli.nix` — new config.json format, advisory activation script (UUID-based)
- `cli/src/main.rs` — Commands: Add, Remove, Replace, Status, Doctor (TUI commented out)
- `cli/src/config.rs` — BTreeMap disks, named-disk helpers
- `cli/src/types.rs` — delete plan/apply types, keep probe types
- `cli/src/probe.rs` — named disks, `braid-<name>` mappers
- `cli/src/status.rs` — display named disks + devids
- `cli/src/doctor.rs` — new config format checks
- `cli/src/lib.rs` — update exports
- `docs/principles.md` — update principles 2, 3, 5
- `README.md` — rewrite managing drives
- `AGENTS.md` — update commands

### Created
- `cli/src/add.rs` — braid add (LUKS init + pool join, superblock guard)
- `cli/src/remove.rs` — braid remove (graceful + missing, --missing-id)
- `cli/src/replace.rs` — braid replace (transactional: add-first-then-evict)
- `cli/src/luks.rs` — LUKS helpers (format, verify, passphrase input, superblock check)
- `cli/src/pool.rs` — pool helpers (add device, balance, remove, bootstrap mount)
- `cli/src/checkpoint.rs` — per-command checkpoint/resume with staleness rules
- `docs/decisions/intent-cli.md` — supersedes unified-cli.md + two-phase-apply.md

### Deleted
- `cli/src/plan.rs`
- `cli/src/apply.rs`
- `cli/src/init_disk.rs`

---

## Verification

1. `cargo test` — all Rust unit tests pass
2. `make test` — all NixOS VM tests pass
3. Smoke test in VM: `braid add toshiba` → `braid status` → reboot → initrd unlock → `braid add ironwolf` → RAID1
4. `braid add ironwolf --dry-run` shows conditional steps (format or not) matching actual disk state
5. `braid add ironwolf --yes` with `BRAID_PASSPHRASE` works non-interactively
6. `nixos-rebuild switch` prints UUID-based advisory for config changes
7. `systemctl start braid-pool.target` prompts once, opens all LUKS, mounts pool
8. Tab completion returns disk names
9. `braid add unknown-name` shows helpful error pointing to config
10. Interrupted balance resumes on re-run; config change invalidates stale checkpoint
11. `braid replace` with pre-formatted new disk skips luksFormat in both execution and dry-run output
12. `braid add` on LUKS-formatted disk with wrong passphrase → clean error exit (no reformat offer)
13. `systemctl start braid-pool.target` with one dead disk → pool mounts degraded
