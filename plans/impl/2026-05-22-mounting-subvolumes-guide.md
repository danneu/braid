# Plan: "Mounting subvolumes" guide

## Context

braid mounts the btrfs pool's top-level (subvolid=5) at `/mnt/storage` and
documents subvolume *creation* in `day-to-day-nas-usage.md`, but nothing in
the manual covers the idiomatic next step: mounting a specific subvolume at
an arbitrary path so a consumer (a user's home directory, a service like
Jellyfin or Plex) sees it as a self-contained filesystem without needing
access to the rest of the pool.

This is a real UX gap, surfaced by walking through a "let Jellyfin read
movies from ~/my-movies" scenario:

- Users naturally reach for `mount -o subvol=movies` -- the standard btrfs
  idiom -- and the docs don't show how it fits with braid.
- The bind-mount alternative is less idiomatic and requires the top-level
  mount to be traversable by the consumer, which conflicts with the
  least-privilege story.
- braid is intentionally hands-off here (no generated `fileSystems`
  entries per ADR 003), so the user owns the wiring -- but they need
  a recipe to copy.

### Why `systemd.mounts`, not `fileSystems`

The recipe must be lifecycle-safe with `braid lock`. Per
[ADR 018 § "On `lock`" step 3](../../docs/decisions/018-systemd-lifecycle.md),
`braid lock` iterates `systemctl show -P BoundBy braid-online.service`
and stops each bound consumer **before** unmounting the pool. Only
units with `BindsTo=braid-online.service` appear in that list.

The `fileSystems` route is therefore wrong: `x-systemd.requires=X`
emits `Requires=X`+`After=X` (per `systemd-fstab-generator(8)`), but
there is **no** `x-systemd.binds-to=` fstab option, so a subvolume
mount declared via `fileSystems` cannot participate in the `BoundBy`
cascade. `braid lock` would skip the subvolume mount, the btrfs
superblock would stay referenced through the subvolume mount, and the
LUKS close step would either block or fail. Native NixOS
`systemd.mounts` is the only declarative path that can set `BindsTo=`,
so the guide is written around that primitive.

The intended outcome is a new guide `manual/guides/mounting-subvolumes.md`
that:

1. Explains the `subvol=` mount idiom and why it isolates the mounted
   subvolume from the rest of the pool (parent invisible, like a bind
   mount but cleaner).
2. Provides a copy-paste NixOS `systemd.mounts` recipe wired into the
   `WantedBy=`+`BindsTo=`+`After=braid-online.service` triad -- the
   same shape the project already uses for samba/nfs/scrub consumers.
3. Contrasts `subvol=` vs bind mount briefly so users understand the
   choice.
4. Closes with a worked Jellyfin example combining the subvol mount,
   read-only access via POSIX ACLs scoped strictly to
   `/mnt/storage/movies` (no `setfacl` on `/mnt/storage` itself, since
   `subvol=` mounts do not require consumer traversal of the
   management mount), and a `WantedBy=`+`BindsTo=`+`After=` chain on
   the *mount unit* (not on `braid-online.service` directly) so the
   service can also `ConditionPathIsMountPoint` its own bind target.

Existing guides keep their scope; `day-to-day-nas-usage.md` and
`sharing-and-permissions.md` get short pointers to the new guide. The
contract the recipe makes -- "this declaration participates in
`braid lock`'s `BoundBy` cascade" -- is pinned by a new NixOS VM test
so future refactors can't silently regress the documented shape.

## Critical files to modify

