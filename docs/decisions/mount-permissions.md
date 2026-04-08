# Decision: Group-based mount point permissions

Status: Active

## Context

When braid mounts the pool at `/mnt/storage`, the btrfs root is `root:root 0755`. Regular users can't write — blocking rsync, cp, and Samba workflows. NAS users need group-level access to the mount root without running everything as root.

## Decision

The NixOS module declares a `storage` group and reconciles mount point permissions (`root:storage 2770`) after every braid command that results in a mounted pool. This is implemented as a wrapper around the braid CLI binary.

### Why group-based (not ACLs, not per-user subvolumes)

- **Group + setgid** is the simplest model that covers the NAS use case: a set of trusted users who all need read/write access to the same pool.
- ACLs add complexity and tooling requirements (getfacl/setfacl) without benefit for the typical home NAS.
- Per-user subvolumes solve a different problem (isolation), not shared access.
- This matches the pattern used by TrueNAS and OpenMediaVault.

### Why the NixOS module handles it (not the Rust CLI)

Mount point permissions are OS-level access policy. The Rust CLI manages LUKS + btrfs — it shouldn't know about Unix groups. Keeping the permission fixup in the wrapper means the CLI stays focused on storage operations, and the NixOS module controls access policy.

### Why wrapper-based fixup

The NixOS module already wraps the `braid` CLI binary to inject tool packages onto `PATH`. The wrapper is extended to apply `chown root:<group>` + `chmod 2770` on the mount point after successful mount-producing commands (`unlock`, `add`, `recover`).

**Properties:**
- **Explicit** — runs synchronously after the braid binary exits, before control returns to the caller
- **Covers all mount paths** — `braid unlock` (direct CLI, systemd service, auto-unlock), `braid add` (bootstrap), and `braid recover` (recovery) all go through the wrapper
- **No async race** — unlike a systemd ExecStartPost or path watch, the fixup completes before the caller sees success
- **Idempotent** — permissions persist in btrfs metadata; re-running is a no-op
- **Failure-tolerant** — warns to stderr if chown/chmod fails; never overrides the wrapped command's exit code

### Why `storage` as default group name

- Standard NAS convention (TrueNAS, OMV use similar names)
- No collision with existing NixOS system groups
- Configurable via `braid.storageGroup`; set to `null` to disable entirely

## Scope

This sets ownership and mode on the **mount root directory only**. It does NOT:
- Override per-file permissions (files created with restrictive umask remain restrictive)
- Provide a complete multi-user collaboration model
- Manage ACLs or sub-directory policies

The setgid bit (2770) ensures new files/directories in the mount root inherit the `storage` group, but the owning user's umask still controls the group-write bit on individual files.

## See

- `modules/braid/braid-wrapper.sh` — the wrapper template
- `modules/braid/wrapper.nix` — Nix derivation that builds the wrapper
- `modules/braid/options.nix` — `storageGroup` option definition
- [Sane defaults](sane-defaults.md) — philosophy on opinionated defaults
