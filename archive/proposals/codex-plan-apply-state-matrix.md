# Braid Plan/Apply State Matrix

This matrix documents the **current implementation** in `scripts/braid.sh` — what the code actually does today, not a spec for how it should work. It is behavior-complete for current `braid plan` and `braid apply` logic by grouping infinitely many concrete disk combinations into behavior-equivalent state classes.

## Planner + Apply Matrix

| # | System State | Flags | `braid plan` result | `braid apply` result | Simple comment |
|---|---|---|---|---|---|
| 1 | Bootstrap, 1 configured disk, present, not LUKS, pool not mounted | none | `status=applicable`, warning `INIT_REQUIRED`, only verify actions | No mutation actions; prints “Nothing to do” | First install case: run `init-disk` first. |
| 2 | Bootstrap, multiple configured disks, present, not LUKS, pool not mounted | none | `status=applicable`, multiple `INIT_REQUIRED`, only verify actions | No mutation actions | Same as #1 but repeated per disk. |
| 3 | Bootstrap, 1 configured disk, present, LUKS, pool not mounted | none | `OPEN_LUKS`, `ADD_DISK_BTRFS_ADD`, verify; no balance | Executes actions, creates single-disk btrfs | Start pool with one formatted disk. |
| 4 | Bootstrap, >=2 configured disks, all present+LUKS, pool not mounted | none | `OPEN_LUKS`+`ADD` per disk, plus `BALANCE_TO_RAID1` | Executes all, ends RAID1 | Start pool with multiple formatted disks. |
| 5 | Bootstrap mixed: some present+LUKS, some present+non-LUKS, some absent | none | Adds only present+LUKS; warns `INIT_REQUIRED` and `DISK_ABSENT_SKIPPED`; may include balance if resulting count >=2 | Applies safe subset only | Missing/unformatted disks are skipped, not fatal. |
| 6 | Pool mounted, config exactly matches pool UUIDs, no missing | none | `status=applicable`, only verify actions | No-op | Healthy steady state. |
| 7 | Pool mounted, config has extra present+LUKS disk not in pool | none | `OPEN_LUKS` (if needed), `ADD_DISK_BTRFS_ADD`, maybe `BALANCE_TO_RAID1` | Adds disk and balances if needed | Normal add flow. |
| 8 | Pool mounted, extra disk already open with expected mapper name | none | Usually only `ADD_DISK_BTRFS_ADD` (+ optional balance) | Adds directly | Open step skipped because already open. |
| 9 | Pool mounted, extra disk open under alias mapper name | none | May still plan `OPEN_LUKS` (open check is basename-based), then `ADD` | `OPEN_LUKS` detects same UUID mapped elsewhere and skips safely | Alias-safe at execution time. |
| 10 | Pool mounted, config includes present non-LUKS disk not in pool | none | Warning `INIT_REQUIRED`, no add actions for that disk | Proceeds with other actions only | Non-LUKS is never auto-formatted by apply. |
| 11 | Pool mounted, config includes absent disk not in pool | none | Warning `DISK_ABSENT_SKIPPED`, no add actions for that disk | Proceeds with other actions only | Absent disk is skipped. |
| 12 | Pool mounted, pool has disk(s) not in config, no absent config disks, removal keeps >=2 disks | none | `REMOVE_DISK_GRACEFUL` + `CLOSE_LUKS_MAPPER` per unmatched disk | Removes disks, no confirmation needed | Normal graceful remove. |
| 13 | Same as #12 but removal would leave <2 disks | none | Same remove actions + confirmation `remove this disk without redundancy` | Fails unless `BRAID_CONFIRM` includes phrase | Safety gate before dropping redundancy. |
| 14 | Pool mounted, removal candidates exist, and at least one config disk is absent (UUID unknown) | none | `status=blocked`, blocked `IDENTITY_AMBIGUOUS_ABSENT_DISK` | Apply aborts with blocked message | Ambiguous identity on destructive path is blocked. |
| 15 | Same as #14 | `--allow-remove-ambiguous` | `status=applicable`, confirmation `remove despite ambiguous identity` | Requires phrase in `BRAID_CONFIRM` | Explicit override for ambiguous remove. |
| 16 | Same as #15, and removal also drops below 2 disks | `--allow-remove-ambiguous` | `status=applicable`, two confirmations: ambiguous + redundancy | `BRAID_CONFIRM` must include both (semicolon-separated) | Combined gate is supported. |
| 17 | Pool mounted degraded: `missing_count=1`, no remove-missing flag | none | Warning `POOL_DEGRADED_MISSING_DEVICES`, no explicit missing-remove action | Continues other safe actions; missing remains | Inform-only unless explicitly requested. |
| 18 | Pool mounted degraded: `missing_count=1` | `--allow-remove-missing` | Adds `REMOVE_DISK_MISSING_EXPLICIT` + confirmation `remove missing device from pool` | Needs phrase; then removes missing device | Explicit missing-device eviction flow. |
| 19 | Pool mounted degraded: `missing_count>1` | `--allow-remove-missing` | `status=blocked`, blocked `AMBIGUOUS_MISSING` | Apply blocked | Too ambiguous to auto-remove which missing device. |
| 20 | Pool mounted with both unmatched pool disks and one missing device | maybe `--allow-remove-missing` | May include graceful removes + missing warning/action depending flags | Apply follows same gates; may need multiple confirmations | Mixed remove scenario. |
| 21 | Any mounted state where planner has add actions and resulting device count after add/remove/missing handling >=2 | any | Includes `BALANCE_TO_RAID1` | Runs balance | Auto-converges to RAID1 when target count warrants it. |
| 22 | Any state where planner has only verify actions | any | `status=applicable`, mutation count 0 | Returns early (“Nothing to do”) | Verify-only plan is treated as no-op. |
| 23 | Any state where UUID for a pool mapper cannot be determined | any | Hard error (`die`) before usable plan | Apply fails before execution | Fail-closed identity model. |
| 24 | Any state where configured disk is present+LUKS but UUID cannot be read | any | Hard error in planner | Apply fails | Also fail-closed. |
| 25 | Any fresh apply run with blocked plan | any | Plan shows blocked reasons | Apply exits non-zero, no actions executed | Apply never executes blocked plans. |
| 26 | Config file `/etc/braid/config.json` missing | any | Hard error (`die`) before plan starts | Apply fails before execution | Most basic precondition failure. |
| 27 | Pool mounted, config swaps disk A for disk B (A in pool not in config, B in config not in pool) | none | `REMOVE_DISK_GRACEFUL`+`CLOSE_LUKS_MAPPER` for A, `OPEN_LUKS`+`ADD_DISK_BTRFS_ADD` for B, maybe `BALANCE_TO_RAID1` | Executes remove then add in plan order | Replace — emergent from add + remove in same plan. |
| 28 | Pool mounted degraded (`missing_count=1`), config has new disk to add | none | Warning `POOL_DEGRADED_MISSING_DEVICES`, `OPEN_LUKS`+`ADD_DISK_BTRFS_ADD` for new disk, `BALANCE_TO_RAID1` if count warrants | Adds disk, balances; missing device stays unless `--allow-remove-missing` | Grow pool while degraded — missing is not a blocker for adds. |

