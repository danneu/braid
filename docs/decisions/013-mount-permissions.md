# Decision: Group-based mount point permissions

Status: Active

## Context

When braid mounts the pool at `/mnt/storage`, the btrfs root is `root:root 0755`. Regular users can't write — blocking rsync, cp, and Samba workflows. NAS users need group-level access to the mount root without running everything as root.

## Decision

The NixOS module declares a `storage` group and emits the Unix group name in runtime config as `pool_access_group`. Rust dispatch reconciles mount point permissions (`root:<group> 2770`) from `mark_online` after every braid command that results in a mounted pool.

### Why group-based (not ACLs, not per-user subvolumes)

- **Group + setgid** is the simplest model that covers the NAS use case: a set of trusted users who all need read/write access to the same pool.
- ACLs add complexity and tooling requirements (getfacl/setfacl) without benefit for the typical home NAS.
- Per-user subvolumes solve a different problem (isolation), not shared access.
- This matches the pattern used by TrueNAS and OpenMediaVault.

### Why module config drives it

Mount point permissions are OS-level access policy, so the NixOS module owns the group name and whether the fixup is configured. Rust dispatch executes the fixup because it already holds `/run/braid-pool.lock` through post-mount lifecycle work, which keeps permissions synchronized with the same mounted-pool state that drives `braid-online.service`.

### Why Rust dispatch executes the fixup

The shell wrapper is a pure exec shim that injects tool packages onto `PATH`. `mark_online` (`cli/src/online_state.rs`) applies `chown root:<group>` + `chmod 2770` on the mount point after successful mount-producing commands (`unlock`, `add`, `recover`).

**Properties:**
- **Explicit** -- runs synchronously after the mounted-pool command succeeds, before control returns to the caller
- **Covers all mount paths** -- `braid unlock` (direct CLI, systemd service, auto-unlock), `braid add` (bootstrap), and `braid recover` (recovery) all go through Rust dispatch
- **No async race** -- unlike a systemd ExecStartPost or path watch, the fixup completes before the caller sees success
- **Idempotent** -- permissions persist in btrfs metadata; re-running is a no-op
- **Failure-tolerant** -- warns to stderr if chown/chmod fails; never overrides the wrapped command's exit code

### Why `storage` as default group name

- Standard NAS convention (TrueNAS, OMV use similar names)
- No collision with existing NixOS system groups
- Configurable via `braid.poolAccessGroup`; set to `null` to disable entirely

## Scope

This sets ownership and mode on the **mount root directory only**. It does NOT:
- Override per-file permissions (files created with restrictive umask remain restrictive)
- Provide a complete multi-user collaboration model
- Manage ACLs or sub-directory policies

The setgid bit (2770) ensures new files/directories in the mount root inherit the `storage` group, but the owning user's umask still controls the group-write bit on individual files.

## See

- `cli/src/online_state.rs` -- `mark_online` permission fixup
- `modules/braid/options.nix` -- `poolAccessGroup` option definition
- [Sane defaults](005-sane-defaults.md) -- philosophy on opinionated defaults
