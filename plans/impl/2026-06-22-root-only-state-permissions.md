# Plan: make braid state root-only -- 0700 directory and 0600 files

## Context

A security audit (`findings/4-file-directory-permissions.md`, Findings 1-2) found that
braid's on-disk state under `/var/lib/braid` relies on the ambient process umask (0022
under systemd): the files land mode 0644 (group/world-readable in the mode bits) instead
of 0600. Affected (the full create-site set -- re-derived against live HEAD, see the
moving-target note below): the JSON state files (`pool.json`, `pending-op.json`,
`acked-stats.json`, `enospc-ack.json`, `alert-latch.json`), the `alert-latch.json.corrupt`
quarantine hardlink, the `pool.json.corrupt-<ts>` forensic sidecars, the
`alert-cleanup-pending` sentinel, and the three shell-`touch` flag files (`smartd-alert`,
`scrub-failed`, `scrub-cancel-requested`).

This remains defense-in-depth, not a live leak: the state directory is root-owned and
non-root cannot traverse in. But the *ideal target* is a single, strict boundary --
**a root-only directory (0700 root:root) holding root-only files (0600 root:root)** --
not the status-quo "0750 parent plus 0600 files."

Why tighten the directory from 0750 to 0700: `/var/lib/braid` is root-owned
implementation state, not a user/group interface. 0750 root:root is *effectively*
root-only today only because normal users are not in group `root`; 0700 makes the
boundary simpler and stricter -- no non-root group traversal, listing, or metadata
access, independent of group membership. It also matches braid's own precedent for
root-only state dirs: `storage.nix` already creates `/run/braid-key` and
`/run/braid-key/mnt` at `0700 root root` ("non-root cannot traverse"), and `luks.rs`
creates `/var/lib/braid/luks-headers` at `0o700`. The state root is the last root-owned
dir still at 0750.

The shared data pool is a *separate* concern and stays separate: `poolAccessGroup`
applies to the mounted NAS data root (`root:poolAccessGroup 2770`, set by Rust
post-unlock fixups -- `storage.nix` mount-point comment; `013-mount-permissions.md`),
never to `/var/lib/braid`, whose group is `root`.

The 0600 file plan stays in full -- as defense in depth and as the *direct* file boundary
if the directory is ever copied, moved, or misconfigured (e.g. a backup tool that
flattens the dir mode). It is also a real consistency fix: every *sensitive* writer in
braid declares its mode at create time -- `luks.rs#backup_luks_header_to` (0400),
`enroll_key_file.rs#generate_key_file` (0400), `pool_lock.rs#open_lock_file` (0600),
`ups.nix` (`umask 077`) -- while the state writers alone lean on the umask. Two of the
umask-dependent files carry exactly the topology data the audit flagged: `pool.json` and
its `pool.json.corrupt-<ts>` forensic copies (disk serials, LUKS UUIDs).

The ideal, per braid's "declare intent at the write boundary" pattern, is for *every*
file braid creates to be exactly 0600 at the moment it is written. braid owns these write
paths and runs as root, so this is fully enforceable at the source. Where a create site
can *safely* converge a file left 0644 by an older binary, it does (the module flag
files -- section 4); the cleanup sentinel is the one deliberate exception -- its
existing-file path is left untouched to preserve a documented "not re-wedged by permission
drift" invariant (see section 3).

**The create-site set is a moving target until this lands.** This inventory was re-derived
against live HEAD, not inherited from the audit (`findings/4`), which predates
`enospc-ack.json`, `scrub-failed`, `scrub-cancel-requested`, and the `scrub-alert.nix`
test (all added recently, some within this planning window). If HEAD moves again before
merge, re-run the three authoritative sources: `cli/src/state_paths.rs` (`StatePaths` --
the canonical file set), `grep -rn 'touch .*/var/lib/braid' modules/` (the shell create
sites), and `grep -rln 'd /var/lib/braid 0755' tests/module/` (the stub fixtures).

## Approach

Two layers: the **directory** (section 0) and the **files** (sections 1-4). The directory
is enforced 0700 on *every* creation path -- the `storage.nix` tmpfiles rule (production
owner) and `state_io.rs#atomic_write`'s own `create_dir_all` fallback (CLI/standalone
path) -- leaving no creation seam at a looser mode.

