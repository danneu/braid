# Plan: Replace BindsTo with ConditionPathIsMountPoint on braid-monitor.service

Status: **Implemented and verified** (`just test monitor-lifecycle` passes)

## Context

`braid-monitor.timer` starts at boot (`wantedBy = timers.target`) and fires every 5 minutes.
Before the pool is unlocked, `mnt-storage.mount` doesn't exist (no `fileSystems` entry — it's
auto-generated from `/proc/mounts` at runtime). Each timer firing tries to start
`braid-monitor.service`, which had `BindsTo=mnt-storage.mount`. systemd tries to pull in the
mount dependency, fails with "Unit mnt-storage.mount not found" (exit 5), and logs a
dependency-failure line every 5 minutes until unlock.

## Key finding during implementation

The original plan assumed `ConditionPathIsMountPoint` would be evaluated before dependency
resolution, allowing us to keep `BindsTo`. Testing proved this wrong — systemd resolves
dependencies first, then checks conditions. `BindsTo` on a non-existent unit causes a hard
failure before the condition is ever evaluated.

Fix: **remove `BindsTo` entirely**, replace with `ConditionPathIsMountPoint`. For a oneshot
that completes in <1s, `BindsTo`'s "stop if mount goes away" behavior has no practical value.

## Changes made

1. **`modules/braid/monitor.nix`** — Removed `bindsTo`, added `unitConfig.ConditionPathIsMountPoint = cfg.mountPoint`
2. **`tests/module/monitor-lifecycle.py`** — Subtests 2 and 8: `machine.execute` → `machine.succeed` (asserts clean condition-skip, not dependency failure). Updated header comments.
3. **`tests/module/monitor-lifecycle.nix`** — Updated header comment.
4. **`docs/decisions/systemd-lifecycle.md`** — Updated monitor contract and consumer dependency docs.
5. **`AGENTS.md`** — Updated monitor unit description and consumer dependency summary.
6. **`cli/src/status.rs:2715`** — Fixed pre-existing test (`"missing"` → `"unknown"`) from commit 1a639f2.
