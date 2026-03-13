# Plan: Group-based mount point permissions

## Context

When braid mounts the pool at `/mnt/storage`, the btrfs root is `root:root 0755`. Regular users can't write — blocking rsync, cp, and Samba workflows. Fix: NixOS module declares a `storage` group and reconciles mount point permissions (`root:storage 2770`) after every braid command that results in a mounted pool.

## Mechanism: wrapper-based permission fixup

The NixOS module already wraps the `braid` CLI binary (via `makeWrapper`) to inject tool packages onto `PATH`. Replace the `makeWrapper` call with a shell script wrapper that:

1. Calls the real `braid` binary with all arguments
2. Captures the exit code
3. Only for mount-producing commands (`unlock`, `add`) that succeeded: if the mount point is mounted, runs `chown root:<group>` + `chmod 2770`
4. Warns to stderr if permission fixup fails — does NOT override the wrapped command's exit code
5. Always exits with the original exit code

```bash
${cfg.package}/bin/braid "$@"
ret=$?
if [ "$ret" -eq 0 ]; then
  # Find the subcommand: first positional arg after skipping global options.
  # Only global option with a value is --config <path> (cli/src/main.rs:15).
  subcmd=""
  skip_next=false
  for arg in "$@"; do
    if $skip_next; then
      skip_next=false
      continue
    fi
    case "$arg" in
      --config) skip_next=true ;;
      --config=*) ;;
      -*) ;;
      *) subcmd="$arg"; break ;;
    esac
  done

  # Skip fixup for non-mounting invocations (help, dry-run, version).
  skip_fixup=false
  for arg in "$@"; do
    case "$arg" in
      --help|-h|--version|-V|--dry-run) skip_fixup=true; break ;;
    esac
  done

  if ! $skip_fixup; then
    case "$subcmd" in
      unlock|add)
        if mountpoint -q "${cfg.mountPoint}" 2>/dev/null; then
          if ! chown "root:${cfg.storageGroup}" "${cfg.mountPoint}"; then
            echo "braid: WARNING: failed to set ownership on ${cfg.mountPoint}" >&2
          fi
          if ! chmod 2770 "${cfg.mountPoint}"; then
            echo "braid: WARNING: failed to set permissions on ${cfg.mountPoint}" >&2
          fi
        fi
        ;;
    esac
  fi
fi
exit $ret
```

Subcommand detection parses the first non-global-option token. Non-mounting flags (`--dry-run`, `--help`, `--version`) skip the fixup entirely. Permission fixup failures warn to stderr but never change the exit code — the mount is the primary operation.

This covers all braid-managed mount paths:
- `braid unlock` (direct CLI or via systemd service)
- `braid-auto-unlock` (USB keyfile service calls `braid unlock`)
- `braid add` (bootstrap — creates pool and mounts)

**Why this works**: Every path that mounts the pool goes through the `braid` binary with either `unlock` or `add` as the subcommand. The wrapper runs synchronously after the binary exits, before control returns to the caller. No systemd inference, no async race.

**Properties**:
- Only runs for `unlock` and `add` — detected by parsing the first non-global-option token (handles `braid --config ... unlock`; correctly ignores `braid remove add`)
- Skips `--dry-run`, `--help`, `--version` — these exit 0 without mounting (clap renders the Rust `dry_run` field as `--dry-run`; no underscore variant exists)
- Only runs on success (`$ret -eq 0`) — failed mounts don't trigger fixup
- Warns to stderr if chown/chmod fails — never overrides the wrapped command's exit code (the mount is the primary operation; permissions are secondary)
- Idempotent — permissions persist in btrfs metadata; re-running is a no-op
- No-op when pool is not mounted (`mountpoint -q` fails, fixup skipped)

## Scope: mount-root access policy only

This sets ownership and mode on the mount root directory. It does NOT:
- Override per-file permissions (files created with restrictive umask remain restrictive)
- Provide a complete multi-user collaboration model
- Manage ACLs or sub-directory policies

The setgid bit (2770) ensures new files/directories in the mount root inherit the `storage` group, but the owning user's umask still controls the group-write bit on individual files.

## Changes (in implementation order)

### 1. Write failing tests first

#### 1a. New test: bootstrap permissions (`tests/module/add-bootstrap.nix` + `.py`)

```
Intent: mount point has correct group ownership after braid add bootstrap
Why: braid add mounts from the Rust CLI, not through a systemd service.
     The wrapper-based fixup must cover this path.
Scenario: first-time user runs braid add disk1 — pool created, mounted,
          wrapper sets root:storage 2770 before returning.
```

**`tests/module/add-bootstrap.nix`**: Single raw disk (no initrd fixture), braid module imported. Same VM compat pattern as `single-disk.nix` and `raid1.nix`.

**`tests/module/add-bootstrap.py`**:
```python
with subtest("Bootstrap: braid add creates pool with correct permissions"):
    machine.succeed("printf ... | braid add disk1 --passphrase-stdin --yes")
    machine.succeed("mountpoint -q /mnt/storage")
    stat = machine.succeed("stat -c '%U:%G %a' /mnt/storage").strip()
    assert stat == "root:storage 2770", f"Expected root:storage 2770, got {stat}"
```