## Apply-Specific Runtime States

| # | Apply Runtime State | `braid apply` behavior | Simple comment |
|---|---|---|---|
| A1 | Fresh apply, checkpoint file already exists | Fails: use `--resume` or clear checkpoint | Prevents accidental parallel apply. |
| A2 | `--resume`, checkpoint missing | Fails immediately | Nothing to resume. |
| A3 | `--resume`, config hash changed vs checkpoint | Fails immediately | Resume is strict against drift. |
| A4 | `--resume`, pending action target absent (`OPEN_LUKS`/`ADD`/`REMOVE`) | Fails `RESUME_TARGET_MISSING`, checkpoint preserved | Restore target and retry resume. |
| A5 | `--resume`, targets present and hash matches | Continues from pending actions, skips completed ones | Normal crash-safe continuation. |
| A6 | Fresh apply, required confirmations missing/partial | Fails with required phrase message | All required phrases must be present. |
| A7 | Fresh apply, multiple confirmations required | Accepts semicolon-separated `BRAID_CONFIRM` with whitespace trimmed | Multi-confirm behavior. |

## Apply Action Handler Runtime Behaviors

These are runtime behaviors within individual action handlers that are not visible at the planner level but affect what actually happens during `braid apply`.

| # | Handler | Runtime State | Behavior | Comment |
|---|---------|---------------|----------|---------|
| H1 | `action_btrfs_add` | Target device already recognized by btrfs (returning member) | `btrfs device scan` finds device already in pool; handler prints "returning pool member" and skips the add | Handles reconnected disks without re-adding. Distinct from LUKS-already-open (#8). |
| H2 | `action_open_luks` | Target LUKS UUID already mapped under a different mapper name | Scans all `/dev/mapper/*` entries; finds matching UUID; skips open | Alias-safe idempotency at execution time (row #9). |
| H3 | `action_balance_raid1` | Pool already RAID1 with no missing devices | Skips balance entirely | Idempotent — safe to include in plan even when not strictly needed. |
| H4 | *(removed)* | *(was: post-balance auto-eviction of missing devices)* | Removed — this bypassed the `--allow-remove-missing` safety gate. Missing-device removal must go through the explicit `REMOVE_DISK_MISSING_EXPLICIT` path. | Fixed: balance no longer silently evicts missing devices. |
| H5 | `action_remove_graceful` | Removal would leave <2 present devices | Converts pool data profile from RAID1 to single before removing device | Profile conversion is prerequisite — btrfs refuses RAID1 remove below 2 devices. |
| H6 | `action_remove_missing` | Removal would leave <2 present devices | Same as H5: converts to single profile first | Same prerequisite as graceful remove. |
| H7 | `action_close_luks` | Mapper does not exist or already closed | Best-effort close; failure is non-fatal | Tolerates races or already-cleaned-up state. |

## Verify Action Semantics

| Action | What it checks | Failure behavior |
|--------|---------------|-----------------|
| `VERIFY_POOL_HEALTH` | Mount point is mounted; counts missing devices | `die` if pool is not mounted (hard fail); warns if missing devices remain (non-fatal) |
| `VERIFY_EXPECTED_DISK_SET` | Each config disk's LUKS UUID is present in pool's btrfs device list | Warns per unmatched config disk; does not fail apply |

## Confirmation Phrase Matrix

| Situation | Required phrase(s) |
|---|---|
| Remove missing device (explicit) | `remove missing device from pool` |
| Graceful remove drops below 2 disks | `remove this disk without redundancy` |
| Ambiguous remove override | `remove despite ambiguous identity` |
| Combined case | Provide all required phrases in `BRAID_CONFIRM`, semicolon-separated |

## Coverage Notes

This includes all state classes currently represented in planner/apply logic and test themes: bootstrap, add/remove/replace, absent/non-LUKS skip, missing-device gate, ambiguity gate, checkpoint/resume, multi-confirmation, handler-level idempotency, and verify semantics.

Potential extra edge tests to add later:

1. Configured disk present+LUKS but UUID probe failure (explicit fail-closed test).
2. Pool mapper UUID probe failure in `discover_live_state`.
3. Mixed case requiring all three confirmations in one plan: missing-device + ambiguous + redundancy.
4. Returning-member detection: disconnect a disk, remove from config, apply (removes it), re-add to config, apply — `action_btrfs_add` should detect the returning member via `btrfs device scan`.
5. Balance idempotency: run apply twice on an already-RAID1 pool with no changes — balance handler should skip.
