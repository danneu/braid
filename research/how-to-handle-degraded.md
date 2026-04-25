# What should braid do when degraded mount is the only option?

## The Danger Being Mitigated

When btrfs RAID1 is mounted degraded, **every new block group allocation uses
`single` profile** — one copy, zero redundancy. The longer you run degraded, the
more unprotected data accumulates. If you lose another drive, that data is gone.
After recovery, you must rebalance to restore RAID1 on those chunks. Forget the
rebalance and you carry a silent data-loss time bomb indefinitely.

## The Options

### A: Refuse to mount, require explicit flag

```
$ braid unlock
[skip] disk disk3: not found (unplugged?)
error: pool has 1 missing device — refusing to mount degraded
       new writes would have ZERO redundancy (single-profile chunks)
hint:  braid unlock --allow-degraded
```

| | |
|---|---|
| **Availability** | Low — pool offline until user explicitly opts in |
| **Safety** | Highest — impossible to accidentally run unprotected |
| **Auto-unlock** | Needs `braid.autoUnlock.allowDegraded = true` in NixOS config |
| **Complexity** | Low — one flag, clear error message |
| **Downside** | NAS offline for a headless box until someone SSHes in with the right flag. For auto-unlock, requires a pre-made policy decision in config. |

### B: Mount read-only by default

```
$ braid unlock
[skip] disk disk3: not found (unplugged?)
[warn] pool: mounted READ-ONLY (degraded — 1 missing device)
hint:   braid unlock --allow-degraded for read-write access
```

| | |
|---|---|
| **Availability** | Medium — reads work, writes fail. Services that write (Samba, Plex metadata, etc.) break. |
| **Safety** | High — no single-profile chunks can be created |
| **Auto-unlock** | Same — comes up RO, services that write will error |
| **Complexity** | Low |
| **Downside** | A NAS that can't be written to is barely functional. Users will hit confusing "read-only filesystem" errors from applications rather than a clear braid message. The failure mode is spread across every service rather than centralized in braid. |

### C: Mount read-write, prompt interactively

```
$ braid unlock
[skip] disk disk3: not found (unplugged?)
[WARN]  1 missing device — mounting degraded means new writes have
        ZERO redundancy until you replace the drive and rebalance.

  Mount degraded read-write? [y/N]:
```

| | |
|---|---|
| **Availability** | High after confirmation; blocks on human input |
| **Safety** | High for interactive — forces informed consent |
| **Auto-unlock** | Can't prompt headless. Needs a fallback policy (refuse? mount RW? mount RO?) |
| **Complexity** | Medium — two code paths (interactive vs auto-unlock) |
| **Downside** | Splits behavior between interactive and auto-unlock. The auto-unlock fallback still needs to be one of the other options, so this doesn't eliminate the design decision — it just defers it to a different context. |

### D: Mount read-write, track state, auto-recover

```
$ braid unlock
[skip] disk disk3: not found (unplugged?)
[WARN]  pool: mounted DEGRADED — new writes have NO redundancy
[WARN]  replace the failed drive with `braid replace disk3 /dev/disk/by-id/new-drive`

$ braid status
pool:   DEGRADED (1 missing device) since 2026-02-27T14:30:00Z
        ⚠ new data written since degraded mount has NO redundancy
health: 2/3 devices online
...

$ braid replace disk3 /dev/disk/by-id/new-drive
[ok]    replacing disk3...
[ok]    replace complete
[ok]    rebalancing single-profile chunks to raid1...
[ok]    pool: healthy (3/3 devices, all data raid1)
```

| | |
|---|---|
| **Availability** | Highest — pool comes up RW, services work |
| **Safety** | Medium — single-profile window exists, but tracked and auto-cleaned on recovery |
| **Auto-unlock** | Works identically — mount RW, track state, warn on next interaction |
| **Complexity** | Highest — persistent state file, `braid status` integration, auto-rebalance in `braid replace` |
| **Downside** | You DO accumulate single-profile chunks during the degraded window. If a second drive fails before replacement, that data is gone. The tracking and auto-rebalance don't prevent loss — they minimize the window and ensure cleanup. |

## Ranking

| | Safety | Availability | UX clarity | Complexity | Auto-unlock works? |
|---|---|---|---|---|---|
| **A: Refuse + flag** | best | worst | clear | low | needs config option |
| **B: Read-only** | great | poor | confusing (errors everywhere) | low | functional but broken services |
| **C: Interactive prompt** | great (interactive) | good after prompt | clear | medium | needs fallback = another option |
| **D: Track + auto-recover** | good | best | clear | highest | works naturally |

## Recommendation: A

**Refuse by default, require `--allow-degraded`.**

Reasoning:

1. **Matches braid's philosophy.** braid exists to make btrfs RAID1 "less
   error-prone." Silent degraded mounts are the #1 btrfs RAID1 footgun. The safe
   default should be safe.

2. **Lowest complexity.** One flag, one error message, one NixOS config option.
   No persistent state files, no auto-rebalance logic, no split behavior between
   interactive and headless.

3. **The auto-unlock case has a clean answer.**
   `braid.autoUnlock.allowDegraded` (default `false`) is a declarative policy
   decision the user makes once in their NixOS config. If true, auto-unlock
   mounts degraded and logs a warning. If false (default), auto-unlock skips the
   mount and the user must SSH in. Either way, the user pre-decided.

4. **Option D is better as a layer on top of A, not instead of it.** The state
   tracking and auto-rebalance in `braid replace` are genuinely valuable — but
   they're orthogonal to the mount decision. You can have `--allow-degraded` AND
   have `braid replace` auto-rebalance afterward. You don't need to accept
   silent degraded mounts to get auto-recovery.

5. **Option B's failure mode is worse than A's.** A read-only NAS produces
   confusing errors from every service. A refused mount produces one clear braid
   error with an actionable hint. Centralizing the failure in braid is better UX
   than scattering it across Samba/Plex/etc.

6. **Option C doesn't stand alone.** It still needs a fallback for auto-unlock,
   which means you're implementing A or D anyway.

The eventual ideal is **A + D's recovery features**: refuse by default, explicit
opt-in, and when the user does replace a drive, `braid replace` automatically
rebalances single-profile chunks as the final step. But A alone is the right
minimum viable behavior.
