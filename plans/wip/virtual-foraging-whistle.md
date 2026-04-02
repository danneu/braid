# Plan: Show actual commands in dry-run output

## Context

Dry-run currently shows human-readable descriptions like `[destructive] LUKS format /dev/disk/by-id/disk1` but not the actual commands that would execute. Users want to see the exact commands (e.g., `cryptsetup luksFormat --type luks2 --batch-mode ...`) for transparency, debugging, and learning.

Four commands already support `--dry-run`: **add**, **remove**, **remove-missing**, **replace**. Each defines its own identical step struct and print loop. This plan unifies them and adds command rendering.

## Target output shape

```
[destructive] LUKS format /dev/disk/by-id/disk1
                → cryptsetup luksFormat --type luks2 --batch-mode '--key-file=-' --label braid-aaa /dev/disk/by-id/disk1
[safe       ] LUKS header backup
                → cryptsetup luksHeaderBackup '--header-backup-file=/var/lib/braid/luks-headers/braid-aaa.luksheader' /dev/disk/by-id/disk1
[safe       ] LUKS open → braid-aaa
                → cryptsetup open --type luks '--key-file=-' /dev/disk/by-id/disk1 braid-aaa
[safe       ] btrfs device add /dev/mapper/braid-aaa /mnt/storage
                → btrfs device add /dev/mapper/braid-aaa /mnt/storage
[long       ] btrfs balance to RAID1
                → btrfs balance start -dconvert=raid1 -mconvert=raid1 /mnt/storage
```

Steps with no concrete command (deferred verification) show only the description line. Every `CmdRequest` that the execution path would fire is represented.

## Architecture changes

### 1. Shell-safe rendering — `cli/src/cmd.rs`

Use `shell_words::join` (already a dependency) for correct quoting:

```rust
impl CmdArgs {
    pub fn to_shell_string(&self) -> String {
        let argv: Vec<&str> = std::iter::once(self.program)
            .chain(self.args.iter().map(|s| s.as_str()))
            .collect();
        shell_words::join(&argv)
    }
}
```

This handles args with `=`, `-`, spaces, or other shell-significant characters correctly. Add unit tests for quoting edge cases (paths with spaces, `--key-file=-`).

### 2. Shared `Step` type with `Vec<CmdRequest>` — `cli/src/cmd.rs`

Store typed `CmdRequest`s on the step; render at print time. Keeps dry-run output derived from the same source as execution, avoids string drift, and centralizes quoting.

```rust
/// A step in a dry-run plan.
#[derive(Debug)]
pub struct Step {
    pub risk: &'static str,
    pub description: String,
    pub commands: Vec<CmdRequest>,
}

impl Step {
    /// Pure renderer — returns the formatted dry-run lines. Testable without stdout capture.
    pub fn render_dry_run(steps: &[Step]) -> String {
        let mut out = String::new();
        for step in steps {
            out.push_str(&format!("[{:<11}] {}\n", step.risk, step.description));
            for cmd in &step.commands {
                out.push_str(&format!("               → {}\n", cmd.to_argv().to_shell_string()));
            }
        }
        out
    }

    /// Print dry-run plan to stdout.
    pub fn print_dry_run(steps: &[Step]) {
        print!("{}", Self::render_dry_run(steps));
    }
}
```

### 3. Delete per-command step structs

Remove these 4 identical structs:
- `AddStep` — `cli/src/add.rs:43-46`
- `RemoveStep` — `cli/src/remove.rs:28-31`
- `RemoveMissingStep` — `cli/src/remove_missing.rs:28-31`
- `ReplaceStep` — `cli/src/replace.rs:36-39`

Each file switches to `use crate::cmd::Step;`

### 4. Replace per-command print loops with `Step::print_dry_run`

All 4 identical loops at:
- `cli/src/add.rs:341-345`
- `cli/src/remove.rs:112-116`
- `cli/src/remove_missing.rs:146-149`
- `cli/src/replace.rs:122-126`

become: `Step::print_dry_run(&steps);`

### 5. Add parameters to compile functions

Two compile functions need additional params for `CmdRequest` construction:

- `compile_remove_present_steps` (`cli/src/remove.rs:230`) — add `mount_point: &MountPoint`
- `compile_steps` (`cli/src/remove_missing.rs:276`) — add `mount_point: &MountPoint`

For complete command coverage in add/replace, the compile functions also need:

- `compile_add_steps_multi` (`cli/src/add.rs:573`) — add `paths: &StatePaths`, `enroll_key_file: Option<&Path>`
- `compile_replace_steps` (`cli/src/replace.rs:464`) — add `paths: &StatePaths`, `enroll_key_file: Option<&Path>`

All callers already have these values available.

### 6. Update step construction sites — full command coverage

Each step carries `commands: vec![CmdRequest::...]` matching the exact CmdRequests the execution path fires.

#### add.rs — `compile_add_steps_multi`

Call `luks_opts_from_env()` at top of function for LUKS format steps.

**PresentNotLuks disk (fresh disk init):**

| Step | Risk | Commands |
|------|------|----------|
| LUKS format {by_id} | destructive | `CryptsetupLuksFormat { device, extra_opts: env_opts + [--label, braid-{name}] }` |
| LUKS header backup | safe | `CryptsetupLuksHeaderBackup { device, backup_path }` |
| LUKS open → {mn} | safe | `CryptsetupLuksOpen { device, mapper }` |
| keyfile enroll (if --enroll-key-file) | safe | `CryptsetupLuksAddKeyFile { device, key_file_path }` |