The files funnel through **two chokepoints** plus two one-off Rust sites: the Rust
`atomic_write` for all JSON state (section 1, five callers) and a shared module
`braid-touch-flag` script for the shell-created flag files (section 4, three flags), plus
the forensic sidecar (section 2) and the cleanup sentinel create path (section 3). At each
**Rust** create site (sections 1-3) set `.mode(0o600)` at open **and** immediately
`set_permissions(0o600)` on the open file handle before writing. `.mode()` alone is insufficient: Rust masks it with the process
umask (so a nonstandard umask yields something other than 0600) and it only applies when
the inode is *created* (so a reused stale `.X.tmp` keeps its old mode).
`File::set_permissions` is `fchmod` on the open fd -- it sets the mode exactly, ignores
the umask, converges a reused inode, and is TOCTOU-free (operates on the fd, follows no
path). Keeping `.mode(0o600)` as well means the inode is never even briefly observable at
0644.

### 0. State directory: 0700 on every creation path

The directory has two creators -- the module's tmpfiles rule and the CLI's own
`create_dir_all` fallback -- and both are pinned to 0700.

**0a. Module (production owner): `modules/braid/storage.nix` tmpfiles rule.**
Change `d /var/lib/braid 0750 root root -` to `d /var/lib/braid 0700 root root -`.
systemd-tmpfiles creates `/var/lib/braid` early at boot -- before the smartd hook or any
braid service runs (the existing `storage.nix` comment notes the hook needs the dir to
pre-exist) -- so in the module the dir is 0700 from first boot. Root-run services and the
CLI are unaffected (root ignores the group/other bits). Existing deployments converge
with no migration step: on `--create`, tmpfiles re-applies a `d` rule's mode to an
*existing* directory -- an existing dir takes the `CREATION_EXISTING` path in
`create_directory` -> `fd_set_perms`, which chmods when the rule's mode differs from the
dir's current mode (`reference/systemd/src/tmpfiles/tmpfiles.c`) -- so every `nixos-rebuild
switch`/boot tightens an already-existing 0750 dir to 0700. This convergence is now
behavior-tested (see Tests, VM convergence subtest), not merely asserted.

**0b. CLI/standalone path: `cli/src/state_io.rs#atomic_write`.**
`atomic_write` opens with `fs::create_dir_all(dir)` (umask-default mode) before writing.
In the module tmpfiles always wins the creation race, so this rarely creates the dir in
production -- but the *ideal from-scratch boundary* is that braid never creates the state
dir looser than 0700 on *any* path (CLI-first, standalone, or a future caller), mirroring
the file-mode boundary and braid's own `luks.rs` `luks-headers` `0o700` precedent. Make
the create explicit-0700:

- Add `use std::os::unix::fs::{DirBuilderExt, PermissionsExt};`.
- `fs::DirBuilder::new().mode(0o700).recursive(true).create(dir)?` (mode at `mkdir`, so a
  freshly-created dir is never briefly observable looser), then
  `fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?` -- the latter forces
  exact 0700 regardless of umask **and** converges a dir an older binary left at 0755,
  neither of which `.mode()` (umask-masked, create-only) does on its own. This is the
  directory analogue of the file sites' `.mode()`-plus-`set_permissions`; it uses a
  path-based `set_permissions` (not an fd `fchmod`) because `std` has no cheap dir-fd chmod
  and the dir's `/var/lib` ancestry is root-owned, not attacker-controllable.
- `.mode()` on a recursive `DirBuilder` would also stamp any *parents* it creates, but
  `/var/lib` always pre-exists (FHS/systemd), so only the `braid` leaf is ever created and
  the leaf `set_permissions` is the load-bearing step. (Equivalently: plain
  `create_dir_all` + leaf `set_permissions(0o700)` -- same result, parents untouched by
  construction.)

### 1. JSON state: `cli/src/state_io.rs#atomic_write`

The chokepoint for all five JSON state files.

- Add `use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};`.
- `.mode(0o600)` on the temp-file `OpenOptions`, then
  `tmp.set_permissions(std::fs::Permissions::from_mode(0o600))?` after open, before
  `write_all`. This forces exact 0600 regardless of umask and re-tightens a reused
  stale `.X.tmp` before the rename -- closing the stale-tmp hole that was previously an
  out-of-scope exception.
- Update the `///` doc comment to record the now-true invariant: braid state files are
  written exactly 0600 regardless of the caller's umask.

