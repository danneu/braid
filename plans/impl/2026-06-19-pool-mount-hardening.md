# Plan: Mount the data pool `nosuid,nodev` (pool mount hardening)

## Context

`base_mount_options()` in `cli/src/cmd.rs` -- the single chokepoint for every
btrfs data-pool mount (`Mount` and `MountWithOptions` arms of
`CmdRequest::to_argv`) -- emits only `noatime,skip_balance,subvolid=5`. The live
pool therefore honors set-uid bits and device-special nodes. braid already
mounts the USB key with `ro,nosuid,nodev,noexec` (`modules/braid/storage.nix`,
`fileSystems."/run/braid-key/mnt"`), so the idiom is known but never applied to
the shared pool.

**Threat model (accurate -- the originating finding overstated it).** A *plain
unprivileged `poolAccessGroup` member* CANNOT exploit this: planting a
setuid-**root** binary needs `CAP_CHOWN`, and a device node needs `CAP_MKNOD` --
both root-only; `chmod u+s` on a file they own is only setuid-themselves (no
gain). The genuinely exploitable principals are:

1. **NFS `no_root_squash` remote-root.** braid's own guide ships this export
   option (`docs/guides/sharing-and-permissions.md`). A remote attacker who is
   root on their own client writes a setuid-root binary or device node onto the
   pool; any local NAS user then `exec`s it for clean local root.
2. **Root-run mode-preserving ingestion.** A backup restore, `tar -p`, or
   `rsync -a` run by root that lands a setuid-root payload on the pool.

`nosuid,nodev` at braid's mount layer neutralize the *privilege-escalation facet*
both principals share -- a planted setuid-root binary loses its bit on exec, and
device nodes are inert -- regardless of share config. This is standard CIS-grade
data-partition hardening with zero functional cost: braid never needs suid
binaries or device nodes on bulk storage.

What `nosuid,nodev` do **not** close is NFS `no_root_squash`'s *separate, broader
facet*: remote client-root mapped to server-root holds full
read/write/chown/delete authority over every pool file (data tampering, secret
disclosure) with no local `exec` required. The mount fix and the guide flip to
`root_squash` (below) are therefore complementary, not redundant -- one closes the
escalation facet, the other closes the remote-root-authority facet.

**Outcome:** the data pool mounts `noatime,skip_balance,subvolid=5,nosuid,nodev`,
with `nosuid,nodev` always rendered **last** (after any extra such as `degraded`)
so they cannot be overridden. `noexec` is deliberately excluded (a NAS
legitimately stores and runs executables). The decision is pinned in a new ADR
and asserted behaviorally.

## Decisions (confirmed)