**PresentLuks (recovery, mapper open):**

| Step | Risk | Commands |
|------|------|----------|
| btrfs device add (recovery) | safe | `BtrfsDeviceAdd { device: mapper_path, mount_point }` |

**PresentLuks (mapper closed, deferred):**

| Step | Risk | Commands |
|------|------|----------|
| LUKS open + identity verification at execution time | safe | `CryptsetupLuksOpen { device, mapper }` |

**Pool phase (no pool mounted):**

| Step | Risk | Commands |
|------|------|----------|
| mkfs.btrfs RAID1 (≥2 disks) | safe | `MkfsBtrfsRaid1 { devices }` |
| mkfs.btrfs (1 disk) | safe | `MkfsBtrfs { device }` |
| mount | safe | `Mount { device: first_mapper, mount_point }` |

**Pool phase (pool already mounted):**

| Step | Risk | Commands |
|------|------|----------|
| btrfs device add | safe | `BtrfsDeviceAdd { device, mount_point }` |
| btrfs balance to RAID1 | long | `BtrfsBalanceRaid1 { mount_point }` |

#### remove.rs — `compile_remove_present_steps`

Add `mount_point: &MountPoint`.

| Step | Risk | Commands |
|------|------|----------|
| btrfs balance RAID1→single (if remaining==1) | long | `BtrfsBalanceSingle { mount_point }` |
| btrfs device remove | long | `BtrfsDeviceRemove { device: mapper_path, mount_point }` |
| cryptsetup close | safe | `CryptsetupClose { mapper }` |

#### remove_missing.rs — `compile_steps`

Add `mount_point: &MountPoint`.

| Step | Risk | Commands |
|------|------|----------|
| btrfs device remove {devid} / missing | long | `BtrfsDeviceRemove { device: devid }` or `BtrfsDeviceRemoveMissing { mount_point }` |
| btrfs balance soft RAID1 (if clearing last missing) | long | `BtrfsBalanceRaid1Soft { mount_point }` |

#### replace.rs — `compile_replace_steps`

Add `paths: &StatePaths`, `enroll_key_file: Option<&Path>`. Call `luks_opts_from_env()` at top.

**PresentNotLuks (fresh disk):**

| Step | Risk | Commands |
|------|------|----------|
| LUKS format {by_id} | destructive | `CryptsetupLuksFormat { device, extra_opts }` |
| LUKS header backup | safe | `CryptsetupLuksHeaderBackup { device, backup_path }` |
| LUKS open → {mn} | safe | `CryptsetupLuksOpen { device, mapper }` |
| keyfile enroll (if --enroll-key-file) | safe | `CryptsetupLuksAddKeyFile { device, key_file_path }` |

**PresentLuks (mapper closed):**

| Step | Risk | Commands |
|------|------|----------|
| LUKS open → {mn} | safe | `CryptsetupLuksOpen { device, mapper }` |

**Replace operation (both Live and Missing source):**

| Step | Risk | Commands |
|------|------|----------|
| btrfs replace start | long | `BtrfsReplaceStart { devid, target_device, mount_point }` |
| btrfs filesystem resize | safe | `BtrfsFilesystemResize { devid, mount_point }` |
| cryptsetup close (Live only) | safe | `CryptsetupClose { mapper: old_mapper }` |
| btrfs balance soft RAID1 (Missing, last missing) | long | `BtrfsBalanceRaid1Soft { mount_point }` |

## Files modified

| File | Change |
|------|--------|
| `cli/src/cmd.rs` | Add `CmdArgs::to_shell_string()`, `Step` struct with `Vec<CmdRequest>`, `Step::print_dry_run()` |
| `cli/src/add.rs` | Delete `AddStep`, use `Step`, add `paths`/`enroll_key_file` to compile fn, add header backup + keyfile enrollment steps |
| `cli/src/remove.rs` | Delete `RemoveStep`, use `Step`, add `mount_point` to compile fn |
| `cli/src/remove_missing.rs` | Delete `RemoveMissingStep`, use `Step`, add `mount_point` to compile fn |
| `cli/src/replace.rs` | Delete `ReplaceStep`, use `Step`, add `paths`/`enroll_key_file` to compile fn, add header backup + keyfile enrollment steps |

## Verification — TDD approach

### Step 1: Write failing tests first

Add tests in each command's `mod tests` that assert exact dry-run output for representative scenarios. Tests call the compile function then `Step::render_dry_run()` and assert against the returned string:
- Correct step ordering
- Exact command strings (via `to_shell_string()`)
- Multi-command steps (LUKS format + header backup + open)
- Presence/absence of conditional steps (keyfile enrollment, balance)
- Shell quoting correctness

Also add unit tests for `CmdArgs::to_shell_string()` in `cmd.rs::tests`:
- Simple args: `btrfs device add /dev/mapper/braid-aaa /mnt/storage`
- Args with `=` and `-`: `cryptsetup luksFormat --type luks2 --key-file=-`
- Path with spaces (defensive): mount point like `/mnt/my storage` gets quoted

### Step 2: Implement the refactor

Apply all architecture changes above to make the failing tests pass.

### Step 3: Run full suite

1. `just test-rust` — all unit tests including new ones
2. `just test` — VM integration tests (no regressions)
