[← Manual](../index.md)

# braid doctor

Runs diagnostic checks on your braid configuration, pool health, RAID profile consistency, LUKS headers, and alerting hardware. Reports issues and suggests fixes.

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
[ok]   declared disks  all 3 declared disk(s) present
[ok]   missing devs    no missing devices
[ok]   data profiles   data profile: RAID1
[ok]   meta profiles   metadata profile: RAID1
[ok]   smart selftest disk1  passed ~2 days ago
[ok]   smart selftest disk2  passed ~12 days ago
[ok]   smart selftest disk3  passed ~30 days ago
[skip] alert beep      skipped (pass --beep to play the audible alert test beep)
```

The SMART self-test check emits one row per pool drive. If a drive has no recent completed self-test, the row includes a paste-ready smartctl command:

```
[warn] smart selftest disk2  no completed SMART self-test recorded -- run: smartctl -t short /dev/disk/by-id/...
```

To test the real alert sound:

```
sudo braid doctor --beep
```

## Machine-readable output

```
sudo braid doctor --json
```

Prints a JSON object with `status` (one of `ok`, `warn`, `fail`, `skip`) and a `checks` array. Each check has `name`, `status`, and `message`. Per-drive checks also include `subject`.

Note: `--json` mode skips the alert beep test even when combined with `--beep` (no audible side effects in machine-readable output). The check still appears in the report as `skip`.

## What it checks

| Check | What it does |
| --- | --- |
| `config_file` | Config exists and is valid JSON |
| `config_schema` | Required fields present and deserializable |
| `config_permissions` | Canonical `/etc/braid/config.json` is not world-writable and is owned by root; custom `--config` paths skip this check |
| `declared_disks` | Every UUID-keyed pool.json member is present, has a readable LUKS header, and its live LUKS UUID matches the pool.json key |
| `pool_missing_devices` | No btrfs missing devices in the live pool |
| `data_profile_mismatch` | Data block groups all use the same RAID profile |
| `metadata_profile_mismatch` | Metadata block groups all use the same RAID profile |
| `smart_self_test` | One result per pool drive: runs `smartctl --json -A -l selftest <by-id>` against each, then reports `Fail` on an active SMART self-test failure, `Warn` if no completed test in the last 90 powered-on days (or never), `Ok` otherwise, or `Skip` for NVMe/SCSI/unsupported drives. In `--json`, every per-drive result carries `name: "smart_self_test"` and a `subject` field naming the pool member; if pool membership is missing or empty, a single `Skip` result with `name: "smart_self_test"` is emitted; if pool membership is corrupt or unreadable, a single `Warn` result with the same `name` is emitted instead. In both fallbacks the `subject` field is omitted. Scripts should check whether `subject` is present before keying on it. |
| `beep_path` | PC speaker alert beep is configured; with `--beep`, the alert beep command succeeds |
| `ups_daemon` | With UPS enabled, `upsc` is available and can query the UPS daemon; missing or spawn-failed `upsc` is a failure, daemon unreachable/non-zero `upsc` is a warning |
| `braid_online_active` | With UPS enabled and the pool mounted, `braid-online.service` is active so shutdown unmounts the pool |

## Flags

| Flag | Effect |
| --- | --- |
| `--json` | Machine-readable JSON output (suppresses alert beep test) |
| `--beep` | Play the audible alert test beep (ignored in `--json` mode) |

## Exit codes

- **0** -- all checks passed (ok/warn/skip)
- **1** -- at least one check failed

## What happens under the hood

1. Reads and validates `/etc/braid/config.json`.
2. Loads UUID-keyed `pool.json` and probes each declared disk via `cryptsetup isLuks`, `cryptsetup luksDump`, and `cryptsetup luksUUID`.
3. If the pool is mounted, queries `btrfs filesystem df` to check RAID profile consistency and probes for missing devices.
4. For each declared disk, runs `smartctl --json -A -l selftest <by-id>` and parses the self-test log to detect active failures and report the age of the most recent passing entry.
5. If the braid monitor NixOS module is configured, reports the alert beep check as skipped by default.
6. With `--beep` and without `--json`, plays a short test beep through the canonical beep wrapper.
7. If UPS support is enabled, checks `upsc` and the mounted-pool `braid-online.service` shutdown hook.
8. Aggregates results and prints a summary.

## Related commands

- [status](status.md) -- live pool health, disk usage, scrub status
- [monitor](monitor.md) -- automated health check for alerting
