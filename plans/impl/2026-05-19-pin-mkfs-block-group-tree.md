# Plan: pin `-O block-group-tree` explicitly in `mkfs.btrfs`

## Context

Upstream btrfs-progs 6.19 (released 2026-02-13) flips the
`block-group-tree` feature on by default for `mkfs.btrfs`. From the
upstream CHANGES file, verbatim:

> make block-group-tree default (support since linux 6.1), use -O ^bgt
> to unset

(Upstream uses the short alias `bgt` in the CHANGES wording; the
documented primary name is `block-group-tree`.)

braid currently invokes `mkfs.btrfs` with no `-O` flags
(`cli/src/cmd.rs:678-695`). The on-disk feature set of new pools is
therefore silently nixpkgs-version-dependent: pools created today on
nixos-25.11 (btrfs-progs 6.17.1) come up without `block-group-tree`;
the same code on nixos-26.05 (6.19.x) will create them with it.

`block-group-tree` is a btrfs `compat_ro` (read-only-compatible)
feature flag -- the kernel rejects an unsupported `compat_ro` bit for
read-write mount but may still allow a read-only mount if no log
replay is required. The kernel-side feature has existed since 6.1;
NixOS 25.11 ships kernel 6.12 and 26.05 ships 6.18, so normal braid
read-write operation is always supported. See
`reference/btrfs-progs/common/fsfeatures.c:217` for the canonical
entry; the short alias `bgt` is registered at `:210` as a
`VERSION_ALIAS`.

We pin `-O block-group-tree` explicitly on both `mkfs.btrfs`
invocations so the choice is:

- visible in the dry-run preview and pinned by exact argv tests in
  `cli/src/cmd.rs` (the pending-op journal records structured
  mutation context, not the rendered argv, so dry-run / argv tests
  are the visibility surface),
- independent of which nixpkgs we're on,
- aligned with the upstream direction in 26.05.

We use the long form rather than the `bgt` alias: it is the documented
primary name, matches the kernel sysfs name (`block_group_tree`), and
is the safer pick if anyone ever pins `braid.packages.btrfsProgs` to a
btrfs-progs old enough to predate the short-alias registration.

Existing pools are unaffected. Offline conversion via `btrfstune
--convert-to-block-group-tree` is out of scope -- the operator will do
that manually on the one prod system if/when they want it.

## Plan

### 1. Pin `-O block-group-tree` in `cli/src/cmd.rs`

Modify both `mkfs.btrfs` match arms in `CmdRequest::to_argv()`
(`cli/src/cmd.rs:678-695`) to add `-O block-group-tree` after the
metadata profile and before the device list:

```rust
CmdRequest::MkfsBtrfs { device } => CmdArgs {
    program: "mkfs.btrfs".to_owned(),
    args: vec![
        "-d".into(), "single".into(),
        "-m".into(), "dup".into(),
        "-O".into(), "block-group-tree".into(),
        device.clone(),
    ],
},
CmdRequest::MkfsBtrfsRaid1 { devices } => {
    let mut args = vec![
        "-d".into(), "raid1".into(),
        "-m".into(), "raid1".into(),
        "-O".into(), "block-group-tree".into(),
    ];
    args.extend(devices.iter().cloned());
    CmdArgs { program: "mkfs.btrfs".to_owned(), args }
}
```

`-O` takes a comma-separated feature list, so adding more features
later extends to `-O block-group-tree,foo` cleanly. Passing it as two
distinct argv strings matches the rest of the file's style and keeps
the dry-run preview readable.

### 2. Update unit tests in `cli/src/cmd.rs`

`mkfs_btrfs_raid1_generates_correct_argv` (~`cli/src/cmd.rs:2509`) and
`mkfs_btrfs_single_generates_correct_argv` (~`cli/src/cmd.rs:2537`)
assert exact argv. Insert `"-O", "block-group-tree"` into both
expected vectors at the same position used in step 1 (after the `-m`
profile, before the device path(s)).

### 3. Doc comments on the variants

The `MkfsBtrfs` and `MkfsBtrfsRaid1` variants in `cli/src/cmd.rs:125-130`
currently have no `///` comment. Other variants in the same enum --
e.g. `WipefsBtrfs`, `BtrfsDeviceScanForget`, `BtrfsBalanceResume` --
carry short `///` comments describing the command syntax plus the
constraint/why behind it. Match that style. One short comment per
variant, e.g.:

```rust
/// `mkfs.btrfs -d single -m dup -O block-group-tree <device>` --
/// pin `block-group-tree` explicitly so the on-disk feature set is
/// independent of the nixpkgs btrfs-progs version (default flipped
/// in 6.19). `compat_ro` flag; kernel >=6.1 needed for rw mount,
/// always satisfied for braid hosts.
MkfsBtrfs { device: String },

/// `mkfs.btrfs -d raid1 -m raid1 -O block-group-tree <device>...`
/// -- raid1 form of `MkfsBtrfs`; same `block-group-tree` rationale.
MkfsBtrfsRaid1 { devices: Vec<String> },
```

