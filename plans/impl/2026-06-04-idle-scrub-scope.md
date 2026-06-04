# Plan: document the `braid idle` host-wide vs pool-scoped asymmetry

## Context

`braid idle` is the autosuspend activity gate. It runs two probes with two
different scopes, and that asymmetry is currently undocumented:

- **Exclusive-op scan -- host-wide.** `cmd_idle` calls
  `preflight::check_any_btrfs_exclusive_op`, which walks every entry under
  `/sys/fs/btrfs/*` and reports busy if *any* btrfs filesystem on the host has
  an active exclusive op (balance, device add/remove/replace, resize, swap
  activate). Confirmed at `cli/src/idle.rs#cmd_idle` and
  `cli/src/preflight.rs#check_any_btrfs_exclusive_op`; pinned by the test
  `cli/src/idle.rs#idle_any_busy_blocks_suspend_multi_btrfs`.
- **Scrub probe -- pool-scoped.** `cmd_idle` runs `btrfs scrub status` against
  only the configured pool mount point (`CmdRequest::BtrfsScrubStatus {
  mount_point }`). A scrub on a *non-pool* btrfs (e.g. a btrfs root) is invisible
  to `braid idle` and does **not** block suspend.

Both `docs/commands/idle.md` ("What happens under the hood" + multi-btrfs note)
and `docs/design/decisions/016-auto-suspend.md` (the "Exclusive-op probe scans
`/sys/fs/btrfs/*` directly" section) go out of their way to explain the
host-wide exclusive-op behavior and frame it as "intentionally conservative" /
"err conservative" -- but neither names the scrub probe's narrower scope. A
reader reasonably infers scrub is also host-wide. This is a real behavioral
asymmetry (not a bug); the docs should name it.

The code is correct and should not change. Making scrub host-wide would mean
spawning a `btrfs scrub status` per filesystem on every autosuspend tick for
coverage braid does not own, and it would contradict the ownership boundary ADR
016 already set for the suspend gate. So this is a docs-only fix.

## Scope decision (why only 2 files)

Authoritative-home fix: edit **only** the two surfaces that already state the
detailed host-wide claim -- `docs/commands/idle.md` and ADR 016. Deliberately
leave the two guides untouched:

- `docs/guides/power-management.md` ("What counts as activity" table) and
  `docs/guides/nixos-configuration.md` (activity-checks list) are brief *and
  already accurate* -- both attribute the `/sys/fs/btrfs/<fsid>/...` host-wide
  path to the *exclusive operation* only ("the latter via ..." / "any btrfs
  kernel exclusive operation"). Neither claims scrub is host-wide.
- braid's documented doctrine puts the fix here: deep rationale lives in
  `docs/design/decisions/` (AGENTS.md "Architecture Authority"), end-user
  behavior in `docs/commands/`, and guides stay brief. The repo even ships a
  `docs-consolidation` agent whose job is to flag "a page restating its
  governing ADR" and duplicated facts -- adding the scope nuance to the guides
  is exactly that anti-pattern.

## Changes

### 1. `docs/commands/idle.md`

**a. Make the scope visible in the procedure (step 5).** Step 3 already says the
exclusive-op scan covers "any btrfs filesystem"; mirror that by naming the
scrub probe's scope so the asymmetry is self-evident from the numbered list.

- Current step 5: `Probes scrub status via `btrfs scrub status` only after the
  sysfs scan is clean (scrub is not in the kernel exclusive-operation set, so
  sysfs cannot detect it)`
- New step 5: insert "against the configured pool mount point," so it reads
  `Probes scrub status via `btrfs scrub status` against the configured pool
  mount point, only after the sysfs scan is clean (scrub is not in the kernel
  exclusive-operation set, so sysfs cannot detect it)`

**b. Extend the multi-btrfs note** (the paragraph beginning "When the host has
more than one btrfs filesystem ...") with one contrasting sentence after the
existing "intentionally conservative" line:

> Scrub detection is narrower: `braid idle` only checks for a scrub on the
> braid pool itself, so a scrub running on a non-pool btrfs (e.g. the btrfs
> root) is not detected and does not block suspend.

Keep the existing fragment-less `[ADR 016: Auto-Suspend](../design/decisions/016-auto-suspend.md)`
link as-is. It now correctly covers both behaviors the paragraph describes, so a
deep-link to one subsection would be wrong; leaving it whole-file also avoids any
linkcheck slug fragility.

### 2. `docs/design/decisions/016-auto-suspend.md`

Add a new sibling subsection immediately after the existing
"### Exclusive-op probe scans `/sys/fs/btrfs/*` directly" section (i.e. after the
`probe::probe_fsid` paragraph, before "### SSH always on, SMB/NFS
auto-detected"). Parallel structure: one named subsection per probe, each
stating scope + rationale.

> ### Scrub probe is scoped to the pool mount point
>
> Unlike the exclusive-op scan, the scrub probe is not host-wide: `cmd_idle`
> runs `btrfs scrub status` against only the configured pool mount point. A
> scrub on a non-pool btrfs (e.g. the btrfs root) is therefore not detected and
> does not block suspend.
>
> This asymmetry is intentional. braid's autosuspend gate protects the braid
> pool, not every btrfs on the host -- the same ownership boundary that scopes
> `braid wol-ready` to braid's suspend path rather than installing a universal
> `sleep.target` gate. The exclusive-op scan is broader only because one pass
> over `/sys/fs/btrfs/*` reads every filesystem's state for free and errs
> conservative; matching that breadth for scrub would mean spawning a `btrfs
> scrub status` subprocess per filesystem on every autosuspend tick, for
> coverage braid does not own.

Leave the ADR's intro mention (the "checks for an in-flight scrub plus any
kernel exclusive operation" line in the Decision section) unchanged -- the new
subsection is the single authoritative home for the scope detail.

## Conventions to honor

- ASCII `--`, not em-dash; plain ASCII quotes. New text already uses `--`,
  matching surrounding prose (global writing-style rule + AGENTS.md CLI-output
  rule).
- No file-by-line-number references in prose (AGENTS.md "File References"). The
  new ADR text refers to the `wol-ready` / `sleep.target` decision by concept,
  not line number.
- ADR 016 is `status: Active`, so editing its body is permitted and required to
  keep behavior docs in sync (AGENTS.md "Decision Doc References" freeze applies
  only to Superseded/Deprecated docs).
- The new ADR heading "Scrub probe is scoped to the pool mount point" has no
  special characters, so its slug is the predictable
  `scrub-probe-is-scoped-to-the-pool-mount-point`; no existing link needs it,
  but it is safe if a future doc wants to deep-link.

## Verification

- `just check-docs` -- SUMMARY parity and doc-table checks (unaffected: no new
  files, no SUMMARY/table changes; run as a guard).
- `nix develop .#docs -c mdbook build docs` -- runs `mdbook-linkcheck2`.
  Confirms the unchanged ADR link still resolves and the new heading does not
  break any in-book target.
- Manual read of the rendered `idle.md` "What happens under the hood" section
  and ADR 016 to confirm step 3 (host-wide) and step 5 (pool-scoped) now read as
  a deliberate contrast, and the two ADR subsections sit parallel.
- No Rust/VM tests are affected (docs-only; behavior unchanged). Do **not** run
  the VM suite for this change.
