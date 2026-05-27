# Plan: pin the LUKS-UUID identity join on the `braid status` surface (end-to-end)

## Context

A Medium/Testing finding flags that **no end-to-end test pins that a present
disk's rendered `luks_uuid` (and UUID-joined `name`) equals the real LUKS UUID**
written by `braid add` / stored in `pool.json`. That join is the central claim
of the UUID-identity migration (decision
[024](../../docs/design/decisions/024-luks-uuid-identity.md)) on the
user-visible status surface: `build_disk_reports` resolves a live pool device's
UUID back to the operator `DiskName` and falls back to the mapper basename only
for foreign devices (`cli/src/status.rs:947-1009`).

What's actually covered today:

- `tests/cli/braid-status-rust.py` asserts only that the `luks_uuid` *key*
  exists (`:113`), never the value; the JSON loop never checks the `name` field
  at all; the degraded JSON block (`:156-159`) asserts only present/unknown
  *counts*.
- `tests/cli/braid-status.py` never references `luks_uuid` and never checks
  `name` values either.
- Human-output assertions use substring `disk in output`, which passes even on
  the `braid-diskN` mapper fallback (`"disk1" in "braid-disk1"` is `True`).
- Rust unit tests (`status.rs` `build_disk_reports_*`, `status_compact_*`) cover
  the join with **mocked** `PoolState`/`PoolMembership` only -- never the wired
  `status -> probe -> membership` path against a real cryptsetup-written UUID.
- `braid-add-persists-before-balance.py` checks the `pool.json` key is canonical
  UUID *form*, but never compares it to the device's real `cryptsetup luksUUID`.

So a regression that resolved the present-disk `name`/`luks_uuid` from the
mapper basename or by-id -- the exact "reconstruct identity from `braid-<name>`"
hazard decision 024 forbids -- would pass the entire VM suite.

**Why the finding's literal fix is not enough (and what "ideal" adds).** Asserting
`name == "disk1"` in the normal phases has a blind spot: the mapper there *is*
`braid-disk1`, so a regression that derived the name by stripping the `braid-`
prefix off the mapper would still yield `"disk1"` and pass. The decision-024
drift-tolerance claim is only fully closed by a **genuine mapper-drift**
scenario where the mapper does not encode the operator name. The established
braid pattern for this is a dedicated single-scenario VM test: `lock`'s side of
the same invariant lives in `tests/cli/luks-mapper-drift.py`; the status side
deserves the symmetric guard.

**Confirmed additive, not a fix.** In the drift scenario `probe_config_disk` for
disk1 finds the *expected* mapper `braid-disk1` inactive (disk1 is open as
`braid-WRONG`) and returns `PresentLuks { mapper_open: false }` with no error
(cf. `probe.rs` `probe_config_disk_mapper_status_inactive_is_closed:1140`).
disk1's UUID is live in the pool, so the unpooled loop skips it
(`status.rs:1012-1018`) and the present row is emitted via the UUID join with
`name == "disk1"`, `mapper == "braid-WRONG"`. `braid status` therefore succeeds
under drift; this plan only adds coverage.

Outcome: the status surface's UUID->name join is pinned end-to-end against a
real cryptsetup-written UUID, in the normal/degraded states and under genuine
mapper drift.

## Part A -- strengthen `tests/cli/braid-status-rust.py` (value assertions, ~free)

These reuse the already-built 3-disk pool, so no extra VM boot.

1. **Capture real UUIDs once** in the `"Setup: 3-disk RAID1 pool"` subtest (the
   `cryptsetup luksUUID` read works regardless of mapper-open state):

   ```python
   real_uuids = {
       name: machine.succeed(
           f"cryptsetup luksUUID /dev/disk/by-id/virtio-{name}"
       ).strip()
       for name in ("disk1", "disk2", "disk3")
   }
   ```

2. **Healthy JSON subtest** (`:105-124`): after the existing key checks, pin the
   value join and triangulate against `pool.json`:

   ```python
   by_uuid = {d["luks_uuid"]: d for d in s["disks"]}
   pool = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
   key_by_name = {entry["name"]: key for key, entry in pool["disks"].items()}
   for name in ("disk1", "disk2", "disk3"):
       uuid = real_uuids[name]
       assert uuid in by_uuid, f"{name} real UUID {uuid} absent from status: {list(by_uuid)}"
       assert by_uuid[uuid]["name"] == name, (
           f"{name} must resolve to operator name via UUID join, got {by_uuid[uuid]['name']!r}"
       )
       assert key_by_name[name] == uuid, f"pool.json key for {name} != real LUKS UUID {uuid}"
   ```

   This ties all three sources: `cryptsetup luksUUID` == `pool.json` membership
   key == status `disks[].luks_uuid`, with `name` proven to be the operator
   name. The triangulation directly pins the finding's "matches pool.json
   membership key" framing and decision 024's single-source-of-truth claim.

3. **Degraded JSON subtest** (`:152-159`): for the present disks, add the same
   UUID->name assertion (closes the counts-only gap):

   ```python
   present_by_uuid = {d["luks_uuid"]: d for d in present_disks}
   for name in ("disk1", "disk2"):
       uuid = real_uuids[name]
       assert uuid in present_by_uuid, f"{name} real UUID missing from present disks"
       assert present_by_uuid[uuid]["name"] == name
   ```

## Part B -- new dedicated drift guard (the "ideal" piece)

A focused VM test mirroring `tests/cli/luks-mapper-drift.py`, which already
proves `lock`/`unlock` under drift -- this is the missing `status` sibling.

**New `tests/cli/status-mapper-drift.py`** (preamble in the
`# Intent: / # Why it exists: / # Scenario:` form used by sibling `.py` tests):