Hardcode 0600 rather than a `mode` parameter. All five callers
(`membership.rs#save_membership_to`, `journal.rs` save, `alert.rs#save_acked_stats_at`,
`alert.rs#save_alert_latch`, `alert.rs#save_enospc_ack`) write root-only state under the
now-0700 dir; encoding the invariant in one place is *more correct* than a per-caller
decision. `enospc-ack.json` persists a `PoolKey { fs_uuid, devices }` (FS UUID +
per-device topology) -- the same data class the audit flagged for `pool.json`.

Existing JSON files self-heal: `atomic_write` writes a fresh inode and renames over the
target, so they reach 0600 on their next write.

Transitively fixes the quarantine hardlink (`alert.rs#quarantine_corrupt_latch`,
`std::fs::hard_link`): a hardlink shares the inode, so once the latch is 0600 the
`.corrupt` copy is too. No separate change.

### 2. Forensic sidecar: `cli/src/membership.rs#write_corrupt_sidecar_at`

The highest-value omission: this snapshots the *full corrupt `pool.json` bytes* (same
topology/serial/LUKS-UUID data) via
`OpenOptions::new().write(true).create_new(true).open(candidate)` with no mode -> 0644.

- Add `use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};`, `.mode(0o600)` on the
  `create_new(true)` open, and `f.set_permissions(from_mode(0o600))?` after open, before
  `write_all`.

`create_new(true)` is always a fresh file, so there is no stale-inode case here; the
`set_permissions` call is for exact-0600-regardless-of-umask consistency with the other
sites. Pre-existing forensic snapshots are write-once and are *not* retroactively swept.

### 3. Sentinel: `cli/src/alert.rs#mark_alert_cleanup_pending` (create path only)

- **Create path** (sentinel absent): bind the handle, `.mode(0o600)` +
  `set_permissions(from_mode(0o600))` before returning, so a new sentinel is exactly
  0600. Add the `OpenOptionsExt`/`PermissionsExt` imports.
- **Existing-sentinel path: leave untouched (no convergence).** The function documents
  -- and a test pins -- that an existing regular sentinel "is accepted without reopening
  ... not re-wedged by later permission drift"
  (`mark_alert_cleanup_pending_existing_read_only_file_does_not_require_write_permission`
  asserts the read-only sentinel "must stay untouched"). chmod-ing it on the early-return
  path would contradict that invariant, so we do not. An old 0644 sentinel therefore
  persists until removed -- acceptable: it is an empty, short-lived (cleared on cleanup
  completion), unreachable flag.

This also dissolves the symlink hazard: because nothing chmods the existing path, there
is no `path.is_file()`-followed symlink target to mutate. (Were convergence ever
wanted here, it would have to gate on `symlink_metadata().file_type().is_file()` to
avoid chmod-through-symlink -- but the cleaner answer is not to converge at all.)

### 4. Module flag files: one shared `braid-touch-flag` chokepoint

braid creates three empty flag files from **shell**, each by a bare `touch
/var/lib/braid/<flag>` with no `chmod` -- so all three land umask-0644 today:

- `smartd-alert` -- `smartdAlertScript` (`pkgs.writeShellScript`, smartd `-M exec`
  callback) in `monitor.nix`.
- `scrub-failed` -- the inline `braid-scrub-failed.service` `script` in `monitor.nix`.
- `scrub-cancel-requested` -- `scrubCancelScript` (`pkgs.writeShellScript`, the scrub
  `ExecStop`) in `storage.nix`.

(The shell `touch` is the *only* production create site for each: every Rust
`fs::write(paths.scrub_failed()/scrub_cancel_requested())` is `#[cfg(test)]`.) These are
the shell analogue of the `atomic_write` JSON chokepoint, and they recur -- a fourth flag
added later would again be a bare `touch`. So rather than sprinkle three `chmod`s,
introduce **one shared script** and route all three sites through it:

- `braidTouchFlag = pkgs.writeShellScript "braid-touch-flag" ''...''` running
  `${pkgs.coreutils}/bin/touch "$1"` then `${pkgs.coreutils}/bin/chmod 0600 "$1"`. Use
  *absolute* coreutils paths: a `writeShellScript` has no ambient PATH, unlike the inline
  scripts that bare-`touch` today by inheriting the unit/smartd PATH.
- Place it where both `monitor.nix` and `storage.nix` can reference it -- a small shared
  module file, mirroring how `storage.nix` already does `import ./constants.nix`.
- Replace each `touch /var/lib/braid/<flag>` with `${braidTouchFlag} /var/lib/braid/<flag>`.

