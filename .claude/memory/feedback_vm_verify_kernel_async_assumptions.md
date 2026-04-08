---
name: never trust unit tests for kernel async / mount-state semantics
description: Unit tests with MockRunner can't catch kernel async behavior (resume workers, in-memory fs_devices caching, mount-session lifetimes). Run the VM repro test before declaring a fix done; iterate against VM output, not against unit tests.
type: feedback
---

When implementing or modifying code that interacts with kernel async
workers or mount-session-scoped state (btrfs dev_replace resume workers,
balance workers, scrub workers, in-memory `btrfs_fs_devices` caching,
LUKS dm-mapper lifetimes), unit tests with MockRunner are necessary but
NOT sufficient. They mock command outputs, not kernel semantics. A unit
test that "exercises the cycle" can pass while the actual cycle still
races a kthread, hits a stale cache, or skips a needed teardown.

**Why:** While fixing the `braid recover` post-replace-resume staleness
bug, three iterations passed unit tests but failed the VM repro:

1. **Iteration 1: `umount + scan --forget + remount` (no LUKS cycle).**
   Unit tests passed. VM test failed: the kernel kept the cached
   `btrfs_fs_devices` because the dm devices were still alive. The fix
   needed to also close+reopen LUKS so the dm devices were torn down.

2. **Iteration 2: full lock+unlock cycle (close+reopen LUKS too).** Unit
   tests passed. VM test failed with the same broken state. The kernel
   `dev_replace ... finished` message landed AFTER recover exited — the
   resume worker was running asynchronously through both mounts and the
   probe captured an in-flight mid-resume snapshot. The fix needed to
   poll `btrfs replace status` and wait for `Finished`/`None` BEFORE
   doing the cycle.

3. **Iteration 3: poll-then-cycle.** Unit tests passed. VM test passed,
   3/3 reliably.

Each iteration needed empirical VM evidence to find what was actually
wrong. Reasoning from "this should work because X" was wrong twice in a
row.

**How to apply:** When touching code that interacts with kernel async
workers, mount-session caches, or device-layer teardown:
1. Implement, run unit tests, then **always run the VM repro test**
   before declaring the fix done. Don't trust "unit tests pass" as a
   proxy for "kernel state machine is satisfied".
2. If the VM test fails, read the FULL test log including the
   `machine #` kernel message timestamps. Async kthread completion
   timestamps relative to your code's progress are the load-bearing
   evidence — sometimes the kernel finishes its work AFTER your code
   exits, which means your probes saw transient state.
3. Iterate against VM output, not against your model of what should
   happen. The kernel's actual behaviour around async workers and
   in-memory caches is more subtle than docs imply.
4. After the fix passes once, run the VM test 3+ times to confirm it's
   not timing-flaky. The original bug here only reproduced ~80% of the
   time, so a single passing run is not strong enough evidence.