- Build a 2-disk pool (`disk1`, `disk2`) using the same `add_cmd` helper as
  `luks-mapper-drift.py:23-33`.
- Capture `uuid1 = machine.succeed("cryptsetup luksUUID /dev/disk/by-id/virtio-disk1").strip()`.
- `braid lock`, then reopen drifted exactly as `luks-mapper-drift.py:49-66`:
  open disk1 as `braid-WRONG` and disk2 as `braid-disk2` via
  `cryptsetup open --key-file=-`, `btrfs device scan`, then
  `mount -o noatime,skip_balance,subvolid=5 /dev/mapper/braid-WRONG /mnt/storage`.
- Assert the join survives genuine drift:

  ```python
  s = json.loads(machine.succeed("braid status --json"))
  assert s["status"] == "intact", s["status"]
  d1 = next(d for d in s["disks"] if d["luks_uuid"] == uuid1)
  assert d1["name"] == "disk1", f"drifted mapper must still resolve to operator name, got {d1['name']!r}"
  assert d1["mapper"] == "braid-WRONG", f"expected observed mapper, got {d1['mapper']!r}"
  assert d1["status"] == "present", d1
  d1_devid = d1["devid"]
  ```

  `name == "disk1"` (not `braid-WRONG`, not `WRONG`, not the by-id path) is what
  catches the mapper-basename, prefix-strip, and by-id regressions that Part A
  cannot.

- Pin the human path too, because it is rendered through the separate
  `compact_drives` / `human_details` plumbing:

  ```python
  human = machine.succeed("braid status")
  compact_rows = [
      line for line in human.splitlines()
      if "present" in line and f"devid={d1_devid}" in line
  ]
  assert len(compact_rows) == 1, f"expected one drifted compact row:\n{human}"
  compact_name = compact_rows[0].split()[0]
  assert compact_name == "disk1", (
      f"drifted compact row must render operator name disk1, got {compact_name!r}:\n{human}"
  )
  assert compact_name != "braid-WRONG", f"mapper basename leaked into compact row:\n{human}"

  detail_rows = [
      line for line in human.splitlines()
      if "present" in line and f"devid {d1_devid}" in line
  ]
  assert len(detail_rows) == 1, f"expected one drifted detail row:\n{human}"
  detail_name = detail_rows[0].split()[0]
  assert detail_name == "disk1", (
      f"drifted detail row must render operator name disk1, got {detail_name!r}:\n{human}"
  )
  assert detail_name != "braid-WRONG", f"mapper basename leaked into detail row:\n{human}"
  ```

  This avoids the existing substring blind spot (`"disk1" in "braid-disk1"`)
  and proves both human status rows name the drifted member by UUID-joined
  operator name. The observed drifted mapper is pinned by the JSON
  `d1["mapper"] == "braid-WRONG"` assertion, not by the compact row's device
  column, which renders `PoolDevice.underlying`.

**New `tests/cli/status-mapper-drift.nix`**: copy `luks-mapper-drift.nix`
verbatim except `name = "status-mapper-drift";` and the `readFile` target. It
already provisions two virtual disks (`serial = "disk1"/"disk2"`), the `braid` /
`btrfs-progs` / `cryptsetup` packages, and `/etc/braid/config.json`.

**Register in `flake.nix`** next to the existing entry (`flake.nix:492-496`):

```nix
status-mapper-drift = pkgs.testers.nixosTest (
  import ./tests/cli/status-mapper-drift.nix {
    braid = linuxCrane.braid;
  }
);
```

## Part C -- keep decision 024 accurate

Add to the "Tests That Enforce This" list in
`docs/design/decisions/024-luks-uuid-identity.md` (after the existing
`cli/src/status.rs` unit-test bullet):

- `tests/cli/braid-status-rust.py` pins that present disks' rendered
  `luks_uuid` equals the real cryptsetup UUID and the `pool.json` membership
  key, and that `name` is the operator name, in intact and degraded states.
- `tests/cli/status-mapper-drift.py` pins that `braid status` resolves the
  operator name via the UUID join when a member is open under a drifted mapper
  (`braid-WRONG`), not the mapper basename, in both JSON and human output.

## Critical files

- `tests/cli/braid-status-rust.py` (edit -- Part A)
- `tests/cli/status-mapper-drift.py` (new -- Part B)
- `tests/cli/status-mapper-drift.nix` (new -- Part B; copy of `luks-mapper-drift.nix`)
- `flake.nix` (register the new check, ~`:492`)
- `docs/design/decisions/024-luks-uuid-identity.md` (Part C)

Reuse, don't reinvent: `add_cmd`/drifted-open scaffolding from
`luks-mapper-drift.py`; the `cat /var/lib/braid/pool.json` + `json.loads` +
`pool["disks"].items()` pattern from `braid-add-persists-before-balance.py:18-26`
and `braid-monitor.py:103-123`; `.strip()` on every `cryptsetup luksUUID`
capture.

## Verification

1. `just test-vm braid-status-rust status-mapper-drift` -- both pass.
2. `just test-rust` -- sanity (untouched, should stay green).
3. **Confirm the guards bite** (do not commit): temporarily edit
   `build_disk_reports` (`status.rs:953-960`) so the present-disk `disk_name`
   comes from `pd.mapper.0` (or `pd.mapper.0.strip_prefix("braid-")`), and
   re-run step 1.
   - `pd.mapper.0`: both `braid-status-rust.py` and `status-mapper-drift.py`
     fail.
   - `strip_prefix("braid-")`: `braid-status-rust.py` still passes (the blind
     spot), `status-mapper-drift.py` fails -- demonstrating why the dedicated
     drift test is the piece that closes the hazard. Revert the edit after.