1. **New file: `manual/guides/mounting-subvolumes.md`**
   - H1: `Mounting subvolumes` (matches kebab-case-file / title-case-H1 convention).
   - Section outline:
     - `[← Manual](../index.md)` back-link (matches other guides).
     - Brief intro: why you might want a subvolume at a non-`/mnt/storage` path.
     - **"How braid mounts the pool"** -- one-paragraph recap that `/mnt/storage` is the management mount (subvolid=5), and consumer-facing services don't need to touch it.
     - **"The `subvol=` mount idiom"** -- explain the kernel-level isolation (parent invisible). Quote the btrfs docs line: *"the parent directory is not visible and accessible, which is similar to a bind mount."*
     - **"Recipe: mount a subvolume at a custom path"** (admin/home-dir variant -- `~/my-movies`). Step-by-step:
       1. `sudo btrfs subvolume create /mnt/storage/movies` (with pool unlocked).
       2. Find the btrfs filesystem UUID: `sudo btrfs filesystem show /mnt/storage` (the `uuid:` line). Note: `braid status` does not currently surface this -- see "Future enhancement" below.
       3. NixOS `systemd.mounts` entry:
          ```nix
          systemd.mounts = [{
            what = "/dev/disk/by-uuid/<btrfs-fs-uuid>";
            where = "/home/dan/my-movies";
            type = "btrfs";
            options = "subvol=movies,ro,noatime";
            wantedBy = [ "braid-online.service" ];
            bindsTo  = [ "braid-online.service" ];
            after    = [ "braid-online.service" ];
          }];
          ```
       4. Explain each field in 1 line. Particularly:
          - `bindsTo = [ "braid-online.service" ]` is the load-bearing bit. It puts the mount unit into `BoundBy braid-online.service`, which is what `braid lock` iterates (per ADR 018) to stop consumers before unmount. Without `bindsTo`, the mount stays active during `braid lock` and blocks the LUKS close step.
          - `wantedBy` brings the mount up when `braid-online.service` activates after `braid unlock`.
          - `after` orders the mount start *after* `braid-online.service` is up so the btrfs `by-uuid` symlink exists by the time systemd resolves `what =`.
          - `ro` is optional but recommended when the path is for read-only consumption.
       5. Rebuild and verify with `findmnt /home/dan/my-movies` and `systemctl show -P BoundBy braid-online.service` (the escaped mount unit name -- e.g. `home-dan-my\x2dmovies.mount` -- should appear in the list).
     - **"`subvol=` vs bind mount"** -- short comparison: functionally equivalent at the kernel level per btrfs docs; `subvol=` is conventional, does not depend on `/mnt/storage` being traversable by the consumer, and is the right default. Bind mounts only when you need to expose the same data at multiple paths within an already-traversable mount.
     - **"Why not `fileSystems` with `x-systemd.requires`?"** -- one short paragraph explaining the `BindsTo` gap (cross-link ADR 018) so a curious user does not "fix" the recipe by porting it back to fstab options.
     - **"Worked example: read-only access for Jellyfin"** (service variant -- `/var/lib/jellyfin/media`, never `~/my-movies`):
       - Step 1: create `/mnt/storage/movies` subvolume (same as generic recipe).
       - Step 2: declare the `systemd.mounts` entry pointing at `/var/lib/jellyfin/media`:
         ```nix
         systemd.mounts = [{
           what = "/dev/disk/by-uuid/<btrfs-fs-uuid>";
           where = "/var/lib/jellyfin/media";
           type = "btrfs";
           options = "subvol=movies,ro,noatime";
           wantedBy = [ "braid-online.service" ];
           bindsTo  = [ "braid-online.service" ];
           after    = [ "braid-online.service" ];
         }];
         ```
       - Step 3: ACL scoping. Grant jellyfin read-execute **only** on the subvolume contents -- do NOT touch `/mnt/storage` itself, because `subvol=` mounts do not require the consumer to traverse the management mount:
         ```sh
         sudo setfacl -R    -m u:jellyfin:rx /mnt/storage/movies
         sudo setfacl -R -d -m u:jellyfin:rx /mnt/storage/movies
         ```
         Why ACL (not `storage` group membership): adding a network-facing daemon to `storage` would grant it read+write across the entire pool, violating least privilege. ACLs scope access to a single subtree, read-only.
       - Step 4: NixOS service config binding to the **mount unit**, not `braid-online.service` directly:
         ```nix
         services.jellyfin = { enable = true; openFirewall = true; };
         systemd.services.jellyfin = {
           wantedBy = lib.mkForce [ "var-lib-jellyfin-media.mount" ];
           bindsTo  = [ "var-lib-jellyfin-media.mount" ];
           after    = [ "var-lib-jellyfin-media.mount" ];
           unitConfig.ConditionPathIsMountPoint = "/var/lib/jellyfin/media";
         };
         ```
         Why bind to the mount, not to `braid-online.service`:
         - Ensures jellyfin only starts once `/var/lib/jellyfin/media` is actually mounted (not just after `braid-online.service` is active).
         - Reverse: when `braid lock` triggers the cascade, the mount unit's `BindsTo` propagates to jellyfin -- jellyfin stops first (because of `after`), then the mount unmounts, then `braid lock` proceeds to unmount the pool.
         - `ConditionPathIsMountPoint = "/var/lib/jellyfin/media"` (not `config.braid.mountPoint`) gates on the *bind target*, not the management mount, matching the consumer's actual dependency.
         The full triad-pattern rationale lives in `sharing-and-permissions.md`'s "Binding shares to the pool lifecycle" section; cross-link to it instead of re-deriving the explanation.
       - Step 5: verification (`sudo -u jellyfin ls /var/lib/jellyfin/media`, point Jellyfin web UI at the path, run `braid lock` and confirm both jellyfin and the mount unit stop before LUKS close completes).
     - **"Future enhancement"** (one-sentence note) -- `braid status` already tracks the btrfs FSID internally (`PoolState.fsid` in `cli/src/types.rs:423`) but does not render it. Surfacing it in `braid status` and the JSON output would remove the `btrfs filesystem show` lookup. Tracked separately.
     - **"What's next"** -- bullet list (relative `*.md` links):
       - `[Sharing and permissions](sharing-and-permissions.md)`
       - `[Day-to-day NAS usage](day-to-day-nas-usage.md)`
     - **"Related commands"** -- `[unlock](../commands/unlock.md)`, `[status](../commands/status.md)`.

