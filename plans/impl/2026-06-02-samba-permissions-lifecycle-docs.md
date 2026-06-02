# Fix: clean up Samba permissions and lifecycle examples

## Context

`docs/guides/sharing-and-permissions.md` documents a collaborative Samba share
whose stated goal is that every `storage`-group member can read/write each
other's files. The primary example (the `storage` share) sets two mutually
exclusive permission mechanisms at once:

- the explicit mask/mode group -- `create mask`/`force create mode = 0664`,
  `directory mask`/`force directory mode = 2775` (lines 102-105), and
- `"inherit permissions" = "yes"` (line 108).

Per the Samba manual ("Using Samba" ch.8, corroborated by `smb.conf(5)`):
*"When the inherit permissions option is set to yes, the create mask, directory
mask, force create mode, and force directory mode are ignored."* So with the
current config the four mask/mode lines are **dead config** and the "Key points"
list describes both mechanisms as active, which is self-contradictory.

Concrete inaccuracies this produces:

- Line 120 claims `force create mode`/`force directory mode` "ensure
  group-writable permissions" -- false while `inherit permissions = yes` wins;
  the forced `0664`/`2775` never apply.
- With `inherit permissions = yes`, new files instead copy the share root's
  permission bits. braid sets the mount root to `2770`, so files would land
  `0660`/dirs `2770` -- not the documented `0664`/`2775` (others silently lose
  read). Group-writability happens to survive for braid's `2770` root, but the
  documented modes are wrong and the config is misleading.
- The comment at line 107 ("Inherit group from parent directory (works with
  setgid)") and the bullet at line 121 ("`inherit permissions` respects the
  setgid bit") both attribute **group** inheritance to `inherit permissions`.
  Group inheritance is the kernel setgid bit's job and is independent of this
  Samba setting -- which only copies permission *bits*.

Intended outcome: the example uses one correct, explicit mechanism (setgid for
group, forced modes for permission bits), the prose matches the config, and the
primary share becomes consistent with the sibling `photos` share -- which
already uses the masks-only pattern with no `inherit permissions`.

The same guide also shows how to bind Samba and NFS to braid's pool lifecycle.
The current lifecycle snippet attaches `wantedBy`/`bindsTo`/`after` to
`samba-smbd.service`, but does not gate activation on the pool actually being
mounted. That leaves a boot-start gap for NixOS services with their own default
start edges:

- Samba's `samba.target` has `wantedBy = [ "multi-user.target" ]`, and
  `samba-smbd.service` has `wantedBy = [ "samba.target" ]`.
- NFS's `nfs-server.service` has `wantedBy = [ "multi-user.target" ]`.

The right fix is to complete the existing long-running-consumer pattern with
`unitConfig.ConditionPathIsMountPoint = config.braid.mountPoint`. The triad
still makes the consumer start on unlock and stop before lock; the mount-point
condition prevents any competing boot or direct-start edge from serving an
offline pool directory. This matches ADR 018 and the
`lock-stops-bound-consumers` VM test shape.

## Scope (verified by repo sweep)

- `inherit permissions` appears **only** in `docs/guides/sharing-and-permissions.md`
  (lines 108 and 121). No other docs, `README.md`, `tests/`, or `modules/` copy.
- The `photos` share in the same file (lines 130-139) already uses masks-only,
  correctly -- it is the precedent and needs no change.
- `README.md` has no Samba config block (only metadata links), so no sync risk.
- No test asserts on Samba-produced permission modes (`tests/samba.py` does a
  mount/write/read round-trip and `tests/samba.nix` uses `force user`/`force
  group`, not masks) -- zero test impact.
- Samba is not in the braid module (`modules/braid/auto-suspend.nix` only reads
  `services.samba.enable` to gate suspend) -- AGENTS.md claim confirmed.
- The lifecycle snippet lives in the same guide under "Binding shares to the
  pool lifecycle" (currently lines 152-166). The NFS cross-reference at the end
  of the NFS section also points back to this pattern.
- ADR 018's long-running-consumer contract currently names the triad but omits
  the condition. That is incomplete for consumers with independent boot edges,
  so ADR 018 must be reconciled with the guide.

Files to edit:

- **`docs/guides/sharing-and-permissions.md`**
- **`docs/design/decisions/018-systemd-lifecycle.md`**

## The fix

### Edit 1 -- config block (currently lines 106-108)

Remove the blank line, the comment, and the setting so the `storage` share ends
right after the mask/mode group:

```nix
      "create mask" = "0664";
      "force create mode" = "0664";
      "directory mask" = "2775";
      "force directory mode" = "2775";
    };
```

(Delete: the `# Inherit group from parent directory (works with setgid)` comment
and the `"inherit permissions" = "yes";` line, plus the now-orphaned blank line
above the comment.)

### Edit 2 -- "Key points" (currently line 121)

Replace the false bullet:

```
- `inherit permissions` respects the setgid bit on parent directories.
```

with a bullet that states the actual mechanism (use `--`, ASCII, per repo CLI
output style):

```
- New files and directories inherit the `storage` group from the setgid bit
  braid sets on the mount root -- a kernel behavior that does not require
  `inherit permissions`. `force directory mode = 2775` keeps that setgid bit on
  Samba-created subdirectories so inheritance carries down the tree.
```

