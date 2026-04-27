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
[skip] alert beep      skipped (pass --beep to play the audible alert test beep)
```

To test the real alert sound:

```
sudo braid doctor --beep
```

## Machine-readable output

```
sudo braid doctor --json
```

Prints a JSON object with `status` (one of `ok`, `warn`, `fail`, `skip`) and a `checks` array. Each check has `name`, `status`, and `message`.

Note: `--json` mode skips the alert beep test even when combined with `--beep` (no audible side effects in machine-readable output). The check still appears in the report as `skip`.

## What it checks

| Check | What it does |
| --- | --- |
| `config_file` | Config exists and is valid JSON |
| `config_schema` | Required fields present and deserializable |
| `config_permissions` | Canonical `/etc/braid/config.json` is not world-writable and is owned by root; custom `--config` paths skip this check |
| `declared_disks` | Every disk in pool.json is present and has a readable LUKS header |
| `pool_missing_devices` | No btrfs missing devices in the live pool |
| `data_profile_mismatch` | Data block groups all use the same RAID profile |
| `metadata_profile_mismatch` | Metadata block groups all use the same RAID profile |
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
2. Loads `pool.json` and probes each declared disk via `cryptsetup isLuks` and `cryptsetup luksDump`.
3. If the pool is mounted, queries `btrfs filesystem df` to check RAID profile consistency and probes for missing devices.
4. If the braid monitor NixOS module is configured, reports the alert beep check as skipped by default.
5. With `--beep` and without `--json`, plays a short test beep through the canonical beep wrapper.
6. If UPS support is enabled, checks `upsc` and the mounted-pool `braid-online.service` shutdown hook.
7. Aggregates results and prints a summary.

## Related commands

- [status](status.md) -- live pool health, disk usage, scrub status
- [monitor](monitor.md) -- automated health check for alerting