### 4. New VM test: `tests/module/mkfs-block-group-tree.{nix,py}`

Create a single multi-node test that bootstraps one single-disk pool
and one raid1 pool via `braid add`, then inspects the resulting
superblock(s) for the `BLOCK_GROUP_TREE` flag. The multi-node pattern
is the one used by `tests/module/scrub-lifecycle.py` and the
`ups-lb-during-*.py` tests, so it is idiomatic for braid.

`mkfs-block-group-tree.nix` -- two nodes, modelled on
`tests/module/add-bootstrap.nix`:

- `nodes.single`: one `emptyDiskImages` entry (`disk1`).
- `nodes.raid1`: two `emptyDiskImages` entries (`disk1`, `disk2`).
- Both: `imports = [ ../../modules/braid ]`, `braid.enable = true`,
  `braid.package = braid`, `environment.systemPackages = [
  pkgs.btrfs-progs ]`, `virtualisation.memorySize = 2048`.

`mkfs-block-group-tree.py` -- required preamble (per `docs/testing.md`):

```python
# Test: mkfs-block-group-tree
#
# Intent: Verify braid creates btrfs pools with the
# `block-group-tree` feature bit set on both single-disk and raid1
# layouts.
#
# Why it exists: btrfs-progs 6.19 flips block-group-tree to default.
# braid pins `-O block-group-tree` explicitly so the choice is
# visible and independent of the nixpkgs version. This test guards
# that pin against future nixpkgs bumps that change mkfs defaults.
#
# Scenario: Boot a VM, run `braid add` to bootstrap a fresh pool,
# then inspect `btrfs inspect-internal dump-super` on the underlying
# mapper device(s). Fails if BLOCK_GROUP_TREE is missing from
# compat_ro_flags.
```

Test body (sketch -- exact passphrase-stdin invocation copied from
`add-bootstrap.py`):

```python
start_all()
single.wait_for_unit("multi-user.target", timeout=120)
raid1.wait_for_unit("multi-user.target", timeout=120)

with subtest("single-disk pool has block-group-tree set"):
    single.succeed(
        "echo -n 'testpassphrase' | braid add "
        "disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes"
    )
    single.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk1 "
        "| grep -q BLOCK_GROUP_TREE"
    )

with subtest("raid1 pool has block-group-tree set on both devices"):
    raid1.succeed(
        "echo -n 'testpassphrase' | braid add "
        "disk1=/dev/disk/by-id/virtio-disk1 "
        "disk2=/dev/disk/by-id/virtio-disk2 "
        "--passphrase-stdin --yes"
    )
    raid1.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk1 "
        "| grep -q BLOCK_GROUP_TREE"
    )
    raid1.succeed(
        "btrfs inspect-internal dump-super /dev/mapper/braid-disk2 "
        "| grep -q BLOCK_GROUP_TREE"
    )
```

### 5. Register the VM test in `flake.nix`

Add one entry next to the existing `braid-module-*` entries (pattern
copied from `braid-module-add-bootstrap`):

```nix
braid-module-mkfs-block-group-tree = pkgs.testers.nixosTest (
  import ./tests/module/mkfs-block-group-tree.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

### 6. Decision doc + index update

New short `Active` ADR at `docs/decisions/027-mkfs-block-group-tree.md`
(the next free number after `026-pool-lock-rust-owned.md`). Follow the
front-matter + Status header + Context + Decision pattern of recent
ADRs. Skeleton:

```markdown
---
intent: Record why braid pins `-O block-group-tree` explicitly when
  running `mkfs.btrfs`. Read before changing pool-creation flags or
  bumping btrfs-progs in nixpkgs.
---

# Decision: Pin `block-group-tree` at mkfs time

Status: Active

## Context

btrfs-progs 6.19 (2026-02-13) flips the `block-group-tree` feature
to be on by default in `mkfs.btrfs`. Without an explicit pin, the
on-disk feature set of new pools varies silently across nixpkgs
bumps.

## Decision

`cli/src/cmd.rs` passes `-O block-group-tree` on both `mkfs.btrfs`
invocations (single and raid1). The unit tests in the same file
assert it; the VM test `braid-module-mkfs-block-group-tree` asserts
the resulting on-disk bit. The long form is preferred over the
`bgt` alias because it is the documented primary name and matches
the kernel sysfs entry `block_group_tree`.

## Notes

- `block-group-tree` is a `compat_ro` feature. The kernel rejects
  unsupported `compat_ro` bits for read-write mount but may still
  allow a read-only mount if no log replay is required. The
  kernel-side feature has been available since 6.1; NixOS 25.11
  ships 6.12 and 26.05 ships 6.18, so normal braid rw operation is
  always supported.