Every flag is then 0600 by construction, and the next flag's author gets it for free.
`chmod 0600` runs unconditionally, so it sets a fresh flag and converges an existing 0644
one. The brief post-`touch` window is irrelevant -- the flags are empty and unreachable
behind the 0700 dir. If a flag is provisioned as a symlink-on-tmpfs (a supported case for
`smartd-alert`; its reader test covers a symlink resolving to a regular file), `chmod`
follows it to the real flag, the intended target -- benign here (root-owned empty markers
behind 0700), unlike the arbitrary-target risk on the sentinel path. All three run as root
from shell (smartd callback / unit scripts), not as a braid binary, so an in-script `chmod`
via the shared helper -- not a unit `UMask=`, which would miss the smartd callback -- is
the correct lever.

### Tests

**Preamble form:** literal `//` line-comment block (Intent / Why it exists / Scenario)
directly above each test, per `docs/dev/testing.md` ("Preamble: literal `//`
line-comment form") and AGENTS.md. NOTE: `state_io.rs`'s existing tests use `/* */`
block comments -- that is legacy non-conformance; new tests use `//` (as the
`membership.rs` and `alert.rs` test modules already do).

**Rust unit (`cargo test`)** -- mirror the keyfile-0400 style in `enroll_key_file.rs`
(`use std::os::unix::fs::PermissionsExt; meta.permissions().mode() & 0o777 == 0o600`).

A fresh-create `== 0600` assertion does **not** by itself pin the fd `set_permissions`:
it is umask-invariant under every sane umask (0022/0077 mask only group/world bits,
which 0600 already clears), so `.mode(0o600)` alone passes it -- the assertion would
survive deletion of `set_permissions`. A mode test discriminates the fd chmod only
(a) against a reused stale inode (`.mode()` never applies) or (b) under an owner-masking
umask (e.g. 0777, where `.mode(0o600)` alone yields 0000). The three sites split on this:

- `cli/src/state_io.rs` (normal lane, any umask): three tests, none needing a hostile
  umask. (i) The stale-tmp test pins the *file* fix -- pre-create the `.<name>.tmp` and
  explicitly `set_permissions(0o644)` on it (not via umask), call `atomic_write`, assert
  the final file is 0600; without the fd chmod the reused inode stays 0644 and this fails.
  (ii) Extend `atomic_write_creates_parent_dirs` to also assert the created leaf dir is
  0700 -- under the test umask a bare `create_dir_all` yields 0755 != 0700, killing the
  unhardened create (section 0b). (iii) Add a *directory-convergence* test: pre-create the
  parent dir and explicitly `set_permissions(0o755)` on it (not via umask), call
  `atomic_write`, assert the dir is 0700 -- this umask-independently pins the dir's
  post-create `set_permissions`, which a `.mode()`-only or bare create cannot pass.
  (ii)+(iii) pin the section-0b dir fix entirely in the normal lane, exactly as the
  stale-tmp test pins the file fix.
- `cli/src/membership.rs`: `create_new` is always a fresh inode (case b only). Keep
  `write_corrupt_sidecar_preserves_existing_snapshot_and_appends_suffix` in the normal
  lane -- it reads back a pre-existing snapshot and so cannot run under an owner-masking
  umask (an owner cannot read its own 0000 fixture); its 0600 assertion documents intent
  but does not pin the chmod. Add a *dedicated minimal* fresh-create test (no collision;
  pass bytes, read no umask-masked fixture) for the gate.
- `cli/src/alert.rs`: the sentinel create path is always fresh too (a pre-seeded inode
  diverts to the untouched existing-path), case b only. Add a fresh-sentinel test for the
  gate asserting `mark_alert_cleanup_pending` creates a *new* sentinel at 0600. Keep
  `mark_alert_cleanup_pending_existing_read_only_file_does_not_require_write_permission`
  pinning that an existing sentinel is left untouched -- do not weaken it.

Gate tests (sidecar + sentinel) prove umask-independence, so each masks owner bits
itself: create the scratch dir *first* under the normal umask (so it stays usable), then
`nix::sys::stat::umask(Mode::from_bits_truncate(0o777))` (or `libc::umask` -- both are
already direct deps), drive the create, restore the prior umask, assert 0600. Because
that mutates process-global umask, mark them `#[ignore]` (with a comment) under a shared
filter token (`exact_0600`) so they never run in the default parallel lane and race
siblings; the gate runs them `--ignored --test-threads=1`. Do **not** set the umask at
the shell around `cargo test` -- that runs the *build* under 0777 (target dirs created
0000) and fails to compile.

**NixOS VM (extend the existing real-hook test -- not a new VM):**

`tests/module/smartd-hook.nix` imports the real `modules/braid` and adds **no**
`/var/lib/braid` tmpfiles stub, so its `/var/lib/braid` is created by the production rule
-- making it the one module test where asserting the production *directory* mode is
meaningful (it reads 750 before this change, 700 after). Extend `smartd-hook.py`:

- **Directory mode (section 0a guard):** after `wait_for_unit("multi-user.target")`,
  assert `stat -c %a /var/lib/braid` == `700` (fresh-boot mode). Then add a *convergence*
  subtest: `chmod 0750 /var/lib/braid`, run `systemd-tmpfiles --create`, and re-assert
  `== 700` -- this behaviorally pins the "existing 0750 deployments tighten on activation"
  claim (the source path verified in section 0a), which the fresh-boot assertion alone
  does not exercise. (Leaves the dir at 700 for the subtests that follow.)
- **Flag-file mode (section 4 guard), all three `braidTouchFlag` flags:**
  - `smartd-alert` -- `smartd-hook.py` subtest 2 already rm's and re-creates the flag via
    the real hook (`machine.succeed(f"{hook_path}")`); assert
    `stat -c %a /var/lib/braid/smartd-alert` == `600` after invocation, plus a convergence
    case (pre-create 0644, invoke the hook, assert 600).
  - `scrub-failed` -- extend `tests/module/scrub-alert.py` (it already drives
    `onFailure -> braid-scrub-failed.service` and defines
    `SCRUB_FAILED_FLAG = /var/lib/braid/scrub-failed`) to assert that flag is `600` after
    the service runs.
  - `scrub-cancel-requested` -- extend `tests/module/scrub-lifecycle.py` (its `cancel`
    subtest exercises the scrub `ExecStop`/`scrubCancelScript`) to assert that flag is
    `600` after a deliberate cancel.
  Each fails if its site is not routed through `braidTouchFlag`. (`smartd-hook`,
  `scrub-alert`, and `scrub-lifecycle` are already registered in `flake.nix`.)

**The 19 module tests that stub their own `d /var/lib/braid 0755 root root` (22 stub lines
-- `scrub-lifecycle`, `immutable-mountpoint`, and `scrub-alert` carry two each): delete the
stub `d` line(s).** Every one of the 19 imports the module *and* enables braid, so the
module's own `d /var/lib/braid 0700` rule already creates the dir -- the stub `d` is pure
redundancy. Only the paired `f /var/lib/braid/pool.json` seed line is load-bearing; keep
it. Deleting the stub `d` is strictly better than syncing it to 0700: no duplicate at all,
so no `is_duplicated_item` NOTICE, no parse-order dependence, and the dir is unambiguously
the module's 0700 (matching production). (systemd keeps the *first-parsed* incompatible
duplicate and ignores later ones with a `LOG_NOTICE` -- `tmpfiles.c#is_duplicated_item`; a
lingering 0755 stub could otherwise win the parse race and boot the very 0755 this plan
eliminates, silently, since these tests assert no dir mode.) The dir-before-seed ordering
holds -- systemd-tmpfiles creates `d /var/lib/braid` before the `f /var/lib/braid/pool.json`
seed by path-prefix (parent before child) -- and is self-verifying: if it failed, all 19 VM
tests would fail at boot when `braid unlock` cannot find `pool.json`, so the existing suite
is the backstop (worth one confirming run during implementation). The 19:
`tests/module/{ups-lb-clean-shutdown, systemd-lifecycle, auto-unlock-key-wrong,
scrub-lifecycle, lock-stops-bound-consumers, braid-doctor-ups, braid-lock-systemd-stop,
pool-lock-contention, lock-tolerates-missing-pool-json, immutable-mountpoint,
mark-online-skips-start-while-deactivating, monitor-lifecycle, execstop-cleans-stale-online,
subvol-mount-lifecycle, auto-unlock-key-present, add-locked-pool, braid-lock-coordinator-race,
braid-lock-then-unlock-no-race, scrub-alert}.nix`.

