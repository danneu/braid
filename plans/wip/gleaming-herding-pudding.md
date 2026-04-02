# Optimize hw ENOSPC test: adaptive coarse-to-fine fill

## Context

`tests/hw/test_remove_under_pressure.py` Phase 2 fills a 3x500GB RAID1 pool 1GB at a time, checking `braid remove --dry-run` after every write. With ~750GB usable capacity, this means ~750 iterations of (dd + sync + dry-run). Takes hours and wears drives unnecessarily.

## Approach

Replace the blind 1GB iteration loop with an adaptive coarse-to-fine strategy using live `btrfs device usage --raw` readings and the same RAID1-capacity math as production (`cli/src/preflight.rs:141-188`).

**Coarse phase:** Query device usage, compute headroom (RAID1 capacity minus target device's allocation, per type — same formula as `check_raid1_relocation_space`). Write headroom/2 in one `dd` call. After each write, run a real `braid remove --dry-run` as a guardrail — if it rejects, stop immediately (the headroom calc is approximate; the production dry-run is authoritative). Repeat until headroom < 5GB.

**Fine phase:** Switch to existing 1GB `dd` + dry-run check loop for the final approach to the threshold.

All writes use `dd` (real I/O), preserving the test's purpose of exercising real hardware behavior.

## Changes

**File: `tests/hw/test_remove_under_pressure.py`**

### 1. Add `parse_device_usage()` helper

Follow the regex style from `tests/repro/btrfs-remove-enospc-crash.py:39-52` (`get_device_unallocated()`), extended to also capture per-type allocations.

```python
def parse_device_usage():
    """Parse btrfs device usage --raw output.

    Returns list of dicts: {path, unallocated, allocations: {Data: n, Metadata: n, System: n}}
    """
    raw = run(f"btrfs device usage --raw {MOUNT_POINT}")
    devices = []
    current = None
    for line in raw.splitlines():
        dev_match = re.match(r"^(\S.*?), ID:", line)
        if dev_match:
            if current:
                devices.append(current)
            current = {"path": dev_match.group(1), "unallocated": 0, "allocations": {}}
            continue
        if current is None:
            continue
        unalloc_match = re.match(r"\s+Unallocated:\s+(-?\d+)", line)
        if unalloc_match:
            current["unallocated"] = max(0, int(unalloc_match.group(1)))
            continue
        alloc_match = re.match(r"\s+(Data|Metadata|System),\S+:\s+(\d+)", line)
        if alloc_match:
            atype = alloc_match.group(1)
            current["allocations"][atype] = (
                current["allocations"].get(atype, 0) + int(alloc_match.group(2))
            )
    if current:
        devices.append(current)
    return devices
```

### 2. Add `raid1_headroom()` helper

Same per-type formula as `check_raid1_relocation_space` in `cli/src/preflight.rs:141-188`. Returns minimum headroom across all allocation types.

```python
def raid1_headroom(devices, target_path):
    """Minimum RAID1 capacity headroom before preflight rejects removal of target_path."""
    target = [d for d in devices if d["path"] == target_path]
    remaining = [d for d in devices if d["path"] != target_path]

    min_headroom = float("inf")
    for alloc_type in ["Data", "Metadata", "System"]:
        bytes_on_target = sum(d["allocations"].get(alloc_type, 0) for d in target)
        if bytes_on_target == 0:
            continue
        remaining_unalloc = sorted([d["unallocated"] for d in remaining], reverse=True)
        if len([u for u in remaining_unalloc if u > 0]) < 2:
            return 0
        largest = remaining_unalloc[0]
        rest = sum(remaining_unalloc[1:])
        total = sum(remaining_unalloc)
        raid1_capacity = rest if largest > rest else total // 2
        headroom = raid1_capacity - bytes_on_target
        if headroom < 0:
            return 0
        min_headroom = min(min_headroom, headroom)

    return min_headroom if min_headroom != float("inf") else 0
```

### 3. Replace Phase 2 fill loop (lines 43-108)

```python
with section("Fill pool until dry-run rejects removal"):
    fill_path = f"{MOUNT_POINT}/fill"
    dry_cmd = remove_cmd("hwtest3", extra="--dry-run") + " 2>&1"
    target_dev = "/dev/mapper/braid-hwtest3"
    file_index = 0
    threshold_crossed = False

    GIB = 1024 * 1024 * 1024
    MIB = 1024 * 1024
    FINE_THRESHOLD = 5 * GIB

    # --- Coarse phase: large writes guided by headroom, guardrailed by dry-run ---
    while True:
        devices = parse_device_usage()
        headroom = raid1_headroom(devices, target_dev)
        print(f"  Headroom: {headroom // MIB} MiB")

        if headroom < FINE_THRESHOLD:
            break

        write_bytes = headroom // 2
        write_mb = write_bytes // MIB

        file_index += 1
        dd_cmd = (
            f"dd if=/dev/zero of={fill_path}_{file_index} "
            f"bs=1M count={write_mb} status=progress 2>&1"
        )
        dd_exit, _ = run_capture(dd_cmd, timeout=7200)
        run_capture("sync", timeout=120)

        if dd_exit != 0:
            print(f"  Coarse {file_index}: pool full during write")
            break
        print(f"  Coarse {file_index}: wrote {write_mb} MiB")

        # Guardrail: production dry-run is authoritative — if headroom
        # calc drifted and we already crossed the threshold, stop now.
        dry_exit, dry_output = run_capture(dry_cmd, timeout=300)
        if dry_exit != 0 and "not enough space" in dry_output.lower():
            print(f"  Coarse {file_index}: dry-run rejected — threshold crossed")
            threshold_crossed = True
            break

    # --- Fine phase: 1 GB writes with dry-run check ---
    while not threshold_crossed:
        file_index += 1

        dd_cmd = (
            f"dd if=/dev/zero of={fill_path}_{file_index} "
            f"bs=1M count=1024 status=progress 2>&1"
        )
        dd_exit, dd_output = run_capture(dd_cmd, timeout=600)

        if dd_exit != 0:
            print(f"  Fine {file_index}: dd failed (pool full)")
            run_capture("sync", timeout=60)
        else:
            run("sync")

        dry_exit, dry_output = run_capture(dry_cmd, timeout=300)
        if dry_exit != 0 and "not enough space" in dry_output.lower():
            print(f"  Fine {file_index}: dry-run rejected — threshold crossed")
            threshold_crossed = True
            break

        if dd_exit != 0:
            # Existing micro-write fallback (64 MB)
            run_capture("sync", timeout=60)
            dry_exit2, dry_output2 = run_capture(dry_cmd, timeout=300)
            if dry_exit2 != 0 and "not enough space" in dry_output2.lower():
                print(f"  Fine {file_index}: rejected after sync")
                threshold_crossed = True
                break

            print(f"  Fine {file_index}: pool full, trying 64 MiB micro-writes")
            for micro in range(1, 17):
                micro_cmd = (
                    f"dd if=/dev/zero of={fill_path}_micro_{micro} "
                    f"bs=1M count=64 2>&1"
                )
                run_capture(micro_cmd, timeout=120)
                run_capture("sync", timeout=60)
                dry_exit3, dry_output3 = run_capture(dry_cmd, timeout=300)
                if dry_exit3 != 0 and "not enough space" in dry_output3.lower():
                    print(f"  Micro {micro}: rejected — threshold crossed")
                    threshold_crossed = True
                    break
            break

        print(f"  Fine {file_index}: wrote 1 GiB, dry-run still passes")

    dev_usage = run(f"btrfs device usage --raw {MOUNT_POINT}")
    print(f"\nDevice usage at threshold:\n{dev_usage}")

    assert threshold_crossed, (
        "Pool filled but braid remove --dry-run never produced ENOSPC rejection. "
        "Either dry-run has a bug or the test's fill strategy is insufficient."
    )
```

### 4. Update module docstring

Mention coarse-to-fine adaptive fill strategy.

### 5. Add `import re` to imports

## Expected improvement

- **Before:** ~750 iterations of (dd 1GB + sync + dry-run) = ~3 hours
- **After:** ~5 coarse writes + ~5 fine iterations = ~30-40 min

Total bytes written is the same (unavoidable — need real chunk allocation). Time savings come from eliminating ~740 sync + dry-run checks.

## Verification

Run the hardware test: `python tests/hw/runner.py test_remove_under_pressure`

Confirm:

- Coarse phase prints decreasing headroom over ~5 iterations
- Fine phase crosses threshold in < 10 iterations
- Phases 3-5 still pass (unchanged)
