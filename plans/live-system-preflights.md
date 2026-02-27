# Live Check System: Pre-flight checks per command

## Context

braid needs to lean harder into **live state detection**: before any mutating operation, probe the current reality and refuse to proceed if the world isn't clean nor ready nor compatible. This plan catalogs what each command should check and what's already implemented.

We want these cleanly and clearly itemized at the top of every braid command function body, like a checklist.

We want to accumulate all the checks here in one place before implementing them. Some checks happen to already be implemented before this planfile was created; for those we mark with `[x]`.

## `braid add <key>`

- [ ] Is a btrfs exclusive op (balance/device remove) already running? → refuse
- [x] Is the config disk present? → error if absent
- [x] Is the LUKS mapper already open? → skip LUKS open
- [ ] Is the mapper open but device NOT in the pool? → partial prior add (LUKS opened, but btrfs device add never ran)
- [x] Is the device already in the pool? → "nothing to do"
- [ ] Does the pool have missing devices? → warn about adding to a degraded pool

## `braid remove <key>`

- [ ] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Is the device in the pool? → error if not
- [x] ENOSPC pre-flight check
- [x] Would this leave 0 devices? → error

## `braid remove-missing`

- [ ] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Are there actually missing devices? → error if not
- [x] ENOSPC pre-flight check

## `braid replace --old <key> --new <key>`

- [ ] Is a btrfs exclusive op already running? → refuse
- [x] Is the pool mounted? → error if not
- [x] Is the new disk present? → error if absent
- [ ] Is the new disk's mapper already open but not in pool? → partial prior replace
- [x] Is the old disk live or dead? → determines eviction path
- [x] If old disk is live, does pool have other missing devices? → refuse

## `braid unlock`

- [x] Is the pool already mounted? → "nothing to do"
- [x] For each config disk: is LUKS already open? → skip that disk

## `braid lock`

- [ ] Is a btrfs exclusive op already running? → refuse (unmounting during a balance would be bad)
- [x] Is the pool mounted? → skip umount if not
- [x] For each mapper: does it exist? → skip close if not
