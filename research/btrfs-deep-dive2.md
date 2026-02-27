# Btrfs RAID1 Deep Dive

## 1. Architecture: How btrfs RAID1 Actually Works

**Chunk-level mirroring, not block-level.** Unlike mdraid (which mirrors entire block devices byte-for-byte), btrfs implements RAID inside the filesystem at the **block group** (chunk) level:

- Space is allocated in large chunks: **1 GiB for data**, **256 MiB–1 GiB for metadata**
- For RAID1, each chunk is placed on **exactly 2 different devices** — always 2 copies, regardless of total device count
- The **two-stage allocator** first picks which 2 devices get a new chunk (favoring devices with the most free space), then allocates extents within that chunk
- With 3+ devices, data is spread across different device *pairs*, but each piece of data still has exactly 2 copies

**Key implications:**

- Adding a 3rd, 4th, 5th drive increases *capacity*, not *redundancy* — still only 2 copies per block group
- For more copies, you need RAID1C3 (3 copies, min 3 devices) or RAID1C4 (4 copies, min 4 devices)
- Checksums on every block enable **auto-repair on read**: bad copy detected → good copy served → bad copy overwritten
- mdraid has no checksums and cannot distinguish good from bad copies

**Read policy:** Default is PID-based distribution. Kernel 6.14+ adds experimental round-robin, latency-based, and manual device selection via `/sys/fs/btrfs/<UUID>/read_policy`.

---

## 2. RAID Profiles Comparison

| Profile | Copies | Min Devices | Space Efficiency | Survives N Failures | Kernel | Status |
|---------|--------|-------------|------------------|---------------------|--------|--------|
| RAID1 | 2 | 2 | 50% | 1 | any | **Stable** |
| RAID1C3 | 3 | 3 | 33% | 2 | 5.5+ | **Stable** |
| RAID1C4 | 4 | 4 | 25% | 3 | 5.5+ | **Stable** |
| RAID5 | parity | 3 | (N-1)/N | 1 | — | **UNSTABLE, DO NOT USE** |
| RAID6 | parity | 4 | (N-2)/N | 2 | — | **UNSTABLE, DO NOT USE** |

RAID1C3/C4 set an incompatibility flag — pre-5.5 kernels cannot mount these filesystems.

---

## 3. Data vs Metadata Profiles

Data and metadata can use **different RAID profiles**. This is a critical feature:

- **Metadata loss is catastrophic** — one bad metadata sector can render the entire filesystem unmountable
- **Data loss is localized** — one bad data sector affects one file; filesystem stays functional

**Best practice for 3+ drives:** `-d raid1 -m raid1c3` — this gives data single-failure tolerance and metadata double-failure tolerance at minimal extra cost (metadata is small).

**Official docs:** "Always use a redundant profile (DUP or RAID1) for metadata, even with single/RAID0 for data." And: "**Never** use RAID5/6 for metadata."

---

## 4. Mixed-Size Drives

Btrfs RAID1 handles different-sized drives because chunks are paired independently:

**The rule:** If your largest device exceeds the sum of all others, usable space = sum of smaller devices. Otherwise, usable space = total / 2.

| Drives | Usable | Why |
|--------|--------|-----|
| 2TB + 2TB | 2TB | Half of total |
| 2TB + 4TB | 2TB | 4TB > 2TB → limited to smaller |
| 3TB + 1TB + 1TB | 2TB | 3TB > (1+1)TB |
| 3TB + 2TB + 2TB | 3.5TB | 3TB < (2+2)TB → half of 7TB |

Excess capacity on the largest device becomes **unallocatable** when no other device has free space to pair with.

---

## 5. Failure Modes & Degraded Operation

### Degraded mount requires explicit opt-in

btrfs will **not** automatically mount with missing devices (unlike mdraid). You must:

```bash
mount -o degraded /dev/mapper/crypt-survivor /mnt/storage
```

### Single block groups during degraded mount

When mounted degraded, btrfs creates **single-profile** block groups for new writes instead of RAID1. This data has **no redundancy**. These persist even after the array is restored. **You MUST rebalance after any degraded operation:**

```bash
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/pool
```

### The one-shot read-write problem

A 2-disk RAID1 that has been mounted degraded once becomes **irreversibly read-only** on the second degraded mount. You get **one chance** at a read-write degraded mount to perform recovery. After that, read-only only unless you fix the array.

### Recovery priority checklist

1. Mount degraded (one shot at read-write)
2. Either `btrfs replace start -r <devid> /dev/new ...` or `btrfs device delete missing`
3. Rebalance to restore RAID1: `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft`
4. Run `btrfs scrub start` to verify

---

## 6. Device Replacement

### `btrfs replace` (PREFERRED)

```bash
btrfs replace start -r <devid> /dev/mapper/crypt-new /mnt/pool
```

