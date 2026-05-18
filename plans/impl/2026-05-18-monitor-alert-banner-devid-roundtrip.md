# Tighten braid-monitor.py to lock the alert-banner devid round-trip

## Context

A review finding (verify-issue) flagged that the `ALERT --` banner from `braid
status` is not exercised against a real mounted-but-degraded pool by any Python
VM test, leaving the monitor->latch->status->name-resolution path unprotected at
the end-to-end layer. Four pure-Rust unit tests in `cli/src/status.rs:4036-4127`
lock the formatter against hand-built `StatusReport`s + `devid_names` maps, but
they cannot prove that `build_devid_names` actually runs in `cmd_status` with a
latched `MissingDevice` cause from `cmd_monitor`. A regression that dropped the
name resolution back to `missing device: devid N` would pass every existing VM
assertion.

The finding proposed extending `tests/cli/braid-status-rust.py` Phase 3, but the
exact simulate-failure -> `braid monitor` -> `braid status` flow already lives
in `tests/cli/braid-monitor.py:75-99`. That existing subtest currently asserts
only the substrings `"ALERT"`, `"braid ack"`, `"missing device"`, and cause type
`"missing_device"` -- none of which would catch a name-resolution regression.

This plan **pivots** the finding: rather than duplicating the simulate-failure
scenario in a new file, tighten the existing assertions in place. The 2026-05-14
plan that fixed the underlying bug
(`plans/impl/2026-05-14-status-missing-device-name.md:261-266`) explicitly
flagged adding this VM-layer assertion as a follow-up; this is that follow-up.

Intended outcome: a single VM check (`braid-monitor`) locks the full
`monitor latch -> status banner` round-trip. Human output asserts the resolved
disk name (`missing device: disk2 (devid N)`); JSON output asserts the
latched cause carries the expected devid (`AlertCause` JSON has no name field,
so JSON can only prove the latched devid -- name resolution is human-only). A
future regression in `devid_to_name`, `build_devid_names`, or the
`MountedExtras.devid_names` plumbing fails the human assertion; a regression in
the monitor->latch->status devid round-trip fails the JSON assertion.

## Critical files

- `tests/cli/braid-monitor.py` -- only file edited. Two imports added
  (`re`, `shlex`), one `get_devid` helper added, one new subtest seeding
  `pool.json` with per-disk LUKS UUID + btrfs devid + name, two human-output
  assertions tightened, one new JSON assertion added.

## Design

### 1. Add a top-level `get_devid` helper

Copy the live-btrfs-authoritative pattern verbatim from
`tests/cli/replace-preserves-devid.py:65-73` (also used in
`tests/cli/replace-dead-disk.py:69`). Place it next to the existing
`acked_stats_fingerprint` helper at `tests/cli/braid-monitor.py:23-28`. Add
`import re` next to the existing `import json` at line 14 -- the file does not
import `re` today.

```python
def get_devid(mapper_name):
    """Extract the btrfs devid for a given mapper from `btrfs fi show`."""
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for line in fi_show.splitlines():
        if mapper_name in line:
            m = re.search(r"devid\s+(\d+)", line)
            if m:
                return int(m.group(1))
    raise AssertionError(f"devid not found for {mapper_name} in:\n{fi_show}")
```

Rationale for `btrfs fi show` over `braid status --json`:

- Decouples the new assertion from the very parser under test.
- Returns `int` directly, matching `AlertCause::MissingDevice.devid: u64`
  (serialized as a JSON integer per `cli/src/alert.rs:611` -- distinct from
  `DiskReport.devid: Option<String>` at `cli/src/status.rs:186`).
- Reuses the established braid VM-test idiom.

### 2. Seed `/var/lib/braid/pool.json` membership before failure

The setup at `tests/cli/braid-monitor.py:31-44` builds the pool with raw
`cryptsetup luksFormat` + `mkfs.btrfs` and never calls `braid add`, so
`/var/lib/braid/pool.json` does not exist. `cmd_status` at
`cli/src/status.rs:406-412` falls back to `PoolMembership::empty()` on
`ErrorKind::NotFound`, after which `build_devid_names`
(`cli/src/status.rs:268-299`) has no `membership.by_devid` hit for the missing
devid and the banner renders `missing device: devid N` -- not the named form
the new assertion checks. Without seeded membership the assertion cannot pass.

Insert a new subtest between the existing
"Healthy mounted pool: ack is a durable no-op" subtest (lines 63-72) and
"Simulate disk failure" (line 75). Adjacent placement minimizes risk of the
existing healthy-pool subtests observing the seeded membership in a way that
changes their substring assertions; the healthy "no ALERT" subtest at lines
50-52 only checks `"ALERT" not in output`, so even if membership were seeded
earlier the assertion would still hold, but adjacent placement keeps the
behavioral diff visible to a reader.