- **`nosuid,nodev` only**, unconditional and **positioned last** in the option
  list so no caller-supplied extra (a future internal `suid`/`dev`/`exec`) can
  override them -- `mount(8)` applies the last conflicting flag as the winner, so
  the hardening must render *after* `MountWithOptions.options`, not merely prepend
  (Principle 3, safe-by-construction). No `noexec`, no module knob: `noexec` fails
  Principle 7 (admins don't *always* want it) and adds marginal security over
  `nosuid,nodev`; a knob is YAGNI and would force config into the currently
  config-free mount-option path. The `noexec` exclusion is recorded as a
  deliberate, revisitable non-decision.
- **New ADR-032** as authority (not ADR-015: its thesis is HDD-vs-flash tuning,
  and this is media-agnostic security; not folded into ADR-013: that would
  stretch its group-permissions thesis across two mechanisms). Banner principle:
  #7 Sane defaults.
- **Update the canonical invariant list.**
  `docs/design/principles.md#3-safe-by-construction-operations` names only
  `skip_balance` in its mount-options bullet; add `nosuid,nodev` and point to
  ADR-032. Per AGENTS.md the principles are law, so a new ADR alone leaves the
  central invariant list incomplete.
- **Flip the NFS guide example to `root_squash`** (the `exports(5)` default) and
  frame `no_root_squash` only as an explicit fully-trusted-client opt-out. This
  closes `no_root_squash`'s *remote-root-authority facet* -- distinct from the
  escalation facet the mount fix handles (see Context): mapping remote client-root
  to server-root grants full read/write/chown/delete over pool files (tamper,
  secret disclosure), which `nosuid,nodev` do NOT touch. So the guide must stop
  teaching it as the copy-paste default, not merely caveat it.

The hardening must live in the Rust mount-option assembler -- `mount_options(extra)`
appending `enforced_mount_options()` last (Implementation 2) -- not a Nix
`fileSystems` entry: the pool mount is imperative/lifecycle-driven
(post-LUKS-unlock), so there is no declarative chokepoint. (`base_mount_options()`
stays the prependable base list; it is no longer where the hardening invariant
lives.)

## Implementation

Work in TDD order: land the failing behavioral assertion + argv-pin expectations
first, confirm they fail for the right reason, then change the mount-option
assembly.

### 1. Behavioral test first (the TDD anchor)

`tests/cli/braid-unlock.py` (~line 152-159) already pins mount options via
`findmnt -o OPTIONS -n /mnt/storage` (and even cites ADR-015). Extend it:

- Add `assert "nosuid" in opts` and `assert "nodev" in opts`.
- Add `assert "noexec" not in opts` -- pins the deliberate exclusion, mirroring
  the existing `assert "compress" not in opts`.
- Update the `# Mount options pinned by ADR 015` comment to also cite ADR-032.

### 2. Code change (`cli/src/cmd.rs`)

Render the hardening flags **last** so they always win, rather than prepending
them in `base_mount_options()` where a future caller extra could override them:

- Keep `base_mount_options()` as-is (`noatime,skip_balance,subvolid=5`).
- Add `enforced_mount_options() -> ["nosuid","nodev"]` with a doc comment in the
  existing per-option style: why the flags (privilege containment of the shared
  pool), why they are positioned last (`mount(8)` last-wins; fail-closed against
  any future `suid`/`dev` extra), why not `noexec` -- citing ADR-032.
- Add a single assembler, `mount_options(extra: &[String])`, returning
  `base + extra + enforced` (enforced last). Route **both** the `Mount` arm
  (`extra = &[]`) and the `MountWithOptions` arm (`extra = options`) through it,
  collapsing the two arms' duplicated `-o` assembly. Give it a `///` doc comment
  (required for top-level CLI items -- AGENTS.md, `docs/dev/doc-comments.md`)
  stating its ownership invariant: it is the one place that composes base
  options, caller extras, and the enforced-last hardening, so `nosuid,nodev`
  cannot be reordered or dropped by any caller.

Emitted strings become:

- `Mount`: `noatime,skip_balance,subvolid=5,nosuid,nodev`
- `MountWithOptions(["degraded"])`: `noatime,skip_balance,subvolid=5,degraded,nosuid,nodev`

Keep all doc-comment text ASCII (enforced by
`scripts/docs/check-output-ascii.py` over `cli/src/**/*.rs`).

### 3. Update the argv pins + add the override-resistance regression

- `cli/src/cmd.rs` `mount_includes_skip_balance` (~2868): expected ->
  `"noatime,skip_balance,subvolid=5,nosuid,nodev"`. Refresh the Intent/Why
  comment to name the new security invariant.
- `cli/src/cmd.rs` `mount_with_options_includes_skip_balance` (~2890): expected
  -> `"noatime,skip_balance,subvolid=5,degraded,nosuid,nodev"` (enforced flags
  now follow the caller extra).
- **New test** `mount_hardening_flags_stay_last` (alongside the above): build a
  `MountWithOptions` whose `options` deliberately conflict
  (`vec!["suid".into(), "dev".into()]`) and assert the argv ends
  `...,suid,dev,nosuid,nodev` -- proving the enforced flags render after caller
  extras and win under `mount(8)` last-wins. This is the regression that pins the
  override-resistance invariant structurally.
- `cli/src/recover.rs` dry-run cycle test (grep the two
  `mount -o 'noatime,skip_balance,subvolid=5,degraded'` literals, ~19218): the new
  rendering appends the enforced flags, so update both to
  `noatime,skip_balance,subvolid=5,degraded,nosuid,nodev`.
- `cli/src/test_fixtures/unlock.rs` (~152, 170): the `ok_raw("mount -o ...")`
  strings are cosmetic mock-stdout labels keyed on the `CmdRequest` variant, not
  assertions -- update for consistency only (optional; won't break either way).

### 4. New ADR `docs/design/decisions/032-pool-mount-hardening.md`

Frontmatter `intent:` + `status: Active`. Structure mirroring recent ADRs:

- `# Decision: Pool mount hardening (nosuid/nodev)`
- `> Principle: [Sane defaults](../principles.md#7-sane-defaults)`
- `## Context` -- the accurate threat model above (call out that a plain group
  member cannot exploit it; the real principals are NFS `no_root_squash`
  remote-root and root-run mode-preserving ingestion). State the facet scope
  explicitly so the ADR does not blur it: `nosuid,nodev` close the
  setuid-binary/device-node escalation facet both principals enable, but NOT
  `no_root_squash`'s separate, broader facet (remote root's full
  read/write/chown/delete authority over pool files), which is what the guide
  flip to `root_squash` addresses. Also note the upgrade-time effect: on the
  next `unlock` after upgrade, any setuid bits or device nodes *already* present
  on the pool (e.g. from a restored system backup or a stored container/chroot
  image) become inert -- intended hardening, called out so an upgrader is not
  surprised.
- `## Decision` -- `nosuid,nodev` unconditional, rendered **last** by the
  `mount_options(extra)` assembler (which appends `enforced_mount_options()`
  after the base list and any caller extra) for both the `Mount` and
  `MountWithOptions` arms, so no caller-supplied option can override them.
- `## noexec excluded` -- deliberate, revisitable; rationale (NAS runs
  executables; marginal value over nosuid,nodev). Note this is where a future
  pool-exec-containment decision would land.
- `## See` -- code paths as backtick `path#symbol` spans, docs as markdown links
  (enforced by `scripts/docs/check-see-paths.py`):
  `` `cli/src/cmd.rs#mount_options` ``, `` `cli/src/cmd.rs#enforced_mount_options` ``,
  `` `tests/cli/braid-unlock.py` ``,
  `[ADR 013: Mount permissions](013-mount-permissions.md)`,
  `[ADR 015: HDD defaults](015-hdd-defaults.md)`,
  `[Sharing and permissions](../../guides/sharing-and-permissions.md)`.

Register in `docs/SUMMARY.md` under `# Decisions`, after the ADR-031 line:
`- [032: Pool mount hardening](design/decisions/032-pool-mount-hardening.md)`
(missing registration fails `mdbook-linkcheck2`).

Optionally add a reciprocal `[ADR 032 ...]` link from ADR-013's `## See`. Also
optional but consistent: ADR-028's "Always-on (non-configurable)" section lists
the unconditional base mount options (`noatime`, `skip_balance`) as the analogy
class for its always-on seal; `nosuid,nodev` join exactly that class, so add them
to that list and add reciprocal ADR-028 <-> ADR-032 `## See` cross-links.

### 4b. Update the canonical invariant (required)

`docs/design/principles.md`, `## 3. Safe-by-construction operations`: extend the
`Mounts always include skip_balance ...` bullet to also state the pool always
mounts `nosuid,nodev` (privilege containment of the shared pool), with a
`[Why ->]` pointer to ADR-032. Use the ASCII arrow `->` knowingly: the existing
ADR-022 bullet already uses `[Why ->]`, the ADR-028 bullet uses a Unicode arrow,
and principles.md mixes both -- ASCII is the repo/global default, so match the
ADR-022 form. This is required, not optional -- principles.md is the top-level
invariant list and AGENTS.md makes it law.

### 5. Doc syncs

- `README.md` (~296): the literal dry-run cookbook output
  `mount -o 'noatime,skip_balance,subvolid=5' ...` -> add `,nosuid,nodev` so it
  matches real output.
- `docs/commands/unlock.md` ("What happens under the hood", step 6): the
  enumeration "Mounts the btrfs filesystem with `noatime`, `skip_balance`, and
  `subvolid=5`" is now incomplete -- extend to "... `subvolid=5`, and
  `nosuid,nodev` (privilege containment)." Step 7's `degraded` note stays
  accurate. (The other `subvolid=5` hits in `docs/guides/mounting-subvolumes.md`
  identify the top-level subvolume or are user-side `subvol=` examples, not the
  baked-in option list -- no change.)
- `docs/guides/sharing-and-permissions.md` (NFS example, line 186): change the
  export from `...,no_root_squash` to `...,root_squash` (the `exports(5)`
  default). Add one line below framing `no_root_squash` as an explicit opt-out for
  fully-trusted clients only, noting it maps remote client-root to server-root
  over pool files -- the residual risk `nosuid,nodev` do NOT close. (Frame around
  remote-root mapping, NOT suid -- the mount fix already closes the suid path.)

### Out of scope

- `findings/` -- point-in-time audit artifacts; leave as the historical record.
- No `braid.*` module option; no change to the USB key mount (already hardened,
  different mechanism).

## Verification

- `just test-rust` -- the two argv pins, the new `mount_hardening_flags_stay_last`
  override-resistance test, and the recover dry-run pin; confirm green.
- `just test-vm braid-unlock` -- the behavioral `findmnt` assertions
  (`nosuid`/`nodev` present, `noexec` absent) pass against a live mount. Run
  before the code change to confirm it fails for the right reason, then after.
- `just docs-build` -- mdBook build + linkcheck (catches the SUMMARY entry, the
  ADR cross-links, and the new principles.md `[Why ->]` pointer to ADR-032).
- `just check-docs-see-paths` and `just check-output-ascii` -- See-path
  validation and ASCII guard over the touched `.rs` doc comment.
