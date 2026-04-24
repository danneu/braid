# Fix `scripts/braid-destroy.sh` -- validated pool.json reader + VM regression

## Context

`scripts/braid-destroy.sh` is a dev-only "nuke the pool" helper (added in
`bfb84f3`, not shipped as part of braid). A walkthrough today found it is
half-broken:

- It reads disk membership from `/etc/braid/config.json` via
  `jq -r '.disks | keys[]'` (line 20).
- Commit `74feca5` ("move disk membership from nix config to cli-owned
  runtime state", 2026-03-26) moved disk membership to
  `/var/lib/braid/pool.json`. `config.json` is now `{mount_point,
  fan_control?, ups?}` only (`cli/src/config.rs:35-41`,
  `modules/braid/cli.nix:13-36`). No `.disks` field.
- The `mapfile -t keys < <(jq ...)` pattern makes the silent-failure path
  worse: `jq` errors to stderr but its exit status is swallowed by the
  process substitution (neither `set -e` nor `pipefail` propagate through
  `<(...)`). `keys` ends up empty, the wipe loop runs zero times, but
  `braid lock` and `rm -rf /var/lib/braid/` still run. LUKS signatures
  remain on every disk; state (including `luks-headers/` backups) is
  gone. The script still prints "Pool destroyed".

Two review findings on the previous draft:

- **No regression coverage** for a behavior change on a destructive path
  in a repo that already runs throwaway-disk VM tests for storage.
- The proposed non-empty-`.disks` guard doesn't catch malformed entries
  like `{"disks":{"disk1":{}}}` -- the script would still pass the guard,
  run `braid lock`, then die partway through `wipefs`, tearing down the
  pool before it discovers it can't finish.

Goal: switch the source of truth to `pool.json`, kill the
process-substitution footgun, validate every member up-front, and pin the
behavior with a VM test so this regression can't come back silently.

## Approach

### 1. Single validated membership-reader in bash

Mirror the pattern used by `tests/hw/runner.py:89-109`
(`devices_from_pool`), which is the project's established
read-and-validate shape: open `pool.json`, require `.disks` non-empty,
require every member to have a non-empty `by_id`, die with a clear
message otherwise.

In bash, collapse the current two ad-hoc `jq` calls (lines 20, 23) into
one `jq` filter that does all the validation and emits `name<TAB>by_id`
tuples, and replace the process-substitution `mapfile` read with a
command-substitution + `pipefail` pipeline so a `jq` failure aborts the
script.

```bash
set -o pipefail
pool_json="/var/lib/braid/pool.json"

[ -f "$pool_json" ] || {
    echo "Error: $pool_json not found -- no pool to destroy." >&2
    echo "If you want to clear residual state, run: sudo rm -rf /var/lib/braid/" >&2
    exit 1
}

read_filter='
  if (.disks // {} | length) == 0 then
    "pool has no disks\n" | halt_error(1)
  else
    .disks | to_entries[] as $e
    | if ($e.value.by_id // "") == "" then
        "disk \"\($e.key)\" has no by_id in pool.json\n" | halt_error(1)
      else
        [$e.key, $e.value.by_id] | @tsv
      end
  end'

tsv="$(jq -r "$read_filter" "$pool_json")" || exit 1
```

The arrays are then built in the parent shell via a here-string (not a
process substitution):

```bash
declare -a keys
declare -A by_id
while IFS=$'\t' read -r name path; do
    keys+=("$name")
    by_id[$name]="$path"
done <<< "$tsv"
```

This is the "removes the process-substitution footgun" the review asked
for: `pipefail` + command substitution means a jq error is a script
error; the here-string feeds the `while` loop in the current shell, so
array mutations persist.

Ordering (lock -> wipefs -> `rm -rf /var/lib/braid/`) stays as-is:
`braid lock` needs `pool.json` to derive mapper names
(`cli/src/lock.rs` loads membership before closing), so `pool.json`
cannot be wiped before lock. `rm -rf /var/lib/braid/` still catches
every file enumerated in `cli/src/state_paths.rs:19-42` (`pool.json`,
`pending-op.json`, `acked-stats.json`, `smartd-alert`,
`alert-latch.json`, `luks-headers/`); no systemd-owned state lives
under that path.

Positional `$1 = /etc/braid/config.json` stays -- it still flows through
to `sudo braid lock --config "$config"`, and `--config` is the global
braid flag (`cli/src/main.rs:16-17`). `justfile:236-238` does not
change.

Why not shell out to `braid status --json` (which exists at
`cli/src/status.rs:169-172` as `DiskReport { name, by_id, ... }`):
`status` does live probing via `lsblk`, `luks::probe_luks_header`, and
`smartctl` (`cli/src/status.rs:478, 872`). A half-broken pool -- exactly
when you reach for `destroy` -- may make it fail or hang. Reading
`pool.json` directly is an offline file read that works on a sick pool.

### 2. NixOS VM regression test

Add `tests/cli/braid-destroy.{nix,py}` modeled on
`tests/cli/braid-remove-disk.{nix,py}` (closest analog: it builds a real
pool via `braid add`, then exercises a destructive CLI path). Register
it in `flake.nix` under `checksFor` following the existing pattern at
`flake.nix:116-120` (`braid-add-disk`) / `flake.nix:146-150`
(`braid-remove-disk`).

Machine config (`braid-destroy.nix`):

- 3 `virtualisation.emptyDiskImages` with
  `driveConfig.deviceExtraOpts.serial = "disk1"` etc. -- same shape as
  `braid-remove-disk.nix:17-30`.
- `environment.systemPackages = [ braid pkgs.cryptsetup pkgs.btrfs-progs
  pkgs.jq ]`. `cryptsetup` for `isLuks` post-condition assertions;
  `btrfs-progs` for the `braid add` pool-build path (same rationale as
  `braid-remove-disk.nix:32-36`); `jq` because the destroy script's
  preflight requires it (`scripts/braid-destroy.sh:9`) -- without it the
  test would fail in preflight rather than exercising the read-and-
  validate path we're trying to cover. `lsblk` and `wipefs` come from
  the default `util-linux` in NixOS.
- `environment.etc."braid/config.json".text = builtins.toJSON {
  mount_point = "/mnt/storage"; }`.
- `environment.etc."braid-destroy.sh" = { source =
  ../../scripts/braid-destroy.sh; mode = "0755"; };` -- installs the
  *actual* repo script at a fixed path, so the test exercises the literal
  file. Any future drift in the script surfaces in CI.
- `testScript = builtins.readFile ./braid-destroy.py;`

Test script (`braid-destroy.py`), four scenarios:

**Scenario 1 -- happy path.** Build a 2-disk RAID1 pool via `braid add`
(reuse the helper pattern from `braid-remove-disk.py:33-40`). Confirm
`/var/lib/braid/pool.json` is present and each disk reports
`crypto_LUKS` via `cryptsetup isLuks /dev/disk/by-id/virtio-disk1`
(exit 0). Run `echo YES | bash /etc/braid-destroy.sh
/etc/braid/config.json` via `machine.succeed`. Assert:

- Script exit code 0.
- `/var/lib/braid/` does not exist (`! test -e /var/lib/braid`).
- `cryptsetup isLuks /dev/disk/by-id/virtio-disk1` exits non-zero for
  each former pool disk -- proves LUKS signatures were actually wiped.
- **Pool was unmounted**: `mountpoint -q /mnt/storage` exits non-zero.
- **Mappers were closed**: `test -e /dev/mapper/braid-disk1` (and
  `braid-disk2`) exits non-zero.

The last two are load-bearing: without them, a regression that skips
`braid lock` but still reaches `wipefs` + `rm -rf` would pass scenario
1 on the state + LUKS assertions while leaving `/mnt/storage` mounted
and mappers open. The mount/mapper checks pin that lock actually ran.
  (Use the `cmd || ec=$?` pattern to capture non-zero exits per the
  reference memory on `set -euo pipefail`.)

**Scenario 2 -- empty `by_id` in a live-shape pool.json fails closed,
before `braid lock` runs.** After the happy path, rebuild the pool
(`braid add disk1 ...`, `braid add disk2 ...`) so mappers `braid-disk1`
and `braid-disk2` are open and `/mnt/storage` is mounted. Overwrite
`/var/lib/braid/pool.json` with
`{"disks":{"disk1":{"by_id":""},"disk2":{"by_id":""}}}`.

The shape is deliberate:

- Keys (`disk1`, `disk2`) match the live mapper names, so if a
  regression let lock run first, lock would actually close the real
  mappers -- the ordering test would have something to catch.
- `by_id:""` is valid per `DiskMember`'s schema: `ByIdPath` is
  `#[serde(transparent)] pub struct ByIdPath(pub String)`
  (`cli/src/types.rs:4-6`) with no non-empty constraint, so
  `load_membership` (`cli/src/membership.rs:77-97`) parses it without
  error and the `braid lock` path at `cli/src/main.rs:503` proceeds
  into env-side work. **This is the key difference from a
  missing-`by_id` field**, which `braid lock`'s own deserializer would
  reject before doing anything -- a regression there would still pass
  the ordering assertions because lock aborted for its own reasons.
  Empty-string `by_id` exercises the shell-side validator as the *only*
  thing standing between the user and an unmount.

Run the script via `machine.fail`, capturing stderr. Assert:

- Non-zero exit.
- Stderr contains the validator's "no by_id" message.
- `/var/lib/braid/` still exists (no `rm -rf` of state).
- `cryptsetup isLuks` still exits 0 on each disk (no `wipefs`).
- **Pool is still mounted**: `mountpoint -q /mnt/storage` exits 0.
- **Mappers are still open**: `test -e /dev/mapper/braid-disk1` (and
  `braid-disk2`) exits 0.

The mount + mapper checks are the load-bearing assertions: they are the
only signals that distinguish "validator fired first" from "lock ran,
then validator rejected". Because `braid lock` can parse the malformed
membership here, the shell validator is the sole guard against env-side
work -- so these assertions fail exactly when the shell-side ordering
contract regresses.

After Scenario 2 passes, do *not* tear the pool down yet -- Scenario 3
reuses the same live pool.

**Scenario 3 -- empty `.disks` in a live-shape pool.json fails closed,
before `braid lock` runs.** Reuse the live pool from Scenario 2's
setup (mappers `braid-disk1`/`braid-disk2` open, `/mnt/storage`
mounted). Overwrite `/var/lib/braid/pool.json` with `{"disks":{}}`.

This shape is the third arm of the `jq` validator (non-empty `.disks`),
separate from the non-empty-`by_id` arm covered in Scenario 2. Without
a dedicated case, a regression that drops the `length == 0` check would
recreate the original destructive failure mode: zero wipe targets,
followed by `braid lock` and `rm -rf`. `braid lock`'s own loader
accepts an empty disks map fine (empty `BTreeMap` is valid), so again
the shell validator is the sole guard.

Run the script via `machine.fail`, capturing stderr. Assert:

- Non-zero exit.
- Stderr contains the validator's "no disks" message.
- `/var/lib/braid/` still exists.
- `cryptsetup isLuks` still exits 0 on each disk.
- `mountpoint -q /mnt/storage` exits 0.
- `test -e /dev/mapper/braid-disk1` (and `braid-disk2`) exits 0.

After Scenario 3 passes, tear down before Scenario 4 using *direct
primitives*, not `braid lock`: `umount /mnt/storage`, then
`cryptsetup close braid-disk1`, `cryptsetup close braid-disk2`, then
`rm -rf /var/lib/braid`. This is deliberate: `braid lock` loads
`pool.json` (`cli/src/lock.rs`) and a malformed or empty-`.disks`
membership puts it in orphan-lock territory whose behavior is outside
this test's contract. Using `umount` + `cryptsetup close` keeps the
teardown membership-independent, so a future `braid lock` regression
doesn't fail this test for unrelated reasons.

**Scenario 4 -- missing `pool.json` fails closed, state preserved.**
After Scenario 3's teardown, state is gone. Recreate a bare
`/var/lib/braid/` containing only a sentinel file
(`mkdir -p /var/lib/braid && touch /var/lib/braid/sentinel-no-pool`).
Run the script via `machine.fail`. Assert:

- Non-zero exit.
- Stderr contains the "no pool to destroy" message.
- `/var/lib/braid/sentinel-no-pool` still exists -- the script did not
  `rm -rf` residual state on a missing `pool.json`.

This scenario pins the "no pool to destroy" preflight
(`scripts/braid-destroy.sh`'s new `[ -f "$pool_json" ]` guard) against
a future regression that falls through. Note: the primary fail-closed
signal here is the *error-message* assertion, not the sentinel --
because the `tsv="$(jq ...)" || exit 1` already aborts on a missing
file via jq's own error, a regression that removes the preflight would
still exit non-zero and preserve the sentinel, just with jq's message
instead of the user-facing "no pool to destroy" one. The sentinel
remains a secondary fail-closed assertion in case both guards regress.

Scenario 2 pins the empty-`by_id` validator arm and the "reject before
lock" ordering. Scenario 3 pins the empty-`.disks` validator arm and
the same ordering invariant. Scenario 4 pins the missing-`pool.json`
preflight. Together they cover all three reject paths in the new
validator plus the file-existence preflight.

## Changes

- `scripts/braid-destroy.sh` -- rewrite read+validate block (lines
  17-33) with the single `jq` filter + command-substitution pattern
  above. Body of the script (confirm, lock, wipefs loop, `rm -rf`)
  stays.
- `tests/cli/braid-destroy.nix` -- new.
- `tests/cli/braid-destroy.py` -- new.
- `flake.nix` -- add `braid-destroy = pkgs.testers.nixosTest (import
  ./tests/cli/braid-destroy.nix { braid = linuxCrane.braid; });` under
  `checksFor`, next to the other `braid-*` cli entries (around
  `flake.nix:146-150`).

No changes to `justfile`. No changes to any Rust code.

## Critical files

- `scripts/braid-destroy.sh` -- the fix.
- `tests/hw/runner.py:89-109` -- reference pattern for the
  read-and-validate shape (open, require non-empty `.disks`, require
  non-empty `by_id` per member, die with clear errors).
- `cli/src/membership.rs:29-51` -- serialized shape of `pool.json`
  (`ByIdPath` is `#[serde(transparent)]` per `cli/src/types.rs:4-6`, so
  `by_id` is a bare JSON string).
- `cli/src/state_paths.rs:19-42` -- what `rm -rf /var/lib/braid/`
  catches.
- `cli/src/main.rs:16-17` -- `--config` global flag, used by
  `braid lock --config "$config"`.
- `tests/cli/braid-remove-disk.nix` / `.py` -- VM test template (disk
  layout, `systemPackages`, `config.json` stub, pool-build helpers).
- `flake.nix:106-170` -- `checksFor` registration block for new VM
  tests.

## Verification

1. `just test-vm braid-destroy` -- runs all four scenarios in the VM
   (happy path; empty-`by_id` reject-before-lock; empty-`.disks`
   reject-before-lock; missing-`pool.json` reject-before-`rm -rf`).
   This is the primary regression signal.
2. Revert the script to the broken `/etc/braid/config.json` read and
   re-run -- scenario 1 must fail (LUKS headers still present after
   "destroy"). Confirms the test actually fails on the regression it's
   meant to catch.
3. Revert only the `by_id` validation arm of the `jq` filter and
   re-run -- scenario 2 must fail (lock runs, pool unmounts, mappers
   close, `wipefs` then dies on empty path). Confirms scenario 2 pins
   the empty-`by_id` contract.
4. Revert only the `length == 0` arm of the `jq` filter and re-run --
   scenario 3 must fail (lock runs, then `rm -rf`, wipe loop is
   zero-iteration so LUKS headers stay but state is gone). Confirms
   scenario 3 pins the empty-`.disks` contract.
5. Move the validator to *after* the `braid lock` call (simulating the
   ordering regression the earlier High review finding called out) and
   re-run -- scenarios 2 and 3 must both fail on the mount/mapper
   assertions. Confirms the reject-before-lock ordering invariant.
6. Remove the new `[ -f "$pool_json" ]` preflight and re-run --
   scenario 4 must fail on the stderr "no pool to destroy" message
   assertion (the script would still exit non-zero via `jq`'s own
   file-not-found error and would still preserve the sentinel, so the
   sentinel assertion alone would pass; only the message assertion
   distinguishes the two failure modes). Confirms scenario 4 pins the
   user-facing preflight, not just the jq fallback.
7. `shellcheck scripts/braid-destroy.sh` -- quick sanity check on the
   rewritten bash.

## Out of scope

- The em-dash on line 5 of the source comment ("Dev use only -- not
  shipped as part of braid."). Not user-facing CLI output; bundling an
  ASCII sweep into this correctness patch is scope creep
  (`feedback_ascii_scope_whole_file.md`).
- Migrating the script to `braid status --json`. Rejected: `status`
  does live probing that isn't reliable on a pool you're trying to
  destroy.
- Stopping/disabling systemd units before wiping. `braid lock` is the
  user-facing teardown; the destroy script goes through it rather than
  duplicating its logic.
