[← braid](../index.md)

# braid doctor

Runs diagnostic checks on your braid configuration, pool health, RAID profile consistency, LUKS headers, auto-suspend wake path, and alerting hardware. Reports issues and suggests fixes.

## When to use it

- After initial setup, to verify everything is wired correctly.
- Periodically, to catch drift (missing disks, mixed RAID profiles, broken alert speaker).
- When something seems wrong and you want a quick health summary.

## Basic example

```
sudo braid doctor
```

Output:

```
[ok]   config file     /etc/braid/config.json exists and is valid JSON
[ok]   config schema   required fields present and valid
[ok]   config perms    /etc/braid/config.json permissions ok
[ok]   declared disks  all 3 declared disks present
[ok]   missing devs    no missing devices
[ok]   enospc risk     per-device unallocated space healthy
[ok]   foreign uuids   no foreign LUKS UUIDs in live pool
[ok]   data profiles   data profile: RAID1
[ok]   meta profiles   metadata profile: RAID1
[ok]   system profiles  system profile: RAID1
[ok]   meta pressure   metadata pressure within bounds
[ok]   paused balance  no paused balance
[ok]   smart selftest disk1  passed ~2 days ago
[ok]   smart selftest disk2  passed ~12 days ago
[ok]   smart selftest disk3  passed ~30 days ago
[skip] alert beep      skipped (pass --beep to play the audible alert test beep)
[skip] ups daemon      skipped (braid.ups not enabled)
[skip] braid-online    skipped (braid.ups not enabled)
[skip] wake-on-lan     skipped (braid.autoSuspend not enabled)
```

The SMART self-test check emits one row per pool drive. If a drive has no recent completed self-test, the row includes a paste-ready smartctl command:

```
[warn] smart selftest disk2  no completed SMART self-test recorded -- run: smartctl -t short /dev/disk/by-id/...
```

The hint uses the stable by-id path: braid's own diagnostic read prefers the member's live backing device, but a `smartctl -t short` you run later should use by-id, which survives reboots and controller reordering.

To test the real alert sound:

```
sudo braid doctor --beep
```

## Machine-readable output

```
sudo braid doctor --json
```

Prints a JSON object with `status` (one of `ok`, `warn`, `fail`, `skip`) and a `checks` array. Each check has `name`, `status`, and `message`. Per-drive checks also include `subject`.

`--json` mode never plays the alert beep test. The check still appears in the report as `skip`. `--json` and `--beep` conflict; run a separate `sudo braid doctor --beep` when you want to test the audible alert path.

## What it checks

