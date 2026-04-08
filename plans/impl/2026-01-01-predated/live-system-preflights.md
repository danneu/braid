# Live Check System: Pre-flight checks per command

## Context

braid needs to lean harder into sanity checks and especially **live state detection**: before any mutating operation, probe the current reality and refuse to proceed if the world isn't clean nor ready nor compatible. This plan catalogs what each command should check and what's already implemented.

We want these cleanly and clearly itemized at the top of every braid command function body, like a checklist.

We want to accumulate all the checks here in one place before implementing them. Some checks happen to already be implemented before this planfile was created; for those we mark with `[x]`.

## `braid add <key>`

- [x] Key exists in config? → error if not
- [x] Key stability (disk-map drift)? → error if by-id reassigned
- [x] Is mount point occupied by non-btrfs? → hard error (currently swallowed as "pool absent")
- [x] Is the pool mounted read-only? → refuse with guidance (btrfs would error anyway, but the native error is cryptic; this gives the user a clear diagnosis and next step)
- [x] Is a btrfs exclusive op (balance/device remove) already running? → refuse
- [x] Is the config disk present? → error if absent
- [x] Is the LUKS mapper already open? → skip LUKS open
- [x] Is the mapper open but device NOT in the pool? → partial prior add (LUKS opened, but btrfs device add never ran)
- [x] Is the device already in the pool (by mapper name)? → "nothing to do"
- [ ] After LUKS open, does the device have a btrfs superblock from a different pool? → refuse (partially implemented: `device_has_btrfs_superblock()` exists but doesn't compare pool UUIDs)
- [x] Does the pool have missing devices? → warn about adding to a degraded pool

## `braid remove <key>`

- [x] Key stability (disk-map drift)? → error if by-id reassigned
- [x] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Is the pool mounted read-only? → refuse with guidance (btrfs would error anyway, but the native error is cryptic; this gives the user a clear diagnosis and next step)
- [x] Is the device in the pool? → error if not
- [x] Does the pool have missing devices? → refuse ("run braid remove-missing first")
- [x] ENOSPC pre-flight check
- [x] Would this leave 0 devices? → error

## `braid remove-missing [--missing-id <devid>]`

- [x] Key stability (disk-map drift)? → error if by-id reassigned
- [x] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Is the pool mounted read-only? → refuse with guidance (btrfs would error anyway, but the native error is cryptic; this gives the user a clear diagnosis and next step)
- [x] Are there actually missing devices? → error if not
- [x] If --missing-id provided, is that devid actually a missing device? → error with guidance
- [x] ENOSPC pre-flight check

## `braid replace --old <key> --new <key>`

- [x] Both keys exist in config? → error if not
- [x] Key stability (disk-map drift)? → error if by-id reassigned
- [x] --old != --new? → error if same
- [x] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Is the pool mounted read-only? → refuse with guidance (btrfs would error anyway, but the native error is cryptic; this gives the user a clear diagnosis and next step)
- [x] Is the new disk present? → error if absent
- [x] Is the new disk's mapper already open but not in pool? → partial prior replace
- [ ] After LUKS open, does the new device have a btrfs superblock from a different pool? → refuse (partially implemented: `device_has_btrfs_superblock()` exists but doesn't compare pool UUIDs)
- [x] Is the old disk live or dead? → determines eviction path
- [x] If old disk is live, does pool have other missing devices? → refuse

## `braid unlock`

- [x] Is the pool already mounted? → "nothing to do"
- [x] For each config disk: is LUKS already open? → skip that disk
- [x] Are there zero unlockable disks (all absent or not-LUKS)? → clear error with guidance

## `braid lock`

- [x] Is a btrfs exclusive op already running? → refuse (unmounting during a balance would be bad)
- [x] Is the pool mounted? → skip umount if not
- [x] For each mapper: does it exist? → skip close if not