**`flake.nix`**: Register test as `braid-module-add-bootstrap` (same pattern: `import ... { braid = linuxCrane.braid-cli-unwrapped; }`).

#### 1b. Add permission check to existing unlock test (`tests/module/raid1.py`)

After the existing `braid unlock` subtest:
```python
with subtest("Mount point has correct group permissions"):
    stat = machine.succeed("stat -c '%U:%G %a' /mnt/storage").strip()
    assert stat == "root:storage 2770", f"Expected root:storage 2770, got {stat}"
```

#### 1c. Extend existing auto-unlock test (`tests/module/auto-unlock-key-present.py`)

After the existing success assertion, add:
```python
with subtest("Mount point has correct group permissions after auto-unlock"):
    stat = machine.succeed("stat -c '%U:%G %a' /mnt/storage").strip()
    assert stat == "root:storage 2770", f"Expected root:storage 2770, got {stat}"
```

This covers both properties in one test: the service reports success (existing assertion) AND the wrapper sets correct permissions. No new test file needed — the existing `auto-unlock-key-present` test is the most operationally important path (unattended systemd on USB keyfile insertion).

### 2. NixOS module changes

#### 2a. `modules/braid/options.nix` — add option + group

```nix
storageGroup = lib.mkOption {
  type = lib.types.nullOr lib.types.str;
  default = "storage";
  description = "Group for mount point access. Sets root:<group> 2770 on the mount root after mount-producing commands (unlock, add). Set to null to disable.";
};
```

Append to the existing `config.assertions` list inside `lib.mkIf cfg.enable` (options.nix:72):
```nix
{
  assertion = cfg.storageGroup == null || builtins.match "[a-z_][a-z0-9_-]*" cfg.storageGroup != null;
  message = "braid.storageGroup '${toString cfg.storageGroup}' is not a valid Unix group name.";
}
```

Group declaration in `config` block:
```nix
users.groups = lib.mkIf (cfg.storageGroup != null) {
  ${cfg.storageGroup} = {};
};
```

#### 2b. New: `modules/braid/braid-wrapper.sh` — shell template

Checked-in shell script with `@placeholder@` substitution markers. Nix replaces these at build time via `substitute --subst-var-by`. The script is always the same structure; the `storageGroup` guard is a runtime `[ -n "..." ]` check (empty string when `cfg.storageGroup == null`).

```bash
#!@shell@
export PATH="@toolPath@:$PATH"
@braidBin@ "$@"
ret=$?

if [ -n "@storageGroup@" ] && [ "$ret" -eq 0 ]; then
  # Subcommand detection mirrors the global CLI shape in cli/src/main.rs (struct Cli).
  # If global options change there, update this parser to match.
  subcmd=""
  skip_next=false
  for arg in "$@"; do
    if $skip_next; then
      skip_next=false
      continue
    fi
    case "$arg" in
      --config) skip_next=true ;;
      --config=*) ;;
      -*) ;;
      *) subcmd="$arg"; break ;;
    esac
  done

  skip_fixup=false
  for arg in "$@"; do
    case "$arg" in
      --help|-h|--version|-V|--dry-run) skip_fixup=true; break ;;
    esac
  done

  if ! $skip_fixup; then
    case "$subcmd" in
      unlock|add)
        if @mountpointBin@ -q "@mountPointPath@" 2>/dev/null; then
          if ! @chownBin@ "root:@storageGroup@" "@mountPointPath@"; then
            echo "braid: WARNING: failed to set ownership on @mountPointPath@" >&2
          fi
          if ! @chmodBin@ 2770 "@mountPointPath@"; then
            echo "braid: WARNING: failed to set permissions on @mountPointPath@" >&2
          fi
        fi
        ;;
    esac
  fi
fi
exit $ret
```

Placeholders: `@shell@`, `@braidBin@`, `@toolPath@`, `@storageGroup@`, `@mountpointBin@`, `@chownBin@`, `@chmodBin@`, `@mountPointPath@`.

Full store paths for `mountpoint`, `chown`, `chmod` — no PATH dependency for fixup commands. Permission fixup failures warn to stderr but never change the exit code.

#### 2c. New: `modules/braid/wrapper.nix` — shared wrapper package

Reads the shell template, substitutes Nix values, produces a derivation. Imported by both `storage.nix` and `cli.nix`.

```nix
# modules/braid/wrapper.nix
{ cfg, pkgs, lib }:
let
  toolPackages = with cfg.packages; [ cryptsetup btrfsProgs utilLinux jq coreutils ];
in
pkgs.runCommand "braid" {} ''
  mkdir -p $out/bin
  substitute ${./braid-wrapper.sh} $out/bin/braid \
    --subst-var-by shell '${pkgs.runtimeShell}' \
    --subst-var-by braidBin '${cfg.package}/bin/braid' \
    --subst-var-by toolPath '${lib.makeBinPath toolPackages}' \
    --subst-var-by storageGroup '${if cfg.storageGroup != null then cfg.storageGroup else ""}' \
    --subst-var-by mountpointBin '${cfg.packages.utilLinux}/bin/mountpoint' \
    --subst-var-by chownBin '${cfg.packages.coreutils}/bin/chown' \
    --subst-var-by chmodBin '${cfg.packages.coreutils}/bin/chmod' \
    --subst-var-by mountPointPath '${cfg.mountPoint}'
  chmod +x $out/bin/braid
''
```