2. **`manual/SUMMARY.md`** -- add one line under `# Guides`:
   ```markdown
   - [Mounting subvolumes](guides/mounting-subvolumes.md)
   ```
   Place alphabetically or right after `[Sharing and permissions]` (whichever matches the existing ordering -- read the file first to confirm).

3. **`manual/index.md`** -- add a row to the Guides table:
   ```markdown
   | [Mounting subvolumes](guides/mounting-subvolumes.md)         | Expose a btrfs subvolume at a custom path (Jellyfin example) |
   ```
   Slot it next to `[Sharing and permissions]` for topic adjacency.

4. **`manual/guides/day-to-day-nas-usage.md`** -- in the existing "Organizing data with subvolumes" section, add one sentence at the end:
   > To mount a specific subvolume at a custom path (e.g. for a service or a friendlier path under `/home`), see [Mounting subvolumes](mounting-subvolumes.md).
   And add `[Mounting subvolumes](mounting-subvolumes.md)` to the "What's next" bullet list.

5. **`manual/guides/sharing-and-permissions.md`** -- add a short pointer in the appropriate location (likely after "Adding users to the storage group" or as a new short subsection "Read-only access for service users"):
   > For network-facing services like Jellyfin or Plex that should only read a single subtree, prefer mounting that subvolume separately and using POSIX ACLs over adding the service to the `storage` group. See [Mounting subvolumes](mounting-subvolumes.md) for the recipe.
   And add the guide to the "Related" / cross-link section at the bottom.

