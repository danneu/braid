# `btrfs replace -r`: What It Does, What braid Should Do, and How to Test It

> Research from a deep dive into btrfs RAID1 best practices, the `-r` flag internals,
> and braid's current replace implementation.

---

## Table of Contents

- [What `-r` Does (Precisely)](#what--r-does-precisely)
- [What braid Currently Does](#what-braid-currently-does)
- [What braid Should Do](#what-braid-should-do)
- [TDD Test Strategy](#tdd-test-strategy)

---

## What `-r` Does (Precisely)

At the kernel level, `-r` sets `BTRFS_IOCTL_DEV_REPLACE_CONT_READING_FROM_SRCDEV_MODE_AVOID`
(value 1) vs the default `MODE_ALWAYS` (value 0), defined in `include/uapi/linux/btrfs.h`:

```c
#define BTRFS_IOCTL_DEV_REPLACE_CONT_READING_FROM_SRCDEV_MODE_ALWAYS  0
#define BTRFS_IOCTL_DEV_REPLACE_CONT_READING_FROM_SRCDEV_MODE_AVOID   1
```

### Behavior Comparison

| | Without `-r` | With `-r` |
|---|---|---|
| **Primary read source** | The old (source) device | The other mirrors in the array |
| **Falls back to source** | N/A — always reads source | Only if no other zero-defect mirror exists |
| **Failing drive (bad sectors)** | Hits every bad sector → retries → timeouts → agonizingly slow | Skips failing drive → reads mirrors → fast |
| **Completely missing drive** | Kernel auto-detects, uses mirrors anyway | Same behavior (explicit) |
| **Healthy drive swap** | Optimal I/O: source reads, target writes | Shifts read load to other mirrors (slightly suboptimal) |
| **Correctness** | Identical | Identical |

### The Key Scenario

A drive that is **partially failing** — still present in the system, responds to some reads,
but has bad sectors or is very slow.

- **Without `-r`:** btrfs attempts to read from the failing drive for every block. Each bad
  sector triggers kernel I/O retries and timeouts. Replacement takes hours or days longer
  than necessary.
- **With `-r`:** btrfs skips the failing drive and reads from mirrors. Replacement completes
  at normal speed.

### When Each Makes Sense

| Scenario | Without `-r` | With `-r` |
|---|---|---|
| Healthy drive (upgrade to larger) | Optimal — one drive reads, one writes | Unnecessary load on other mirrors |
| Degrading drive (SMART warnings, bad sectors) | Extremely slow, retries/timeouts | Fast, avoids bad drive |
| Missing drive (physically removed) | Kernel auto-falls back to mirrors | Same result (explicit) |

### Downside of Always Using `-r`

On a healthy source drive, `-r` shifts read I/O onto the remaining drives instead of reading
from the source. In a 2-drive RAID1, this means the one remaining drive must serve both the
replace reads AND any concurrent filesystem I/O, while the source drive sits idle. On a busy
NAS during a planned replacement, this could matter. On an idle NAS, negligible.

**There is no correctness downside** — `-r` does not risk data loss or corruption. It is
purely a performance consideration.

---

## What braid Currently Does

braid has **two replacement paths**, determined by whether the old disk is live or dead.

### Live Path (old drive present in pool)

**Code:** `cli/src/replace.rs:222-276`, command built in `cli/src/cmd.rs:362-379`

```rust
// cmd.rs — actual command construction
"btrfs", "replace", "start", "-f", "-B", &devid_str, target_device, mount_point
```

Runs: `btrfs replace start -f -B <devid> <target> <mount>`

- `-f` = force (don't prompt)
- `-B` = run in background (braid polls status separately)
- **No `-r`**

After replace completes:
1. `btrfs filesystem resize <devid>:max <mount>` — expand to full capacity
2. `cryptsetup close <old_mapper>` — best-effort LUKS close

**Properties:** Preserves devid. Fast. Single operation.

### Dead Path (old drive missing)

**Code:** `cli/src/replace.rs:277-297`

Does **not** use `btrfs replace` at all. Uses three-step add+balance+remove:

1. `btrfs device add -f <new_mapper> <mount>`
2. `btrfs balance start -dconvert=raid1 -mconvert=raid1 <mount>`
3. `btrfs device remove missing <mount>` (or `btrfs device remove <devid> <mount>`)

**Properties:** Does not preserve devid. Slower (full balance). Has intermediate state where
pool has an extra device with mixed profiles.

### Eviction Target Resolution

```
old disk in pool + no missing → Live path (btrfs replace)
old disk NOT in pool + 1 missing → Dead path (add+balance+remove, auto-detect)
old disk NOT in pool + --missing-id → Dead path (add+balance+remove, explicit devid)
```

---

## What braid Should Do

### Change 1: Add `-r` to the live path

```diff
- &["replace", "start", "-f", "-B", &devid_str, target_device, mount_point]
+ &["replace", "start", "-r", "-f", "-B", &devid_str, target_device, mount_point]
```

**Rationale:** braid cannot distinguish "healthy swap" from "dying drive still responding."
The downside of `-r` on a healthy drive is negligible. The upside on a degrading drive is
massive (hours faster). Always passing `-r` is the safe default.

The live path already detects and warns about I/O errors on the source device
(`replace.rs:226-247`), which further supports the case that the source may be degrading.

### Change 2: Dead path should use `btrfs replace -r` instead of add+balance+remove

`btrfs replace` works fine with missing devices — you just need the devid, which braid
already resolves. So the dead path could become:

```
btrfs replace start -r -f -B <devid> <new_device> <mount>
```

Benefits over add+balance+remove:

| Aspect | add+balance+remove | btrfs replace -r |
|---|---|---|
| **Speed** | Full balance of all chunks | Direct targeted copy |
| **Preserves devid** | No | Yes |
| **Complexity** | Three operations | One operation |
| **Intermediate state** | Pool has extra device with mixed profiles | No intermediate state |
| **Failure modes** | Balance can ENOSPC; remove can fail independently | Single atomic operation |

**Caveat:** A post-replace soft balance is still needed to clean up single-profile chunks
written while the pool was mounted degraded:

```
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft <mount>
```

This applies regardless of which replace strategy is used — it's a consequence of degraded
mode, not the replacement method.

---

## TDD Test Strategy

### Approach 1: Rust Unit Test (fast, `cargo test`)

Test that the `CmdRequest::BtrfsReplaceStart` variant generates args containing `-r`.

```rust
#[test]
fn btrfs_replace_start_uses_read_from_mirrors_flag() {
    // The -r flag tells btrfs to read from mirrors instead of the source
    // device during replacement. This is critical for degrading drives.
    let cmd = CmdRequest::BtrfsReplaceStart {
        devid: 2,
        target_device: "/dev/mapper/braid-new".into(),
        mount_point: "/mnt/storage".into(),
    };
    let args = cmd.to_args(); // expose this method
    assert!(args.contains(&"-r"), "replace start must use -r flag");
}
```

### Approach 2: NixOS VM Test (end-to-end, slower)

Wrap `btrfs` with a shim that logs invocations, then assert `-r` was passed:

```python
# Test: replace-uses-mirrors-flag
#
# Intent:
# - Verify that `braid replace` passes the `-r` flag to `btrfs replace start`,
#   so replacement reads from healthy mirrors rather than the source device.
#
# Why it exists:
# - Without `-r`, replacing a degrading (but still present) drive hits every bad
#   sector, triggering kernel I/O retries and making the operation agonizingly
#   slow. `-r` bypasses the source and reads from mirrors. Since braid can't
#   distinguish healthy-swap from dying-drive, it should always pass `-r`.
#
# Scenario:
# - Operator notices SMART warnings on a drive. The drive is still responding
#   but has growing bad sectors. They run `braid replace` to proactively swap
#   it out. Without `-r`, the replace reads from the dying drive and takes
#   hours. With `-r`, it reads from mirrors and finishes quickly.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new, extra=""):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid replace --old {old} --new {new} --passphrase-stdin --yes {extra}"
    )


# --- Setup: install a btrfs wrapper that logs invocations ---

with subtest("Install btrfs argument logger"):
    machine.succeed("""
        cp $(which btrfs) /usr/local/bin/btrfs-real
        cat > /usr/local/bin/btrfs-wrapper << 'WRAPPER'
#!/bin/bash
echo "$@" >> /tmp/btrfs-invocations.log
exec /usr/local/bin/btrfs-real "$@"
WRAPPER
        chmod +x /usr/local/bin/btrfs-wrapper
        ln -sf /usr/local/bin/btrfs-wrapper /run/current-system/sw/bin/btrfs
    """)

# --- Build 3-drive pool ---

with subtest("Setup: build 3-drive pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'test data' > /mnt/storage/test.txt && sync")

# Clear the log before the replace operation
machine.succeed("rm -f /tmp/btrfs-invocations.log")

# --- Live replace disk2 -> disk4 ---

with subtest("Live replace disk2 with disk4"):
    machine.succeed(replace_cmd("disk2", "disk4"))

# --- Assert -r flag was used ---

with subtest("btrfs replace start was called with -r flag"):
    log = machine.succeed("cat /tmp/btrfs-invocations.log")
    print(f"btrfs invocations:\n{log}")

    replace_lines = [l for l in log.splitlines() if "replace start" in l]
    assert len(replace_lines) > 0, (
        f"Expected at least one 'replace start' invocation, got none.\n"
        f"Full log:\n{log}"
    )

    for line in replace_lines:
        assert "-r" in line.split(), (
            f"Expected -r flag in btrfs replace start, got: {line}"
        )

with subtest("Data intact after replace"):
    content = machine.succeed("cat /mnt/storage/test.txt").strip()
    assert content == "test data", f"Expected 'test data', got '{content}'"

machine.shutdown()
```

### Recommended TDD Sequence

1. **Write the Rust unit test** asserting `-r` is in the generated args → watch it fail
2. **Add `-r` to `BtrfsReplaceStart` in `cmd.rs`** → unit test passes
3. **Run existing `replace-live-disk` VM test** → confirms no regression
4. Optionally add the NixOS VM wrapper test for extra confidence

---

## Sources

- [btrfs-replace(8) — Official BTRFS documentation](https://btrfs.readthedocs.io/en/latest/btrfs-replace.html)
- [btrfs-replace(8) — Linux man page](https://man7.org/linux/man-pages/man8/btrfs-replace.8.html)
- [Guide on replacing a disk (Forza's Ramblings)](https://wiki.tnonline.net/w/Btrfs/Replacing_a_disk)
- [Linux kernel btrfs.h header](https://github.com/torvalds/linux/blob/master/include/uapi/linux/btrfs.h)
- [Btrfs: Add device replace code (LWN.net)](https://lwn.net/Articles/524589/)
- [Btrfs RAID1 recovery (Axllent)](https://www.axllent.org/docs/btrfs-raid1-recovery/)
