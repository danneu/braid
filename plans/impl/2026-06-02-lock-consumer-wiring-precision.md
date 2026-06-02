# Plan: tighten the lock.md systemd-consumer wiring summary

## Context

A review finding (Medium / Accuracy) flagged `docs/commands/lock.md:49,52` for
inconsistently naming the systemd directives that wire SMB/NFS consumers into
the pool lifecycle, claiming it "overstates what `WantedBy` alone does."

Verification (see `/verify-issue` output earlier this session) found the doc is
**technically accurate** -- stop-before-umount really does come from `BindsTo`
(lock walks its reverse, `BoundBy`) and restart-after-unlock really does come
from `WantedBy`. The finding's headline ("inaccurate / overstates `WantedBy`")
does not hold, and its proposed line-52 rewrite is verbatim the current text.

The genuine, narrow gap is a **precision + skimmability** one:

- Line 49 frames a consumer as wired "via `BindsTo=`" as if that single
  directive is the wiring. A working consumer needs **both** `BindsTo`
  (stop) and `WantedBy` (restart); a consumer with only one half-works.
- Line 49 omits the `BoundBy` mechanism that the more-precise sibling docs all
  state (`sharing-and-permissions.md`, `mounting-subvolumes.md`, ADR 018).
- A reader who lands on the lock bullet alone (e.g. from search) shouldn't have
  to click through to the guide to avoid the half-work trap.

Intended outcome: make `lock.md` self-contained against the half-work trap and
consistent with the half-work-trap framing `docs/guides/troubleshooting.md:301`
already nails -- while staying terse, asserting no directive count, and not
duplicating the authoritative setup in the sharing guide.

## Code grounding (already verified, no code change)

- Stop side: `cli/src/lock.rs#run_lock_pre_steps` calls
  `online_ops.list_bound_by("braid-online.service")`
  (`cli/src/online_state.rs#list_bound_by`, which runs
  `systemctl show -P BoundBy`). `BoundBy` is the reverse of consumers'
  `BindsTo=`.
- Restart side: `cli/src/online_state.rs#mark_online` calls
  `systemctl_start(BRAID_ONLINE_UNIT)`; starting `braid-online.service` pulls up
  its `Wants`, the reverse of consumers' `WantedBy=`.

This is a docs-only change. No Rust, Nix, or test code is touched.

## Scope

**Edit:** `docs/commands/lock.md` only (two lines).

**Do NOT touch (confirmed in/out by `rg` sweep of the repo):**

- `docs/commands/unlock.md` -- the deliberate mirror; already pairs both
  directives ("wired ... with `WantedBy=`" + "matching `BindsTo=`"). Leave it.
- `docs/guides/troubleshooting.md` -- already states the half-work failure mode
  and the triad; it is the model to match, not a target.
- `docs/guides/sharing-and-permissions.md`, `docs/guides/mounting-subvolumes.md`,
  `docs/design/decisions/018-systemd-lifecycle.md`, `026-pool-lock-rust-owned.md`
  -- authoritative / rationale layer; accurate and detailed. Leave them.
- `README.md` -- no `BindsTo`/`WantedBy`/`braid-online` mention; no sync needed.
- `plans/impl/2026-05-20-document-lifecycle-lock-unlock.md` -- historical record,
  not live docs.

## The change

File: `docs/commands/lock.md`, in the "### On NixOS module installs" section.

### Line 49 -- add the `BoundBy` mechanism (precision)

Before:

```
- Stops any consumer wired into the pool lifecycle via `BindsTo=braid-online.service` (e.g. an SMB or NFS unit you set up that way -- see [Sharing and permissions](../guides/sharing-and-permissions.md)) before unmount.
```

After:

```
- Stops any consumer wired into the pool lifecycle via `BindsTo=braid-online.service` (lock walks its reverse, `BoundBy`; e.g. an SMB or NFS unit you set up that way -- see [Sharing and permissions](../guides/sharing-and-permissions.md)) before unmount.
```

Only change: insert `lock walks its reverse, ` + "`BoundBy`; " at the front of
the existing parenthetical. The `[Sharing and permissions](../guides/sharing-and-permissions.md)`
Markdown link is preserved verbatim (no linkcheck impact).

### Line 52 -- make the both-directives requirement explicit (skimmability)

Before:

```
`braid unlock` reverses the third step: it reactivates `braid-online.service` after mount, which restarts every consumer that is also `WantedBy=braid-online.service` (the recommended setup -- see the sharing guide).
```

After:

```
`braid unlock` reverses the third step: it reactivates `braid-online.service` after mount, which restarts every consumer that is also `WantedBy=braid-online.service`. A consumer wired with only one of the two half-works -- `BindsTo` stops it before lock, `WantedBy` restarts it after unlock -- so wire both; the sharing guide shows the full setup.
```

The first clause (correct `WantedBy` attribution) is unchanged. The trailing
`(the recommended setup -- see the sharing guide)` is replaced by one descriptive
sentence: it names the half-work trap and each directive's role, asserts **no
field count**, and defers completeness with "the sharing guide shows the full
setup" -- so a skim-reader cannot mistake the two named directives for the
complete recipe (the boot-edge guard `ConditionPathIsMountPoint` and ordering
`after` live in the guide; see the count note under Style). No new Markdown link
is introduced on this line (the prose pointer was already plain text), so
linkcheck is unaffected.

## Style / invariant checks

- ASCII only; uses `--` (double hyphen), not em-dash -- per project CLI/docs
  style.
- Terse; defers the full setup (`wantedBy` + `bindsTo` + `after` +
  `ConditionPathIsMountPoint` -- four load-bearing fields per the sharing guide)
  and the code example to that guide rather than duplicating it. Matches the
  altitude of `troubleshooting.md:301`.
- Asserts **no directive count** on line 52, deliberately. The docs already
  disagree: `troubleshooting.md:301` says "all three (`wantedBy` + `bindsTo` +
  `after`)" while `sharing-and-permissions.md:167` says "All four fields are
  load-bearing" (it adds `ConditionPathIsMountPoint`). Reconciling that
  pre-existing discrepancy is out of scope (both are leave-alone targets); line 52
  sidesteps it by describing the two-directive half-work trap and pointing to "the
  full setup" rather than stating a total -- keeping lock.md from becoming a third,
  smaller count in an already-inconsistent set.
- No line-number cross-references introduced; the existing `path#` /
  `[text](path)` link forms are preserved.

## Verification

1. `mdbook build docs` -- confirms `mdbook-linkcheck2` passes (the
   `../guides/sharing-and-permissions.md` link on line 49 is unchanged; line 52
   introduces no link). A broken cross-link fails this build.
2. Visual read of the rendered "On NixOS module installs" section: confirm the
   half-work trap and each directive's role are stated inline without a
   click-through, that line 52 asserts no field count and points onward with "the
   full setup", and that lock.md now reads consistently with `unlock.md` (mirror)
   and the failure-mode framing in `troubleshooting.md`.
3. No Rust/Nix/test changes -> no `just test-*` runs required for this change.

## Follow Up

- Pre-existing field-count disagreement between guides: `docs/guides/troubleshooting.md:301`
  says the recommended setup attaches "all three (`wantedBy` + `bindsTo` + `after`)",
  while `docs/guides/sharing-and-permissions.md:167` says "All four fields are
  load-bearing" (it also lists `ConditionPathIsMountPoint`). Reconcile to one count
  (four is correct -- `ConditionPathIsMountPoint` is load-bearing per the sharing
  guide's own boot-edge explanation). Out of scope for this lock.md change.