Leave line 120 (`force create mode`/`force directory mode` ... group-writable
regardless of umask) untouched -- removing `inherit permissions` makes it true.

### Edit 3 -- lifecycle snippet (currently lines 152-166)

Keep the service-level long-running-consumer triad, but add the missing
mount-point condition. Include comments for every directive line so readers
understand both halves of the contract: lifecycle binding and boot/direct-start
gating.

```nix
systemd.services.samba-smbd = {
  # Start smbd when braid marks the pool online after a successful unlock.
  wantedBy = [ "braid-online.service" ];
  # Stop smbd when braid-online stops, before braid lock unmounts the pool.
  bindsTo = [ "braid-online.service" ];
  # Order smbd on the correct side of braid-online start and stop jobs.
  after = [ "braid-online.service" ];
  # Skip boot or direct starts when the braid mount point is not mounted.
  unitConfig.ConditionPathIsMountPoint = config.braid.mountPoint;
};
```

Update the prose below the snippet:

- Replace "All three fields are load-bearing" with text that explains all four
  fields: `wantedBy` starts Samba when `braid-online.service` starts,
  `bindsTo` stops it when braid-online stops, `after` orders start/stop jobs on
  the correct side of the pool lifecycle, and `ConditionPathIsMountPoint`
  skips Samba when the mount point is only an offline directory.
- Keep the existing `braid lock` / `BoundBy braid-online.service` explanation,
  but update it to include the condition as part of the consumer pattern.
- Mention why the condition matters even with `wantedBy`: NixOS also starts
  Samba through `samba.target` at boot, and the condition prevents that boot
  edge from serving an unmounted pool directory.
- Leave `samba.target`, `nmbd`, and `winbindd` untouched. Only `smbd` serves
  files from the pool and can hold the pool busy during lock.

Update the NFS paragraph after the NFS example so it carries the same complete
pattern:

- Say the same `wantedBy` + `bindsTo` + `after` +
  `ConditionPathIsMountPoint` pattern applies to `nfs-server.service`.
- Note that the condition gates NixOS's default `nfs-server.service`
  boot-start edge against an offline braid mount point.

### Edit 4 -- ADR 018 long-running-consumer contract

Update `docs/design/decisions/018-systemd-lifecycle.md` under "Consumer
dependency contracts" -> "Long-running services holding open files" so the
authoritative contract matches the guide:

- Include `ConditionPathIsMountPoint=<pool mount>` alongside
  `WantedBy=braid-online.service` + `BindsTo=braid-online.service` +
  `After=braid-online.service`.
- State that for consumers with their own boot or direct-start edges, the
  condition is the load-bearing gate that prevents serving an offline mount
  directory. The triad handles unlock-start and lock-stop lifecycle; the
  condition handles starts not initiated by `braid-online.service`.
- Keep the existing explanation that `braid lock` walks
  `BoundBy braid-online.service` and stops consumers before unmount.

## What we are deliberately NOT changing

- **Keep the masks at `0664`/`2775`.** They match the page's umask note
  (line 76: `664`/`775`) and are the standard collaborative-share idiom. The
  other-read bit (`0664`) is harmless: `valid users = @storage` gates the share
  and the `2770` parent blocks other-traversal on disk.
- **Keep `inherit permissions` absent from `photos`** -- already correct.
- **Do not add `inherit permissions` anywhere.** The setgid + forced-modes
  combination is complementary and fully covers the collaborative goal,
  including subdirectories (via the `2` in `force directory mode = 2775`).
- **Do not override `samba.target`.** The condition-on-service pattern is
  already braid's documented/tested consumer shape and avoids changing the boot
  behavior of `nmbd` and `winbindd`.
- **Do not copy the whole Caja Samba config.** The SMB3-only, encrypted-only,
  `vfs_fruit`, and 445-only firewall choices are useful site-specific
  hardening/macOS tuning, but they are not needed for this simple braid example.

## Verification

Docs-only change; no Rust or VM tests exercise this prose.

1. `mdbook build docs` -- must pass. Cross-links inside `docs/` are validated by
   `mdbook-linkcheck2` (see `docs/book.toml`); the edit adds no links, but this
   is the standard gate per AGENTS.md.
2. Re-read the "Samba integration" section end to end: the `storage` share now
   carries only the mask/mode group, the "Key points" no longer mention
   `inherit permissions`, and the group-inheritance story is attributed to
   setgid. Confirm it reads consistently with the `photos` share below it.
3. Re-read the lifecycle subsection end to end: it now documents the four-line
   service pattern, comments every directive line in the Samba snippet, and
   keeps the `BoundBy braid-online.service` story accurate.
4. Re-read the NFS section end to end: the cross-reference now includes
   `ConditionPathIsMountPoint` and no longer points NFS users at an incomplete
   three-field pattern.
5. Re-read ADR 018's "Long-running services holding open files" paragraph:
   it includes the condition, explains its load-bearing role for consumers
   with independent boot/direct-start edges, and still matches the
   `lock-stops-bound-consumers` test shape.
6. (Authority check, no command) The behavior claim is grounded in the Samba
   manual entry for `inherit permissions`; no code path to run.
7. (Authority check, no command) The lifecycle claim is grounded in the current
   NixOS modules: `samba.target` is wanted by `multi-user.target`,
   `samba-smbd.service` is wanted by and part of `samba.target`, and
   `nfs-server.service` is wanted by `multi-user.target`.