```python
with subtest("Seed pool.json membership before failure"):
    members = {}
    devids_by_name = {}
    for name in ["disk1", "disk2", "disk3"]:
        luks_uuid = machine.succeed(
            f"cryptsetup luksUUID /dev/disk/by-id/virtio-{name}"
        ).strip()
        devid = get_devid(f"braid-{name}")
        members[luks_uuid] = {
            "name": name,
            "by_id": f"/dev/disk/by-id/virtio-{name}",
            "devid": devid,
            "added_at": "2024-01-01T00:00:00Z",
        }
        devids_by_name[name] = devid
    pool_json = json.dumps(
        {"disks": members}, sort_keys=True, separators=(",", ":")
    )
    machine.succeed(
        f"printf '%s' {shlex.quote(pool_json)} > /var/lib/braid/pool.json"
    )
    disk2_devid = devids_by_name["disk2"]
```

The capture must occur before line 77's `cryptsetup close braid-disk2` --
after closure, `btrfs fi show` reports `MISSING` for that row instead of a
devid number. (`cryptsetup luksUUID` itself reads the on-disk header and works
fine against a by-id path of a closed device, but capturing all three disks
together in one place is simpler than interleaving with the failure
simulation.)

Add `import shlex` next to the planned `import re` at line 14 -- the file does
not import `shlex` today. The shape (`{"disks": {UUID: {name, by_id, devid,
added_at}}}`) and the `printf ... | shlex.quote` write idiom are copied from
`tests/cli/braid-discover-migration.py:25-28` and validated against the
production schema at `cli/src/membership.rs:233-248` (UUID-keyed map of
`DiskMember` records, value side carries `name` / `by_id` / optional `devid` /
optional `added_at`, `#[serde(deny_unknown_fields)]` rejects extras). The
`cryptsetup luksUUID` pattern is reused from
`tests/cli/replace-cloned-luks-header-rejected.py:76-77`.

### 3. Tighten the human-output banner assertions (lines 88-92)

The current subtest "Degraded pool: status shows ALERT banner" reads:

```python
with subtest("Degraded pool: status shows ALERT banner"):
    output = machine.succeed("braid status")
    assert "ALERT" in output, f"Expected ALERT in degraded status, got: {output}"
    assert "braid ack" in output, f"Expected 'braid ack' hint in status, got: {output}"
    assert "missing device" in output, f"Expected 'missing device' cause in status, got: {output}"
```

Two changes:

- `"ALERT"` -> `"ALERT -- disk health issue detected."` -- locks the literal
  banner header from `cli/src/status.rs:949`. Catches accidental rewording or
  Unicode em-dash drift (the project's CLI-output style requires `--`, not `—`,
  per `AGENTS.md`).
- `"missing device"` -> `f"missing device: disk2 (devid {disk2_devid})"` --
  locks the resolved name and devid against the `cli/src/status.rs:957-960`
  format. This is the regression guard that pure-Rust tests cannot give us.

The `"braid ack"` hint assertion is unchanged -- the finding is about the
banner / name resolution, not the hint line.

### 4. Tighten the JSON cause assertion (lines 94-99)

`AlertCause` (`cli/src/alert.rs:25-32`) is `#[serde(tag = "type",
rename_all = "snake_case")]` and the `MissingDevice` variant carries only
`{devid: u64}` -- there is no name field on the wire. So JSON output can only
prove the latched cause carries the expected devid; the resolved disk name is
human-output-only (see Section 3) and the JSON assertion locks the
monitor->latch->status devid round-trip, not name resolution.

The current subtest "Degraded pool: status --json shows alert" reads:

```python
with subtest("Degraded pool: status --json shows alert"):
    json_output = machine.succeed("braid status --json")
    report = json.loads(json_output)
    assert report["alert_active"] == True, f"Expected alert_active=true, got: {report}"
    cause_types = [c["type"] for c in report["alert_causes"]]
    assert "missing_device" in cause_types, f"Expected missing_device cause, got: {cause_types}"
```

Add an assertion that the `missing_device` cause carries the captured devid:

```python
missing_causes = [
    c for c in report["alert_causes"]
    if c["type"] == "missing_device" and c.get("devid") == disk2_devid
]
assert missing_causes, (
    f"Expected missing_device cause with devid={disk2_devid}, got: {report['alert_causes']}"
)
```

The existing `cause_types` substring check is kept -- it surfaces a cleaner
failure message when the cause type is missing entirely vs. when only the devid
field is wrong.

## Reused code, no new abstractions