6. **New VM test: `tests/module/subvol-mount-lifecycle.nix` + `tests/module/subvol-mount-lifecycle.py`**, registered in `flake.nix` `checks.aarch64-darwin` alongside the existing `lock-stops-bound-consumers` / `systemd-lifecycle` entries. The test pins the lifecycle contract the guide documents.

   **Template / parent test:** `tests/module/lock-stops-bound-consumers.{nix,py}`. That test already exercises the `BoundBy braid-online.service` cascade for a generic consumer holding `exec 3>/mnt/storage/.consumer-lock`; the new test specializes the same scaffolding to a *mount unit* plus a service that holds the bind target busy.

   **Deterministic UUIDs (closes the eval-time UUID gap):** `systemd.mounts.what` must reference a stable `by-uuid` path that exists at config eval. The shared `tests/module/lib/initrd-fixture.nix` already pins LUKS UUIDs via the `diskUuidMap` / case-statement path; extend it with an optional `btrfsFsid` parameter that is fed straight into `mkfs.btrfs -U <fsid> ...` in the existing `mkfsCmd`. Default `null` keeps every other test's behavior unchanged.

   **Exact fixture import** (specified to remove implementer ambiguity -- the fixture documents the `supportedFilesystems` knob at `tests/module/lib/initrd-fixture.nix:19` for in-initrd mounts, and `tests/module/degraded-raid1.nix:31` shows the exact pattern to mirror):
   ```nix
   (import ./lib/initrd-fixture.nix {
     inherit passphrase diskNames;
     supportedFilesystems = [ "btrfs" ];   # btrfs kmod in initrd so preCloseScript can mount
     btrfsFsid = "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
     description = "Prepare LUKS + btrfs fixture with movies subvolume for mount-lifecycle test";
     preCloseScript = ''
       mkdir -p /tmp/fixture-mount
       mount /dev/mapper/braid-disk1-fmt /tmp/fixture-mount
       btrfs subvolume create /tmp/fixture-mount/movies
       # Pre-create the busy-mount probe file. Mode 644 so the unprivileged
       # dummy-jellyfin service can open it read-only at runtime against the
       # `ro` subvol mount; an open(O_WRONLY) would fail on a ro mount, so
       # the test must use read-only fd to hold the mount busy.
       touch /tmp/fixture-mount/movies/.consumer-lock
       chmod 0644 /tmp/fixture-mount/movies/.consumer-lock
       sync
       umount /tmp/fixture-mount
     '';
   })
   ```
   - The fixed FSID `aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa` is the value the test's `systemd.mounts.what` references literally.
   - `supportedFilesystems = [ "btrfs" ]` is load-bearing: without it the initrd lacks the btrfs kmod and `mount /dev/mapper/...` in `preCloseScript` fails. This is the gap `degraded-raid1.nix` already closes for its own preCloseScript mount.
   - `.consumer-lock` is created at this exact step so the dummy service has a deterministic file to hold open read-only at runtime. No fallback paths, no either/or.

   **NixOS config (test fixture):**
   - Enable braid (`braid.enable = true; braid.package = braid;`) and pre-populate `pool.json` with the deterministic LUKS UUIDs (same shape as `lock-stops-bound-consumers.nix`).
   - Declare the documented `systemd.mounts` entry verbatim:
     ```nix
     systemd.mounts = [{
       what = "/dev/disk/by-uuid/aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa";
       where = "/var/lib/jellyfin/media";
       type = "btrfs";
       options = "subvol=movies,ro,noatime";
       wantedBy = [ "braid-online.service" ];
       bindsTo  = [ "braid-online.service" ];
       after    = [ "braid-online.service" ];
     }];
     ```
   - Declare `systemd.services.dummy-jellyfin` that mirrors the guide's Jellyfin shape: `wantedBy/bindsTo/after = [ "var-lib-jellyfin-media.mount" ]`, `unitConfig.ConditionPathIsMountPoint = "/var/lib/jellyfin/media"`, and an `ExecStart` that opens the pre-created lock file read-only and blocks. The `.consumer-lock` file was placed inside the `movies` subvolume by `preCloseScript` above, so it appears at `/var/lib/jellyfin/media/.consumer-lock` once the `ro` subvol mount activates:
     ```sh
     exec 3</var/lib/jellyfin/media/.consumer-lock
     sleep 300
     ```
     Read-only `<` is mandatory because the mount is `ro`; an `O_WRONLY` open would fail at activation time and the test would be meaningless. Holding fd 3 open against a file inside the mount is what makes the mount busy until systemd stops the service.

   **Assertions (behavioral, not log-string-based):**
   1. After `braid unlock`: `systemctl is-active var-lib-jellyfin-media.mount` is `active`, and `dummy-jellyfin.service` is `active`.
   2. `systemctl show -P BoundBy braid-online.service` output contains `var-lib-jellyfin-media.mount` -- this is the literal contract the guide makes ("declared this way, your mount participates in `braid lock`'s cascade").
   3. Run `braid lock` and assert **exit 0**. This is the behavioral test that subsumes journal-order parsing: if the cascade did not stop `dummy-jellyfin` before unmounting `var-lib-jellyfin-media.mount`, the open fd would `EBUSY` the unmount and `braid lock` would not reach LUKS close cleanly. Successful exit is the proof that ordering is correct.
   4. Post-lock teardown: `dummy-jellyfin.service` is `inactive`, `var-lib-jellyfin-media.mount` is `inactive`, `/mnt/storage` is not a mountpoint, and `cryptsetup status braid-disk1` / `braid-disk2` return "is inactive" (LUKS mappers closed).
   5. Second cycle: re-`braid unlock`, assert mount + service both reactivate (proves `wantedBy` on the mount unit pulls everything back up). This catches asymmetric failures where shutdown works but startup is broken on the second activation.

   **Preamble:** standard three-section comment (intent / why it exists / scenario) per `docs/testing.md`. Scenario should explicitly reference the guide: "documented in manual/guides/mounting-subvolumes.md; this test is the regression gate for the systemd.mounts + bound-service shape that guide tells users to write."