- **Several times faster** than remove+add (direct data copy, not full rebalance)
- `-r` flag reconstructs from RAID mirrors, avoiding reads from failing source
- Atomic: single operation replaces the device
- **Replacement must be >= original size**
- After replace with a larger disk, you must manually resize: `btrfs filesystem resize <devid>:max /mnt`
- **Kernel 6.19+:** Replace gets cancelled on suspend/hibernate and must restart from scratch

### Remove + Add (slower, for special cases)

```bash
btrfs device delete missing /mnt/pool
btrfs device add /dev/mapper/crypt-new /mnt/pool
btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft /mnt/pool
```

Use when the replacement disk is smaller than the original, or when `btrfs replace` isn't available.

> **Warning:** Do not run balance to convert from a profile with more redundancy to less in order to remove a failing device. Use `btrfs replace` instead.

---

## 7. The Write Hole

**btrfs RAID1 effectively eliminates the write hole** through COW + checksums:

- COW means data is never overwritten in place — new data goes to a new location, pointer updated atomically
- If power fails mid-write, old data is intact (worst case: uncommitted writes lost)
- Checksums detect any divergence between mirrors

**The exception — NOCOW files:** `chattr +C` disables COW *and* checksums. For NOCOW files, the write hole fully applies (~50% chance of reading wrong copy after crash). **Never use NOCOW for important data in RAID1.**

**Comparison with mdraid:** mdraid has no checksums and no COW. It cannot detect bit-rot, cannot tell which mirror is correct after a crash, and has a real write hole.

---

## 8. Scrub

Scrub reads every block, verifies checksums, and auto-repairs corruption from RAID mirrors:

- **Run monthly** (official recommendation)
- Run immediately after any degraded operation, power failure, or unclean shutdown
- NOCOW files are NOT verified (no checksums)
- Scrub is NOT fsck — it checks data integrity, not filesystem structure
- Since kernel 6.19, scrub is cancellable on suspend but can be resumed with `btrfs scrub resume`
- Per-device bandwidth limits: `/sys/fs/btrfs/FSID/devinfo/DEVID/scrub_speed_max` (since 5.14)

---

## 9. Self-Healing in Practice

**Two layers:**

1. **Reactive (auto-repair on read):** Every read verifies checksums. Bad block → fetch from mirror → serve good copy → repair bad copy. Silent, transparent. But blocks that are never read accumulate corruption undetected.

2. **Proactive (scrub):** Reads and verifies *everything*. Repairs all detectable corruption. This is why monthly scrubs are essential.

**What self-healing does NOT do:**

- Does not protect against memory corruption (bad RAM writes identical bad data to both mirrors with matching checksums)
- Does not repair filesystem structural damage (tree corruption)
- Does not provide unattended degraded boot
- Does not eliminate need for backups

**ECC RAM is strongly recommended** — a documented 2024 incident showed RAM corruption defeating RAID1 because both copies were corrupted identically.

---

## 10. Operational Best Practices

### Mount options

| Option | Recommendation |
|--------|----------------|
| `noatime` | Always. Eliminates COW overhead from access time updates |
| `compress=zstd:1` | Good default for NAS. CPU time < I/O time saved on HDDs |
| `space_cache=v2` | Default on modern kernels. Use it |
| `discard=async` | For SSDs only. Default since kernel 6.2 |

**Avoid:** `autodefrag` (breaks reflinks/snapshots), `nobarrier` (corruption risk), `nodatacow` (disables checksums)

### Balance maintenance

- After adding/removing devices: **mandatory** — run `btrfs balance start -dconvert=raid1,soft -mconvert=raid1,soft`
- Preventive maintenance: `btrfs balance start -dusage=5 /mnt` (compact underutilized chunks)
- **Never** balance without filters on large filesystems (rewrites everything)
- Balance needs workspace (unallocated space). If ENOSPC during balance: try `usage=0` first

### Monitoring

| Command | Purpose |
|---------|---------|
| `btrfs filesystem usage /mnt` | Accurate space reporting (NOT `df`) |
| `btrfs device stats /mnt` | Error counters per device |
| `btrfs device stats -c /mnt` | Boolean: non-zero exit if errors |
| `btrfs scrub status /mnt` | Last scrub results |

**`df` is unreliable for btrfs** — it doesn't account for the two-stage allocator, compression, reflinks, or RAID duplication.

### ENOSPC prevention

- RAID1 requires free space on **two** devices for each new block group
- Unbalanced free space across devices causes premature ENOSPC
- Deleting files requires metadata space — can itself fail with ENOSPC. Truncate files to zero instead
- Run periodic balance with low usage filters to compact chunks
- Automatic chunk reclaim (kernel 5.19+): `/sys/fs/btrfs/FSID/allocation/PROFILE/bg_reclaim_threshold`

