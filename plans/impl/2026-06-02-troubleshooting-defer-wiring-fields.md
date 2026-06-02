# Plan: defer the consumer-wiring field set in troubleshooting.md to the sharing guide

## Context

A follow-up from the `lock.md` consumer-wiring edit
(`plans/impl/2026-06-02-lock-consumer-wiring-precision.md`) flagged a pre-existing
disagreement between two guides on the systemd directives that wire an SMB/NFS
consumer into the braid pool lifecycle:

- `docs/guides/troubleshooting.md:301` says the recommended setup attaches
  "all three (`wantedBy` + `bindsTo` + `after`)".
- `docs/guides/sharing-and-permissions.md:167` says "All four fields are
  load-bearing" and enumerates `wantedBy`, `bindsTo`, `after`, **and**
  `ConditionPathIsMountPoint`.

Four is the correct count -- `ConditionPathIsMountPoint` is genuinely
load-bearing (the sharing guide's boot-edge explanation at
`sharing-and-permissions.md:176` shows NixOS starts Samba at boot via
`samba.target` before any unlock, and only the condition stops `smbd` from
serving the empty, offline mount directory), and ADR 018
(`docs/design/decisions/018-systemd-lifecycle.md`) agrees (it calls
`WantedBy` + `BindsTo` + `After` the "triad" plus `ConditionPathIsMountPoint`).
So `sharing-and-permissions.md` is right and is the single source of truth for
the field set.

The root cause of the disagreement is **duplication**: the four-field
enumeration is restated on the troubleshooting page, and the copies drifted
(three vs four). Syncing the troubleshooting copy to a verbatim third instance
of the list would fix today's symptom but reset the drift clock -- the next
person who changes the field set updates the sharing guide and forgets this page
again.

Intended outcome: the troubleshooting page stops restating the field set and
defers to the sharing guide (the single source of truth), so the two pages can
never disagree again. This matches the in-repo precedent -- `mounting-subvolumes.md:156`
defers ("The full triad pattern is the same lifecycle shape described in
[Sharing and permissions]") rather than restating the fields -- and the File
References anti-drift philosophy in `AGENTS.md`.

## Scope

**Edit:** `docs/guides/troubleshooting.md` only (one sentence).

**Confirmed leave-alone (full repo sweep):**

- `docs/guides/sharing-and-permissions.md` -- correct and authoritative; the
  single source of truth we defer to ("All four fields", and the exact
  `wantedBy` + `bindsTo` + `after` + `ConditionPathIsMountPoint` enumeration at
  line 193). Untouched.
- `docs/design/decisions/018-systemd-lifecycle.md` -- correct ("triad" plus
  `ConditionPathIsMountPoint`); terminology differs but the count is right.
- `docs/guides/mounting-subvolumes.md` -- already defers to the sharing guide
  for the field set; no standalone count. It is the precedent this plan follows.
- `docs/commands/lock.md` -- "wire both" refers to the `BindsTo`/`WantedBy`
  start/stop halves and defers to the sharing guide; the "skip all three" at
  line 54 counts the three module-only `braid lock` actions (the bullets at
  lines 48-50: scrub units, lifecycle consumers, `braid-online.service`
  itself), not lifecycle directives. No conflict.
- `docs/commands/unlock.md` -- mentions both halves, states no count.

## The change

File: `docs/guides/troubleshooting.md`, under
`## SMB/NFS service inactive after `braid lock``.

Before (line 301):

```
If the service does not restart on `braid unlock`, it is wired for the stop side (`BindsTo`) but not the start side (`WantedBy`). The recommended setup attaches all three (`wantedBy` + `bindsTo` + `after`) -- see [Binding shares to the pool lifecycle](sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle).
```

After:

```
If the service does not restart on `braid unlock`, it is wired for the stop side (`BindsTo`) but not the start side (`WantedBy`). The recommended setup wires the share into the full pool lifecycle -- see [Binding shares to the pool lifecycle](sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle).
```

The first sentence is unchanged -- it carries the page's diagnostic value
(`BindsTo` present, `WantedBy` missing). The second sentence drops both the
count word and the inline field enumeration, replacing them with "wires the
share into the full pool lifecycle" and leaving the existing link to point at
the full setup. The `[Binding shares to the pool lifecycle](...)` link is
preserved verbatim (no linkcheck impact).

## Style / invariant checks

- ASCII only; keeps the existing `--` (double hyphen) before the link.
- No new Markdown link introduced; the existing anchor link is untouched.
- No line-number cross-references introduced; the defer is a `path#anchor`
  link, the drift-proof form `AGENTS.md` prescribes.
- Removes the duplicated field list rather than syncing it. There is no count
  or enumeration left on this page to drift; the sharing guide remains the only
  place that states the fields, matching the `mounting-subvolumes.md:156`
  precedent.

## Verification

1. `mdbook build docs` -- confirms `mdbook-linkcheck2` passes (the anchor link
   is unchanged; no new link is introduced).
2. Visual read: the troubleshooting "Fix" paragraph keeps its `BindsTo`/`WantedBy`
   diagnosis and now defers the full setup to the sharing guide with no count or
   field list of its own.
3. `rg "all (three|four)" docs/guides/troubleshooting.md` returns nothing for
   this entry -- confirms the drift-prone count is gone.
4. No Rust/Nix/test changes -> no `just test-*` runs required.