| Check | What it does |
| --- | --- |
| `config_file` | Config exists and is valid JSON |
| `config_schema` | Required fields present and deserializable |
| `config_permissions` | Canonical `/etc/braid/config.json` is not world-writable and is owned by root; custom `--config` paths skip this check |
| `declared_disks` | Every UUID-keyed `pool.json` member is present, is a block device, has a readable LUKS header, its live LUKS UUID matches the `pool.json` key, and, when the pool is mounted, is assembled into the live btrfs pool. **Warn** if a member is missing, is not a block device, has an unreadable LUKS header or probe failure, is present and identity-verified but not assembled into the live pool (`offline`), or the pool is mounted but its live topology cannot be probed to verify assembly; **Fail** if a member's live LUKS UUID does not match its `pool.json` key. |
| `pool_missing_devices` | No btrfs missing devices in the live pool |
| `enospc_risk` | Warns when the pool is one disk-loss away from insufficient RAID1 chunk-pair space. Per-device threshold scales with pool size (min(1 GiB, 10% of total device bytes), matching the kernel's effective data chunk size) |
| `foreign_luks_uuid` | **Fail** when the live (mounted) pool contains a btrfs device whose LUKS UUID is not declared in `pool.json` (a foreign disk). The message names each foreign UUID and its mapper and suggests `btrfs device remove /dev/mapper/<mapper> <mount>` then `cryptsetup close <mapper>`. Skipped when the pool is not mounted. |
| `data_profile_mismatch` | Data block groups all use the same RAID profile |
| `metadata_profile_mismatch` | Metadata block groups all use the same RAID profile |
| `system_profile_mismatch` | System block groups all use the same RAID profile |
| `metadata_enospc_pressure` | Warns when metadata is near the next allocation threshold and fewer than two RAID1 devices have enough unallocated space for the next metadata chunk |
| `paused_balance` | Warns if a btrfs balance is paused on the mounted pool (e.g. a prior balance interrupted by reboot, manual pause, or kernel pause) and suggests resuming with `btrfs balance resume <mount>`. |
| `smart_self_test` | One result per pool drive: runs `smartctl --json -A -l selftest <device>` against each -- `<device>` is the member's live backing device (e.g. `/dev/sda`) when it is assembled into the mounted pool, otherwise its persisted by-id path (pool offline, probe failed, or that member not currently assembled -- e.g. missing or hot-unplugged on a degraded mount) -- then reports `Fail` on an active SMART self-test failure, `Warn` if no completed test in the last 90 powered-on days (or never), `Ok` otherwise, or `Skip` for NVMe/SCSI/unsupported drives. In `--json`, every per-drive result carries `name: "smart_self_test"` and a `subject` field naming the pool member; if pool membership is missing or empty, a single `Skip` result with `name: "smart_self_test"` is emitted; if pool membership is corrupt or unreadable, a single `Warn` result with the same `name` is emitted instead. In both fallbacks the `subject` field is omitted. Scripts should check whether `subject` is present before keying on it. |
| `beep_path` | PC speaker alert beep is configured; with `--beep`, the alert beep command succeeds |
| `ups_daemon` | With UPS enabled, `upsc` is available and can query the UPS daemon; missing or spawn-failed `upsc` is a failure, daemon unreachable/non-zero `upsc` is a warning |
| `braid_online_active` | With UPS enabled and the pool mounted, `braid-online.service` is active so shutdown unmounts the pool |
| `wake_on_lan` | With auto-suspend enabled, `ethtool <interface>` reports magic-packet wake support and active `Wake-on: g`; disabled, unsupported, missing, or unparseable WoL state is a failure |

## Flags

| Flag | Effect |
| --- | --- |
| `--json` | Machine-readable JSON output; never plays the alert beep test |
| `--beep` | Play the audible alert test beep; conflicts with `--json` |

## Exit codes

- **0** -- all checks passed (ok/warn/skip)
- **1** -- at least one check failed

## What happens under the hood

1. Reads and validates `/etc/braid/config.json`.
2. Loads UUID-keyed `pool.json` and probes each declared disk via `cryptsetup isLuks`, `cryptsetup luksDump`, and `cryptsetup luksUUID`.
3. If the pool is mounted, queries `btrfs filesystem df` and `btrfs device usage --raw` to check RAID profile consistency and metadata allocation headroom, probes for missing devices, reconciles each live pool member's LUKS UUID against `pool.json` to flag foreign devices, and runs `btrfs balance status` to detect paused balances.
4. For each declared disk, runs `smartctl --json -A -l selftest <device>` -- the member's live backing device when it is assembled into the mounted pool, otherwise its persisted by-id path (including a member that is missing or unassembled on a degraded but mounted pool) -- and parses the self-test log to detect active failures and report the age of the most recent passing entry. See [ADR-024](../design/decisions/024-luks-uuid-identity.md#benefits) for why present members are probed by live path rather than by-id.
5. If the braid monitor NixOS module is configured, reports the alert beep check as skipped by default.
6. With `--beep`, plays a short test beep through the canonical beep wrapper.
7. If UPS support is enabled, checks `upsc` and the mounted-pool `braid-online.service` shutdown hook.
8. If auto-suspend is enabled, runs `ethtool <interface>` to verify runtime Wake-on-LAN state.
9. Aggregates results and prints a summary.

## Related commands

- [status](status.md) -- live pool health, disk usage, scrub status
- [monitor](monitor.md) -- automated health check for alerting