---

## 11. LUKS + btrfs RAID1 Stack

### Architecture

```
/dev/sda → LUKS → /dev/mapper/crypt-a ─┐
/dev/sdb → LUKS → /dev/mapper/crypt-b ─┼─ btrfs RAID1 → /mnt/storage
/dev/sdc → LUKS → /dev/mapper/crypt-c ─┘
```

btrfs native encryption does not exist. LUKS below btrfs is the only option. btrfs sees plaintext devices, so checksumming and self-healing work correctly on decrypted data.

### Unlock order (strictly sequential)

1. `cryptsetup open` each drive → `/dev/mapper/*`
2. `btrfs device scan` — essential for btrfs to discover all members
3. `mount` using any single mapper device

### Key management

Each drive has a unique LUKS master key; the passphrase is just the unlock mechanism. Same passphrase across drives is fine and simplest for interactive unlock. NixOS: `boot.initrd.luks.reusePassphrases = true`.

### LUKS headers are NOT protected by RAID1

The LUKS header sits below btrfs on the raw block device. Each drive's header is independent. **A corrupted LUKS header = permanently lost drive** (even with correct passphrase). Header backups are critical:

```bash
cryptsetup luksHeaderBackup /dev/sdX --header-backup-file sdX-header.bak
```

### Performance

On NAS HDDs with a modern CPU (AES-NI): negligible overhead. HDDs are the bottleneck, not encryption. On NVMe, LUKS can be catastrophic without hardware offloading (RAID1 doubles the encryption work since each write goes to two devices).

### TRIM passthrough

Disabled by default for security (reveals allocation patterns). Enable with `cryptsetup --allow-discards --persistent open` if using SSDs and the trade-off is acceptable. Irrelevant for HDDs.

---

## 12. Snapshots & Subvolumes with RAID1

- RAID profiles are **filesystem-wide**, not per-subvolume
- Defragmentation breaks reflinks from COW/snapshots — can cause massive space explosion. **Avoid `autodefrag`** on snapshot-heavy systems
- Many snapshots (>12) dramatically slow balance, device remove, and resize operations
- Delete oldest snapshots first when reclaiming space
- **Never change a received (read-only) snapshot to read-write** if using `btrfs send/receive` — breaks incremental send chains

---

## 13. Quotas

**Avoid standard qgroups** unless you have a compelling need. Performance penalty is severe with snapshots. Simple quotas (`squotas`, kernel 6.7+) are lighter but don't track shared vs. exclusive usage. For space tracking, prefer `btrfs filesystem du` or `compsize`.

---

## 14. btrfs RAID1 vs ZFS Mirror

| | btrfs RAID1 wins | ZFS mirror wins |
|---|---|---|
| **Flexibility** | Add/remove individual devices dynamically; convert between profiles online | Pools are rigid after creation |
| **Resources** | 2–4 GB RAM fine | 8+ GB recommended; 1 GB/TB |
| **Linux integration** | In-tree kernel module | Out-of-tree; kernel updates can break |
| **Failure handling** | — | Dirty Time Log tracks missed writes; only resilvers changed blocks |
| **Degraded boot** | Requires manual intervention | Generally automatic |
| **Maturity** | Stable for RAID1 but less battle-tested | 20+ years production |
| **Auto-resilver** | — | Immediate, unattended |

**Community consensus:** ZFS for critical production data; btrfs RAID1 for home NAS, hobbyist, and budget-hardware deployments where flexibility and low resource usage matter.

---

## 15. Hard Rules — Never Do These

1. **Never use RAID5/6** for production — write hole is real and unresolved
2. **Never use RAID5/6 for metadata** — use raid1 or raid1c3
3. **Never use `dd` or block-level cloning** on btrfs — identical UUIDs cause corruption
4. **Never use NOCOW on important data in RAID1** — disables checksums, defeats self-healing
5. **Never run `btrfs balance` without filters** on large filesystems
6. **Never convert to lower redundancy to remove a failing device** — use `btrfs replace`
7. **Never assume degraded mode is safe for extended operation** — fix immediately

---

## 16. Recent Kernel Improvements (6.x Era)

| Kernel | Change |
|--------|--------|
| 6.0 | Repair all mirrors for RAID1C3/C4; fix compressed extent repair |
| 6.2 | Checksum verification during RAID5 RMW cycle; `discard=async` default |
| 6.4 | Scrub rewrite (caused temporary regression); device replace improvements |
| 6.6 | Scrub performance partially restored |
| 6.7 | RAID stripe tree (foundation for eventual RAID56 fix); simple quotas (squotas) |
| 6.11 | Dynamic block group reclaim framework |
| 6.14 | Experimental RAID1 read balancing strategies (round-robin, latency, devid) |
| 6.19 | Scrub/replace cancelled on suspend; replace must restart from scratch |