## Out of scope (considered, deliberately excluded)

(The CLI `create_dir_all` 0700 hardening and the module-test `0755` stub fixtures, both
previously parked here, are now **in scope** -- sections 0b and Tests respectively, folded
in after reviewer direction.)

- **Editing `findings/` or any `docs/` page.** No `docs/` page hardcodes the state-dir
  mode, and `principles.md` enshrines no permission invariant for it (only that
  membership state is CLI-owned in `pool.json`), so nothing under `docs/` needs to change.
  The `findings/` audit (untracked) describes the pre-change 0750 state and is left as the
  historical record.
- **doctor drift-detection check.** A `check_state_permissions` sibling to
  `doctor.rs#check_config_permissions` would only *warn*, on a condition unreachable
  behind the 0700 dir -- noise, not signal. config.json needs detection because its mode
  is set by Nix/operators outside braid's control; state files are braid-owned, so
  prevention at the source is total. (Reviewer concurs: no `UMask=`/doctor pivot.)
- **Dedicated migration sweep / tmpfiles relabel.** Not needed: the directory tightens to
  0700 on the next activation (tmpfiles re-applies the `d` mode to the existing dir --
  verified in source, behavior-tested per Tests); JSON files self-heal on next
  `atomic_write`; the `smartd-alert` flag converges via the explicit chmod at its create
  site; the cleanup sentinel's existing-file path is intentionally left untouched
  (section 3), so an old 0644 sentinel persists -- acceptable for an empty, short-lived,
  unreachable flag; old write-once forensic sidecars are unreachable and left as-is. A
  startup chmod or tmpfiles `Z` sweep is machinery we don't need.