`substitute` is a shell function from stdenv's `setup.sh`, available in `runCommand`. Each `--subst-var-by name value` replaces every `@name@` in the source. Explicit substitution (not `substituteAll`) avoids accidental replacement of other `@...@` patterns.

#### 2d. `modules/braid/storage.nix` — use shared wrapper

Replace the `makeWrapper`-based `braidWrapped` definition (lines 15-25) with:
```nix
braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; };
```

#### 2e. `modules/braid/cli.nix` — use shared wrapper

Replace the `makeWrapper`-based `braid` definition (lines 6-12) with:
```nix
braid = import ./wrapper.nix { inherit cfg pkgs lib; };
```

Both modules now share one wrapper definition. Future changes to the command set, mode, or fixup behavior stay in sync.

### 3. Update samba test

**`tests/samba.nix`**:
- Add `users.groups.storage = {};`
- Add `extraGroups = [ "storage" ];` to `nas` user
- Add `"force group" = "storage";` to Samba share config

**`tests/samba.py`** — replace line 23 (`chown nas /mnt/storage`) with:
```python
server.succeed("chown root:storage /mnt/storage")
server.succeed("chmod 2770 /mnt/storage")
```

### 4. Update docs

#### 4a. `docs/decisions/sane-defaults.md` — add to defaults table

| Setting | Value | Rationale |
|---------|-------|-----------|
| `braid.storageGroup` | `"storage"` | Mount root set to `root:storage 2770`. Users in the group can read/write the mount root. Setgid ensures new entries inherit the group. Same pattern as TrueNAS/OMV. Does not override per-file umask. |

#### 4b. New: `docs/decisions/mount-permissions.md`

Status: Active. Documents:
- Why group-based mount-root permissions (not ACLs, not per-user subvolumes)
- Why NixOS module handles it (not Rust CLI) — access policy is OS-level
- Why wrapper-based fixup (explicit, synchronous, covers all mount paths)
- Scope: mount-root only, not a file-level collaboration model
- Why `storage` as default group name (NAS convention, no NixOS collision)

#### 4c. `docs/principles.md` — extend principle 7

Mention `storageGroup` as a sane default alongside autoScrub.

### 5. Update README

```markdown
### Mount Point Permissions

braid sets the mount root to `root:storage 2770` after mount-producing commands
(`unlock`, `add`). Users in the `storage` group can read and write the mount root directory.
New entries inherit the `storage` group via setgid.

Note: individual file permissions still depend on the creating process's umask.
For collaborative access, ensure users set `umask 002` or configure Samba with
`force create mode` / `force directory mode`.

Add a user to the storage group:
  users.users.myuser.extraGroups = [ config.braid.storageGroup ];

Customize the group name:
  braid.storageGroup = "nas-users";

Disable automatic permissions:
  braid.storageGroup = null;
```

## Files to modify

| File | Change |
|------|--------|
| `modules/braid/options.nix` | `storageGroup` option + `users.groups` |
| `modules/braid/braid-wrapper.sh` | **New** — shell template with `@placeholder@` substitution markers |
| `modules/braid/wrapper.nix` | **New** — reads template, substitutes Nix values, produces wrapper package |
| `modules/braid/storage.nix` | Replace `braidWrapped` with `import ./wrapper.nix` |
| `modules/braid/cli.nix` | Replace `braid` with `import ./wrapper.nix` |
| `tests/module/add-bootstrap.nix` | **New** — bootstrap permissions test |
| `tests/module/add-bootstrap.py` | **New** — bootstrap test script |
| `tests/module/raid1.py` | Add permission check subtest |
| `tests/module/auto-unlock-key-present.py` | Add permission check subtest |
| `tests/samba.nix` | Group + Samba config |
| `tests/samba.py` | Group-based chown/chmod |
| `flake.nix` | Register `braid-module-add-bootstrap` test |
| `docs/decisions/sane-defaults.md` | Add storageGroup to defaults table |
| `docs/decisions/mount-permissions.md` | **New** — decision doc |
| `docs/principles.md` | Extend principle 7 |
| `README.md` | Document storage group |

## What does NOT change

- No Rust code (cli/src/*)
- No config.json schema
- No CLI tests (tests/cli/* — these don't use the braid module)

## Verification

1. `just test braid-module-add-bootstrap` — permissions after bootstrap mount (TDD: write first, fails, then implement)
2. `just test braid-module-raid1` — permissions after unlock
3. `just test braid-module-auto-unlock-key-present` — service success + permissions after auto-unlock
4. `just test samba` — Samba with group permissions
5. `just test braid-module-single-disk` — existing test still passes