- Existing pools created before this pin are unaffected. Offline
  conversion is possible via `btrfstune
  --convert-to-block-group-tree`; braid does not wrap that.
- Forward-compat note: a rescue boot from very old live media
  (kernel <6.1) cannot rw-mount a `block-group-tree` pool. A
  read-only mount may still succeed if no log replay is needed.
  Not a blocker -- braid doesn't ship rescue media -- but visible
  here so we don't forget.
```

Add a corresponding line to the decision listing in
`docs/index.md` (matching the existing `026-` entry's format).

### 7. No-op verification (no edits expected)

The following surfaces were checked and found not to mention `mkfs`
options or feature flags, so the plan should not touch them; reconfirm
during implementation:

- `README.md` -- describes `braid add` at a user-surface level, no
  mention of mkfs flags or kernel mins.
- `docs/principles.md` -- mentions `mkfs.btrfs` only in passing
  (the `-f`-omission rationale).
- `cli/tests/fixtures/` -- no fixture references `block-group-tree`,
  `bgt`, or `compat_ro_flags`. Parsers consume `btrfs filesystem
  usage`/`show`/`df`, none of which surface feature flags.
- No other braid call site invokes `mkfs.btrfs`: every Rust path goes
  through `CmdRequest::MkfsBtrfs{,Raid1}` in `cli/src/cmd.rs`.

## Files modified

- `cli/src/cmd.rs` -- doc comments on two variants, two `to_argv()`
  match arms, two unit tests.
- `tests/module/mkfs-block-group-tree.nix` (new) -- two-node NixOS
  config.
- `tests/module/mkfs-block-group-tree.py` (new) -- multi-node
  testScript.
- `flake.nix` -- one new `braid-module-mkfs-block-group-tree` entry
  under `checks.<system>`.
- `docs/decisions/027-mkfs-block-group-tree.md` (new) -- short
  `Active` ADR.
- `docs/index.md` -- one new listing line for 027.

## Verification

1. `just test-rust` -- both updated unit tests pass; nothing else
   regresses.
2. `just test-vm braid-module-mkfs-block-group-tree` -- new VM test
   passes on nixos-25.11. (`just test-vm` passes names verbatim to
   the flake `checks.<system>` attribute set, so the full
   `braid-module-...` name is required.) Confirms the explicit pin
   produces `block-group-tree` on the current pinned toolchain
   (6.17.1), where it would NOT be the default.
3. `just test-vm braid-module-mkfs-block-group-tree --unstable` --
   new VM test also passes on nixos-unstable. Confirms forward
   compatibility with 6.19+, where `block-group-tree` would be the
   default anyway -- the pin is a no-op but should not error.
4. Manual spot check (optional): in any VM that already runs braid,
   `braid add disk1=... --passphrase-stdin --yes` then `btrfs
   inspect-internal dump-super /dev/mapper/braid-disk1` shows
   `BLOCK_GROUP_TREE` in `compat_ro_flags`.

## Out of scope

- Migrating existing pools to `block-group-tree` -- offline-only via
  `btrfstune`, done manually by the operator if desired.
- A `braid migrate-block-group-tree` (or similar) subcommand. No
  demand.
- Any other `-O` features (e.g. `free-space-tree`, `raid-stripe-tree`,
  `extent-tree-v2`). Each is a separate decision and a separate ADR.

## Resources

- `cli/src/cmd.rs:125-130` -- variant declarations (no doc comments
  yet).
- `cli/src/cmd.rs:678-695` -- `to_argv()` match arms to change.
- `cli/src/cmd.rs:2509-2547` -- unit tests to update.
- `tests/module/add-bootstrap.{nix,py}` -- closest template for the
  new VM test (single-disk bootstrap via `braid add` with
  `--passphrase-stdin`).
- `tests/module/scrub-lifecycle.py` -- template for the multi-node
  testScript pattern.
- `docs/decisions/026-pool-lock-rust-owned.md` -- template for the
  new ADR's header/frontmatter format.
- `docs/testing.md` -- VM-test preamble and `flake.nix`-registration
  conventions.
- `reference/btrfs-progs/common/fsfeatures.c:210-224` -- short alias
  `bgt` at `:210` (a `VERSION_ALIAS`), canonical `block-group-tree`
  entry at `:217` with `compat_ro_flag =
  BTRFS_FEATURE_COMPAT_RO_BLOCK_GROUP_TREE`.
- `reference/btrfs-progs/Documentation/mkfs.btrfs.rst:424-437` --
  upstream feature-flag docs.
- `justfile` -- `test-vm` recipe passes names verbatim to the flake
  `checks.<system>` attribute set; use the full `braid-module-...`
  form when invoking a single test.
- Upstream CHANGES: <https://btrfs.readthedocs.io/en/latest/CHANGES.html>
  (search "6.19").
