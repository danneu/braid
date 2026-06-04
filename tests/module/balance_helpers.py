"""Shared helper for VM tests that need a btrfs balance paused mid-flight.

Extracted because the start/pause/retry idiom was hand-copied across four test
scripts and one copy (braid-unlock Test 8) drifted into a flaky form.
"""

import re
import time


def pause_balance_with_remaining_work(node, *, mount_point="/mnt/storage", attempts=3):
    """Start a btrfs balance and pause it with remaining work still to do.

    Returns the paused `btrfs balance status` output (str). Raises if no
    attempt pauses with remaining work, or a failed attempt cannot be
    cancelled cleanly.

    Always converts BOTH data and metadata (`-dconvert -mconvert`; `-m` also
    covers the system group). btrfs honors a pause request only *between*
    block-group relocations, and only pauses with remaining work
    (current < total) when >= 2 block groups exist. Converting data alone on a
    small payload yields a single data block group, which can only pause at
    "1 out of 1" -- the original Test 8 flake. See
    reference/linux/fs/btrfs/volumes.c __btrfs_balance (the
    `(!counting && pause_req)` check at the top of the chunk loop).

    Caller owns payload size and any dm-delay slowdown; this helper only owns
    the start/pause/verify/retry loop.
    """
    for _ in range(attempts):
        # Start in background, then tight-loop pause attempts natively on the
        # VM (no Python roundtrip overhead -- a fast balance finishes in <1s).
        # Fixed hard raid1 convert (no `soft`): on a raid1/raid1 pool a hard
        # convert rewrites every block group regardless of its current profile,
        # so each retry has fresh work without alternating the target. Do NOT
        # switch to `-dconvert=single -mconvert=single` to force work: -mconvert
        # also rewrites system chunks, and reducing metadata/system redundancy
        # (raid1 -> single) makes the kernel reject the start with -EINVAL unless
        # --force is given (reference/linux/fs/btrfs/volumes.c btrfs_balance, the
        # reducing_redundancy gate).
        node.execute(
            f"btrfs balance start -dconvert=raid1 -mconvert=raid1 {mount_point} "
            f"> /tmp/balance.log 2>&1 & "
            f"for i in $(seq 1 200); do "
            f"  btrfs balance pause {mount_point} 2>/dev/null && break; "
            f"  sleep 0.02; "
            f"done"
        )

        output = node.execute(f"btrfs balance status {mount_point}")[1]
        if "paused" in output.lower():
            match = re.search(r"(\d+)\s+out of about\s+(\d+)\s+chunks", output)
            if match and int(match.group(1)) < int(match.group(2)):
                return output

        # Completed or paused with no remaining work -- cancel and retry. The
        # hard raid1 convert above rewrites every block group again, so the next
        # attempt always has fresh work.
        node.execute(f"btrfs balance cancel {mount_point} 2>/dev/null || true")
        for _ in range(30):
            if "no balance" in node.execute(
                f"btrfs balance status {mount_point}"
            )[1].lower():
                break
            time.sleep(0.2)
        else:
            raise Exception(
                "balance did not terminate after cancel -- cannot retry safely"
            )

    raise Exception(
        f"could not pause balance with remaining work after {attempts} full attempts"
    )
