# btrfs RAID1 Deep Dive

## How It Actually Works

btrfs RAID1 is chunk-level mirroring inside the filesystem, not block-level mirroring below it (like mdraid). The key differences:

- Chunks (256MiB-1GiB) are allocated across devices. For RAID1, each chunk is placed on exactly 2 devices — regardless of how many devices are in the pool. 5 drives still means 2 copies per chunk.
- Three separate block group types: DATA, METADATA, SYSTEM — each can have an independent RAID profile.
- The filesystem controls allocation, so it doesn't care about matching disk sizes — it just needs two devices with free space to form a pair. Unequal drives work, though larger drives will have stranded capacity once smaller ones fill up.
- COW (copy-on-write) means data is never overwritten in place. New data goes to fresh locations, a new metadata tree is built, and the superblock atomically switches. This gives transaction-level crash consistency — you lose at most ~30s of uncommitted writes, but the filesystem is always structurally valid.

## RAID1 vs RAID1C3 vs RAID1C4

| Profile | Copies | Survives | Space | Min Devices | Kernel |
|---------|--------|----------|-------|-------------|--------|
| RAID1   | 2      | 1 failure  | 50%   | 2           | ancient |
| RAID1C3 | 3      | 2 failures | 33%   | 3           | 5.5+    |
| RAID1C4 | 4      | 3 failures | 25%   | 4           | 5.5+    |

Best practice: `-d raid1 -m raid1c3` — data gets 2 copies, metadata gets 3. Metadata is tiny but losing it is catastrophic. RAID1C3 is positioned by developers as the reliable alternative to RAID5/6 (which remain unstable — never use in production).

## The Write Hole

btrfs RAID1 does NOT have the classic RAID write hole for normal COW operations. The COW design provides atomicity. Exception: NOCOW files (systemd journals, VM images with `chattr +C`) re-enable in-place overwrites, reintroducing the write hole and disabling checksums. Scrub cannot verify NOCOW data.

---

## The #1 Operational Hazard: Degraded Mode

This is the single most important thing for braid:

1. btrfs refuses to mount degraded by default — requires explicit `-o degraded`
2. While degraded, new block groups are allocated as single profile (one copy, zero redundancy)
3. Second degraded RW mount becomes read-only — you get one shot
4. After recovery, you MUST rebalance:
   ```bash
   btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/pool
   ```
5. Without this, single-profile chunks silently persist — a data-loss time bomb.

This is the bug Synology sidestepped entirely by using mdraid for redundancy and btrfs only for filesystem features.

---

## ENOSPC: The Space Accounting Trap

btrfs has a two-stage allocator (chunks → blocks within chunks). ENOSPC can occur when:

- All chunks of one type (DATA or METADATA) are full
- No unallocated device space exists for new chunks
- Free space exists in the wrong chunk type

The deletion paradox: Deleting files requires metadata allocation. If metadata chunks are full and no space exists for new metadata chunks, you can't delete files to free space. Circular deadlock.

RAID1 doubles the problem — every chunk needs free space on two devices simultaneously.

Recovery: Add a temporary device (even USB) for breathing room, or `btrfs balance start -dusage=0` to reclaim empty block groups.

---

## Drive Replacement

Always use `btrfs replace`, never add-then-remove:

```bash
# For a dead/missing drive:
mount -o degraded /dev/mapper/crypt-surviving /mnt/pool
btrfs replace start -r <devid> /dev/mapper/crypt-new /mnt/pool
# -r = avoid reading from failed source device

# After replace completes:
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/pool
```

Known issue: `btrfs replace` aborts on read errors from source. The `-r` flag mitigates this by reading from mirrors instead. Another known bug: ENOSPC during replace due to metadata block group locking deadlock.

---

## Operational Best Practices

**Mount options:**

```
defaults,noatime,compress=zstd:3,space_cache=v2,discard=async
```

Never put `degraded` in fstab — it would silently mount degraded and risk losing the last copy.

**Monitoring — the essential commands:**

```bash
btrfs device stats /mnt/pool          # All counters should be 0
btrfs filesystem usage -T /mnt/pool   # Accurate space (not df)
btrfs scrub status /mnt/pool          # Last scrub results
```

**Scrub:** Monthly. Auto-repairs corrupt blocks from healthy mirrors. Cannot help NOCOW files.

**Balance maintenance:**

```bash
# Safe periodic maintenance (weekly timer):
btrfs balance start -dusage=10,limit=1 /mnt/pool
# Never routine metadata balances — increases ENOSPC risk
```

**Automatic block group reclaim (kernel 5.19+):**

```bash
echo 75 > /sys/fs/btrfs/<FSID>/allocation/data/bg_reclaim_threshold
```

---

## LUKS + btrfs RAID1 (The braid Stack)

The stack works well but has specific considerations:

1. Unlock order matters — all LUKS devices must be opened before btrfs can assemble
2. LUKS header backups are critical — corrupted header = irrecoverable device regardless of btrfs redundancy
3. Device replacement requires LUKS setup first — format + open the replacement with LUKS before `btrfs replace`
4. Use btrfs UUID in fstab, not mapper paths — stable across device reordering
5. Compression operates above encryption — `compress=zstd` works correctly with LUKS underneath

---

## Hardware Requirements

- **ECC RAM is strongly recommended.** A documented 2024 case study showed non-ECC bit-flips corrupting data in memory before write — both RAID1 mirrors received identical bad data. Checksums detected it but couldn't repair.
- **Disks must honor flush/FUA commands.** btrfs's entire consistency model depends on this. Consumer SSDs and some SATA controllers are known to lie. This is documented as "perhaps the most serious problem and impossible to mitigate by filesystem."

---

## Kernel Version Guide

| Kernel | Key Feature |
|--------|-------------|
| 5.5+   | RAID1C3/C4 |
| 5.19+  | Auto block group reclaim |
| 6.1+   | Superblock scrub repair |
| 6.2+   | discard=async default |
| 6.6+   | Best current LTS for RAID1 |
| 6.14+  | Experimental RAID1 read balancing |

NixOS 24.05+ ships 6.6+ by default — excellent for btrfs RAID1.

---

## Key Takeaways for braid

1. Post-degraded rebalance is mandatory — braid must automate `balance -dconvert=raid1,soft -mconvert=raid1,soft` after any degraded recovery or replace operation
2. Monitor unallocated space, not just used/free — `btrfs filesystem usage` is the truth, not `df`
3. Pre-flight space checks before replace/balance — both operations can deadlock on ENOSPC
4. LUKS header backups should be part of braid's operational guidance
5. `btrfs replace -r` is the right tool for failed-drive replacement, not add+remove
6. Never put `degraded` in fstab — mount degraded only interactively/explicitly