- `get_devid` helper -- copied verbatim from
  `tests/cli/replace-preserves-devid.py:65-73`. Not promoted to a shared
  module: each VM test file is self-contained today, and one duplicate is
  cheaper than building a test-library import path.
- `cryptsetup luksUUID /dev/disk/by-id/virtio-<name>` UUID-extraction pattern
  -- reused from `tests/cli/replace-cloned-luks-header-rejected.py:76-77` and
  `tests/cli/replace-enroll-existing-luks.py:68-69`. Inlined into the seed
  loop, not extracted as a helper (one call site).
- Pool.json write idiom (`printf '%s' {shlex.quote(...)} > .../pool.json` with
  `mkdir -p /var/lib/braid` already satisfied by line 45 of the existing
  setup) -- reused from `tests/cli/braid-discover-migration.py:25-28`.
- Pool.json schema -- value-side fields `name`, `by_id`, `devid`, `added_at`
  per `cli/src/membership.rs:233-248`. UUID-keyed top-level map. No
  `luks_uuid` on the value side (the loader rejects it via
  `deny_unknown_fields`). Validated against the production shape that
  `tests/cli/braid-add-persists-before-balance.py:104-121` asserts on.
- AlertCause JSON shape `{"type": "missing_device", "devid": N}` -- already
  exercised by `cli/src/alert.rs:611` (legacy-fixture round-trip test) and
  serialized via `#[serde(tag = "type", rename_all = "snake_case")]` at
  `cli/src/alert.rs:26`. No format assumption made beyond what the wire
  contract already documents.

## Out of scope

- **`tests/cli/braid-status-rust.py`** -- the finding's proposed location.
  Duplicates the simulate-failure scenario already in `braid-monitor.py`;
  Phase 3 of that file deliberately stays monitor-free to keep `cmd_status`
  scenarios decoupled from the alert-latch state machine.
- **`tests/cli/monitor-hot-unplug.py`** -- focuses on the monitor exit-code
  contract over hot-unplug, not banner content. Tightening can be a follow-up
  if it ever surfaces a missed regression.
- **BtrfsDeviceErrors / SmartdAlert banner variants** -- the unit tests at
  `cli/src/status.rs:4054` and `cli/src/status.rs:4094` cover their formatting.
  The end-to-end gap that motivated this plan is specifically `MissingDevice`,
  where the devid-resolution fallback bug originally happened
  (`plans/impl/2026-05-14-status-missing-device-name.md:1-33`).
- **Promoting `get_devid` to a shared VM-test helper module** -- premature;
  three copies (this file, `replace-preserves-devid.py`, `replace-dead-disk.py`)
  is below the threshold for extraction.

## Verification

1. `just test-vm braid-monitor` -- the tightened test must pass against the
   current code (the banner format is already correct on master after
   `plans/impl/2026-05-14-status-missing-device-name.md` shipped, and the
   seeded `pool.json` membership unblocks `build_devid_names` for the missing
   devid).
2. Sanity-check the regression guard for human output: temporarily replace the
   body of `devid_to_name` in `cli/src/status.rs:931-936` with
   `format!("devid {devid}")` (drop the named-fallback branch) and rerun
   `just test-vm braid-monitor`. In the current build pipeline, this exact
   mutation is caught by Rust formatter tests in the `braid-cli` package build
   before the VM boots. Revert the temporary edit. Manual confirmation, not a
   permanent test -- do it once during implementation review.
3. Sanity-check the JSON devid round-trip guard: temporarily mutate the
   `MissingDevice` arm of `compute_alert_state` in `cli/src/alert.rs` (or the
   monitor's classification) to emit `MissingDevice { devid: 999 }` instead of
   the live devid; rerun. In the current build pipeline, this exact mutation is
   caught by Rust alert tests in the `braid-cli` package build before the VM
   boots. Revert. Same caveat -- manual one-shot, not a permanent test.
4. `just test-rust` -- the four pure-Rust alert tests
   (`alert_missing_device_uses_devid_names_map`,
   `alert_missing_device_falls_back_when_map_missing_entry`,
   `alert_btrfs_errors_shows_name`,
   `alert_btrfs_errors_foreign_live_mapper_keeps_basename` at
   `cli/src/status.rs:4036-4127`) must still pass. They are untouched.
5. The "Corrupt latch" and "MissingDevice acked offline" subtests further down
   in `braid-monitor.py` (lines 134-321) reuse the same mounted-degraded state
   set up by the tightened subtest with the seeded `pool.json` now present --
   confirm they still pass unchanged. Specifically, the offline-ack subtest at
   lines 211-245 reads `acked_stats` and the corrupt-latch subtests don't
   touch membership, so they are insensitive to the new `pool.json`. Run the
   full file (not just the tightened subtests) to confirm.