## Style notes (apply throughout)

- ASCII only: `--` not em-dash, straight quotes, `4x` not `4×`.
- Match the existing guide voice: short, cookbook-like, copy-paste examples first, prose second.
- All NixOS snippets use `config.braid.mountPoint` and `config.braid.poolAccessGroup` (not literals) where applicable, to match the convention already established in `sharing-and-permissions.md`.

## Out of scope (deliberately)

- No CLI / module / wrapper source changes. The plan adds a new VM test (under `tests/module/`) and one additive parameter to the shared `tests/module/lib/initrd-fixture.nix` helper (`btrfsFsid ? null`, default no-op), but does not modify production code paths.
- No new NixOS module options. Subvolume mounts are user-owned NixOS config -- braid stays hands-off per ADR 003.
- No change to existing mount behavior, ADRs, or principles.
- Surfacing `PoolState.fsid` in `braid status` is flagged inside the guide as a future enhancement but is **not** done in this plan.

## Verification

The contract the guide makes is "this recipe participates in `braid lock`'s `BoundBy` cascade so the LUKS close step is safe." The VM test (item 6 in Critical files) is the primary verification -- it pins that contract.

1. **Run the new VM test** (lifecycle contract):
   ```sh
   just test-vm subvol-mount-lifecycle
   ```
   All assertions from item 6 above must pass. This is the regression gate; manual host verification is not sufficient because the contract is a systemd ordering claim that humans cannot reliably eyeball.

   **Plus**: because the plan touches `tests/module/lib/initrd-fixture.nix` (adding the optional `btrfsFsid` parameter), re-run at least one existing consumer of the fixture that does *not* pass `btrfsFsid` to confirm the default-`null` path is a no-op:
   ```sh
   just test-vm lock-stops-bound-consumers
   ```

2. **Render the mdbook locally** and confirm the new guide renders:
   ```sh
   cd /Users/dan/Code/braid/manual && mdbook build && mdbook serve
   ```
   - Open `http://localhost:3000`, confirm "Mounting subvolumes" appears in the left nav and the page renders cleanly (no broken anchors, code blocks highlighted).
   - Click every relative link in the new guide and confirm none 404.
   - `grep -rn "mounting-subvolumes" /Users/dan/Code/braid/manual/` to confirm every inbound cross-link target resolves.

3. **Optional host sanity check** on caja (not a substitute for the VM test, but exercises the rendered recipe against a real pool):
   ```sh
   ssh dan@caja
   sudo btrfs subvolume create /mnt/storage/test-subvol
   sudo btrfs filesystem show /mnt/storage         # capture uuid
   # apply documented systemd.mounts snippet to ~/world/hosts/caja, then:
   sudo nixos-rebuild switch
   findmnt /home/dan/test-subvol                    # mount present
   systemctl show -P BoundBy braid-online.service   # mount unit listed
   sudo braid lock                                  # exits 0
   findmnt /home/dan/test-subvol                    # gone
   ```
   Tear down: revert the config change, `sudo braid unlock`, `sudo btrfs subvolume delete /mnt/storage/test-subvol`.

## Potential follow-ups (not part of this plan)

- Surface `PoolState.fsid` in `braid status` human + JSON output (`cli/src/status.rs`). Small code change; would let the guide drop the `btrfs filesystem show` step.
- Confirm `install-nixos.md` and `ups.md` are intentionally absent from `manual/index.md`'s Guides table -- separate cleanup if not.