## Verification

- `just test-rust` (or `cargo test` scoped to `state_io` + `membership` + `alert`) --
  the stale-tmp variant, the new `state_io` directory-mode tests (fresh 0700 + the
  0755->0700 convergence pin, section 0b), and the existing `atomic_write`/durability/
  sidecar and sentinel read-only tests stay green. The two `#[ignore]`d `exact_0600` tests
  are skipped here.
- **Hostile-umask gate (required, standing):** add a `just` recipe (e.g.
  `test-state-modes`) running `cargo test --manifest-path cli/Cargo.toml --lib exact_0600
  -- --ignored --test-threads=1`. This executes the two fresh-create tests (sidecar +
  sentinel) that mask owner bits internally (see Tests), so each fails if its site's fd
  `set_permissions(0o600)` is removed -- the regression the normal lane (passing on
  `.mode` alone) cannot catch. Wire it into the same CI / pre-commit lane as
  `just test-rust`; per braid convention the recipe carries an explanatory `justfile`
  comment.
- `just test-vm smartd-hook` -- asserts `/var/lib/braid` is `700` on fresh boot **and**
  that a dir hand-set to `0750` converges back to `700` after `systemd-tmpfiles --create`
  (directory boundary + convergence, section 0a), and the `smartd-alert` flag is `600` on
  both fresh create and convergence (section 4).
- `just test-vm scrub-alert` and `just test-vm scrub-lifecycle` -- assert the `scrub-failed`
  and `scrub-cancel-requested` flags are `600` after their shell sites run (the other two
  `braidTouchFlag` flags, section 4).
- The full module-test suite (`just test-vm`) stays green with the redundant `d
  /var/lib/braid` stubs deleted from all 19 fixtures: the dir now comes solely from the
  module's 0700 rule, and a broken dir-before-seed ordering would fail every fixture at
  boot.
- Run the project's Rust format/lint lane (`cargo fmt`, `cargo clippy`) clean.
- `scripts/docs/check-output-ascii.py` is unaffected (no echo/Unicode added); run the
  normal pre-commit lane regardless.
- Doc/expectation audit (point 5): `rg -n '0750|/var/lib/braid' docs/` confirms no `docs/`
  page asserts the state-dir mode (only the data-mount `013-mount-permissions.md` and the
  USB-parent `luks-unlock.md` 0700, both unrelated), and `principles.md` enshrines no
  permission invariant for it -- so no doc updates are required.
- Optional manual end-to-end on a NixOS host/VM: confirm `stat -c %a /var/lib/braid`
  -> `700`, then trigger a state write (a `braid` membership change), a corrupt-pool
  rebuild, a smartd alert, a scrub failure, and a scrub cancel, then
  `stat -c %a /var/lib/braid/{pool.json,pool.json.corrupt-*,enospc-ack.json,smartd-alert,scrub-failed,scrub-cancel-requested,alert-cleanup-pending}`
  -> `600` (a brand-new `alert-cleanup-pending`; a pre-existing one is intentionally
  left as-is).

## Implementation notes

- Updated `cli/src/monitor.rs#save_acked_stats_failure_latches_computation_error` to poison the atomic temp path instead of chmodding the state directory read-only, because `cli/src/state_io.rs#atomic_write` now intentionally converges the parent directory to 0700 before saving.
