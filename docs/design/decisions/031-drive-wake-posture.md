---
intent: Record braid's mounted-vs-locked drive-wake posture and the TUI pool auto-refresh boundary. Read before adding background disk probes, standby detection, or auto-spindown behavior.
status: Active
---

# Decision: Drive-wake posture

> Principle: [HDD defaults](../principles.md#11-hdd-defaults)

## Context

braid previously treated all pool disks as potentially asleep at any time. The
TUI reflected that blanket anti-wake posture by keeping the expensive pool probe
manual-only: `smartctl -H -A --json` per disk plus btrfs state was run only on
startup or `r`.

That posture was broader than the actual ownership boundary. Today braid does
not park drives with `hdparm -S` or any equivalent per-drive standby timer.
While the pool is mounted, btrfs and normal NAS access already make member
drives active. A future opt-in `braid.autoSpinDown` may park drives, but that
feature belongs to the locked state and will own the "do not wake a parked
locked member" rule.

## Decision

While the pool filesystem is mounted (`PoolStatus::Mounted` in the TUI,
`pool.mounted` in `status`/`doctor`), braid treats member disks as awake and
reads/refreshes them freely. The anti-wake concern applies only to the locked
state and is owned by the future opt-in `braid.autoSpinDown` feature, which will
gate on `braid-online.service`. braid adds no online-side standby detection.

The automatic read boundary is narrow: the TUI pool auto-refresh loop is the
only automatic disk-read loop added here, and it re-arms only while the live pool
is mounted. It does not run while the pool is locked, not mounted, or in an error
state.

The user-invoked read paths keep their existing semantics:

- `braid status` live SMART is mounted-only; it returns the not-mounted status
  before spawning per-disk `smartctl`.
- `braid doctor` SMART self-test diagnostics may target a member's persisted
  `by_id` path even when the pool is unmounted or locked.
- TUI Browse-tab SMART commands are explicit user actions and may target a
  member's persisted `by_id` path regardless of mount state.

"Online" in this decision means the mounted live pool (`PoolStatus::Mounted` or
`pool.mounted`). `braid-online.service` is a correlated systemd lifecycle marker,
not something these read paths consult; it is the handle the future
`braid.autoSpinDown` feature will gate on.

Locked-state TUI probes are not claimed to be non-waking. The pre-existing TUI
startup/manual probe builds LUKS state before the mount gate and reads on-disk
LUKS2 metadata with `cryptsetup luksDump --dump-json-metadata <by-id>` for locked
members. That read can wake a parked drive. This decision leaves that behavior
unchanged and only ensures the new automatic pool loop is mount-gated.

## Non-goals

- No `smartctl -n standby`.
- No `Standby` SMART health state.
- No `braid.autoSpinDown` implementation.
- No `hdparm` integration.
- No NixOS module changes.
- No smartd configuration changes.
- No change to `status`'s mounted-only SMART probe.
- No change to explicit `doctor` or Browse-tab SMART reads.
- No auto-refresh while locked.

## Alternatives considered

### Online-side standby detection

Rejected. Adding `smartctl -n standby` and a `Standby` health state creates
parser and UI state complexity for a state braid does not create while the pool
is mounted. If an operator has configured an out-of-band standby timer, the cost
of being wrong is a single wake-on-read, matching today's explicit reads.

### Blanket anti-wake posture

Rejected. Keeping every pool probe manual-only forces interactive monitoring to
behave like locked-state recovery. The locked state is the only state where
braid-managed drive parking belongs, so the locked-state feature should own that
rule instead of leaking it into mounted-pool UX.

## See

- `cli/src/tui/app.rs#update`
- `cli/src/tui/probe.rs#probe_pool_for_tui`
- `cli/src/status.rs#build_status`
- `cli/src/doctor.rs#check_smart_selftests`
- `cli/src/online_state.rs#OnlineStateOps`
- [ADR 015: HDD defaults](015-hdd-defaults.md)
- [ADR 016: Auto-suspend](016-auto-suspend.md)
- [ADR 030: SMART + btrfs error reporting](030-smart-btrfs-error-reporting.md)
