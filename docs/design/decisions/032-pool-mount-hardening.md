---
intent: Record why braid mounts the shared data pool with nosuid,nodev, why the
  flags are rendered last, and why noexec remains excluded.
status: Active
---

# Decision: Pool mount hardening (nosuid/nodev)

> Principle: [Sane defaults](../principles.md#7-sane-defaults)

## Context

The data pool is shared NAS storage. It does not need setuid binaries or device
special files to function, but a pool that honors those features can turn stored
payloads into local privilege-escalation material.

A plain unprivileged `poolAccessGroup` member cannot make the exploitable
payloads by themselves: planting a setuid-root binary requires `CAP_CHOWN`, and
creating a device node requires `CAP_MKNOD`. The real principals that can land
dangerous metadata on the pool are narrower:

- NFS `no_root_squash` remote-root. A client root mapped to server root can write
  a setuid-root binary or device node onto the pool; a local NAS user can later
  execute or open it.
- Root-run mode-preserving ingestion. A restore, `tar -p`, or `rsync -a` run as
  root can land setuid-root or device-node payloads from another system onto the
  pool.

Mounting the pool `nosuid,nodev` closes the setuid-binary and device-node
escalation facet for both principals. It does not close NFS `no_root_squash`'s
separate, broader remote-root-authority facet: remote client-root still has full
read/write/chown/delete authority over pool files. The sharing guide uses
`root_squash` by default for that reason.

On the next `unlock` after upgrading, any setuid bits or device nodes already on
the pool, such as from a restored system backup or stored container/chroot image,
become inert. That is intended hardening, not a migration step.

## Decision

braid mounts the data pool with `nosuid,nodev` unconditionally. The flags are
rendered last by `cli/src/cmd.rs#mount_options`, which appends
`cli/src/cmd.rs#enforced_mount_options` after the base list and after any caller
extra such as `degraded`.

That ordering is load-bearing. `mount(8)` applies the last conflicting flag as
the winner, so caller-supplied `suid` or `dev` cannot override the hardening
flags. Both `CmdRequest::Mount` and `CmdRequest::MountWithOptions` route through
the same assembler so no mount path can accidentally omit or reorder them.

## noexec excluded

`noexec` is deliberately excluded. A NAS legitimately stores and may run
executables, scripts, build outputs, or application data from the shared pool.
`nosuid,nodev` provide the privilege-containment property braid needs with lower
functional cost.

This is revisitable as a separate pool-exec-containment decision if a concrete
use case appears.

## See

- `cli/src/cmd.rs#mount_options`
- `cli/src/cmd.rs#enforced_mount_options`
- `tests/cli/braid-unlock.py`
- [ADR 013: Mount permissions](013-mount-permissions.md)
- [ADR 015: HDD defaults](015-hdd-defaults.md)
- [ADR 028: Immutable unmounted mountpoint](028-immutable-unmounted-mountpoint.md)
- [Sharing and permissions](../../guides/sharing-and-permissions.md)
