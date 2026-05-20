# Document the systemd-lifecycle side effects of `braid lock` (and `braid unlock`) in the user manual

## Context

The May 2026 rust-owned-pool-operation-lock migration (commit ff6f766) added
three observable side effects to `braid lock` on NixOS module installs --
none of which are reflected in `manual/commands/lock.md`.

When `systemd_lifecycle = true`, `cmd_lock_impl` now:

1. Pre-step: stops `braid-scrub.timer`,
   `braid-scrub-resume-trigger.service`, `braid-scrub.service` silently
   (`cli/src/lock.rs:996-1021`, `run_lock_pre_steps`).
2. Pre-step: iterates `systemctl show -P BoundBy braid-online.service`
   and stops each non-scrub consumer (warning on nonzero exit). Empty
   unless the operator has wired `BindsTo=braid-online.service` onto a
   service of theirs (samba, nfs, ...).
3. Post-unmount: calls `mark_offline`
   (`cli/src/main.rs:1013` -> `cli/src/online_state.rs:294-313`), which
   runs `systemctl stop braid-online.service`.

Standalone CLI installs (lifecycle disabled) hit none of this; both
pre-step and `mark_offline` short-circuit on `!cfg.systemd_lifecycle()`.

`manual/commands/lock.md:33-42` still enumerates only the LUKS/btrfs
work. The mirror page `manual/commands/unlock.md:61-73` has the
symmetric gap: it documents the mount steps but not that
`mark_online` (`cli/src/main.rs:443, 602, 892`) activates
`braid-online.service`, which is the mechanism that pulls in any
consumer the operator has wired in with `WantedBy=braid-online.service`
(typically alongside `BindsTo=` and `After=`).

The decision doc covers the mechanism
(`docs/decisions/018-systemd-lifecycle.md:131-183`); the user manual
never connects it to the two commands an operator actually types.

**Intended outcome:** An operator reading
`manual/commands/{lock,unlock}.md` learns -- without leaving the page --
that on NixOS module installs, lock stops the scrub stack and
`braid-online.service` (and any consumers they have bound to it), and
unlock reverses that. No more "braid lock unexpectedly killed my samba"
without an answer in the manual.

## Scope

Five files change. No code, no tests.

| File                                          | Change                                                                  |
| --------------------------------------------- | ----------------------------------------------------------------------- |
| `manual/commands/lock.md`                     | Add an "On NixOS module installs" subsection to "What happens under the hood" enumerating the three lifecycle side effects. |
| `manual/commands/unlock.md`                   | Add an "On NixOS module installs" subsection noting that unlock activates `braid-online.service` after a successful mount, which starts every consumer wired in with `WantedBy=braid-online.service`. |
| `manual/guides/sharing-and-permissions.md`    | Add a "Binding shares to the pool lifecycle" subsection (under Samba, with a one-liner for NFS) teaching the full `wantedBy + bindsTo + after` triad on `braid-online.service` as the canonical way to wire SMB/NFS in. |
| `manual/guides/troubleshooting.md`            | Add a "SMB/NFS is inactive after `braid lock`" symptom entry explaining the cascade, pointing at `braid unlock` for the restart, and at the sharing guide for the missing `wantedBy` if a stop-only setup is leaving the consumer dead after unlock. |
| `docs/decisions/018-systemd-lifecycle.md`     | Fix the long-running-consumer contract at line 186 to name the full `WantedBy + BindsTo + After` triad (currently lists only `After + BindsTo`, which contradicts the manual and the scrub-consumer paragraph at line 184). Lock-step with the manual edits so the lifecycle authority and the user guide land consistent. |

Out of scope (deliberately):

- **A new `braid.shareIntegration` Nix option** that auto-wires SMB/NFS
  to braid-online. Teaching the operator to write a five-line
  `systemd.services.samba-smbd` override is cheaper than introducing a
  new option surface and reasoning about its interactions with
  `services.samba`'s upstream unit. Leave that decision for a separate
  ADR if demand emerges.

## Voice / style

Follow the existing "What happens under the hood" pattern of numbered
single-line steps. Existing pages don't carry any
lifecycle-conditional behavior, so the subsection is a new shape -- but
it can be small (three lines for lock, two for unlock) and stays under
the heading instead of becoming a sibling section.

Plain ASCII, double-hyphen `--`. Match existing lock.md's
parenthetical/clarifying style.

## Proposed text

### `manual/commands/lock.md`

After line 42 (the "If the pool is already unmounted..." sentence),
insert:

```markdown
### On NixOS module installs

When braid is installed via the NixOS module, `braid lock` also:

- Stops `braid-scrub.timer`, `braid-scrub-resume-trigger.service`, and
  `braid-scrub.service` before unmount.
- Stops any consumer wired into the pool lifecycle via
  `BindsTo=braid-online.service` (e.g. an SMB or NFS unit you set up
  that way -- see [Sharing and permissions](../guides/sharing-and-permissions.md))
  before unmount.
- Stops `braid-online.service` itself after a successful unmount.

`braid unlock` reverses the third step: it reactivates
`braid-online.service` after mount, which restarts every consumer that
is *also* `WantedBy` `braid-online.service` (the recommended setup --
see the sharing guide).

Standalone CLI installs (no NixOS module) skip all three -- there is
no `braid-online.service` or scrub unit to stop.
```

### `manual/commands/unlock.md`

After line 73 (the "If all mappers are already open..." sentence),
insert:

```markdown
### On NixOS module installs

After a successful mount, `braid unlock` activates
`braid-online.service`. Any unit you have wired into the pool lifecycle
with `WantedBy=braid-online.service` (e.g. an SMB or NFS unit -- see
[Sharing and permissions](../guides/sharing-and-permissions.md))
starts as part of that activation. `braid lock` stops them again on the
way down via the matching `BindsTo=braid-online.service`.

Standalone CLI installs (no NixOS module) skip this -- there is no
`braid-online.service` to activate.
```

### `manual/guides/sharing-and-permissions.md`

Insert a new H3 subsection at the end of the "Samba integration"
section (after the "Multiple shares" subsection at line 143, before
the "NFS" H2 at line 145). The subsection contains a `nix` code block,
so the literal text is shown below with `~~~markdown` outer fences to
avoid colliding with the inner backtick fence:

~~~markdown
### Binding shares to the pool lifecycle

By default, `samba-smbd.service` (the systemd unit NixOS creates from
`services.samba.enable`) keeps running after `braid lock`. If a client
is mid-transfer when you lock, `umount` blocks until the file handle is
released. Wire the share into the pool lifecycle so systemd starts
`samba-smbd` after `braid unlock` and stops it again before `braid
lock` runs `umount`:

```nix
systemd.services.samba-smbd = {
  wantedBy = [ "braid-online.service" ];
  bindsTo  = [ "braid-online.service" ];
  after    = [ "braid-online.service" ];
};
```

All three fields are load-bearing and do different jobs:

- `wantedBy` -- `samba-smbd` starts when `braid-online.service` starts
  (i.e. after `braid unlock`).
- `bindsTo` -- `samba-smbd` stops if `braid-online.service` stops or
  goes inactive (i.e. before `braid lock` runs `umount`).
- `after` -- ordering only, ensures `samba-smbd` is started/stopped on
  the correct side of `braid-online.service`.

`braid lock` walks `systemctl show -P BoundBy braid-online.service`
(the reverse of `BindsTo=`) and stops every consumer this way before
unmount. This is the same pattern braid's own scrub timer uses (see
`modules/braid/storage.nix`).

`braid doctor` also picks up active SMB connections as auto-suspend
inhibitors -- see [Power management](power-management.md).
~~~

Add a one-line NFS pointer at the end of the existing NFS section
(after line 158), before "Auto-suspend integration":

```markdown
The same `wantedBy` + `bindsTo` + `after` triad on `braid-online.service`
(see "Binding shares to the pool lifecycle" under Samba above) applies
to `nfs-server.service` if you want NFS to stop before `braid lock` runs
`umount` and start again after `braid unlock`.
```

### `manual/guides/troubleshooting.md`

Insert a new H2 entry after the existing "Scrub won't start" section
(after line 169, before the "Related" section at line 171):

```markdown
## SMB/NFS service inactive after `braid lock`

**Symptom:** `systemctl status samba-smbd.service` (or
`nfs-server.service`) shows `inactive (dead)` immediately after you ran
`braid lock`.

This is intentional. On NixOS module installs, `braid lock` stops every
service bound to `braid-online.service` via `BindsTo=braid-online.service`
before it unmounts the pool. The cascade prevents busy-mount unmount
failures.

**Fix:** Run `braid unlock`. It reactivates `braid-online.service`
after mount, and systemd restarts every consumer that is also
`WantedBy` `braid-online.service`.

If the service does not restart on `braid unlock`, it is wired for the
stop side (`BindsTo`) but not the start side (`WantedBy`). The
recommended setup attaches all three (`wantedBy` + `bindsTo` + `after`)
-- see
[Binding shares to the pool lifecycle](sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle).
```

### `docs/decisions/018-systemd-lifecycle.md`

Replace the existing "Long-running services" paragraph at line 186:

> **Long-running services holding open files** (samba, nfs): Must
> additionally use `After=braid-online.service` +
> `BindsTo=braid-online.service`. This ensures systemd stops them
> *before* `braid lock` runs `ExecStop`, preventing unmount failures
> from busy filesystems. Rust dispatch iterates
> `BoundBy braid-online.service` and stops these consumers before
> unmount, mirroring the cascade systemd performs on shutdown for
> user-initiated lock.

with:

```markdown
**Long-running services holding open files** (samba, nfs): Use the full
`WantedBy=braid-online.service` + `BindsTo=braid-online.service` +
`After=braid-online.service` triad (same shape as the scrub timer
above). `BindsTo` + `After` ensures systemd stops them *before* `braid
lock` runs `ExecStop`, preventing unmount failures from busy
filesystems; `WantedBy` ensures they restart automatically when `braid
unlock` reactivates `braid-online.service`. Rust dispatch iterates
`BoundBy braid-online.service` and stops these consumers before unmount,
mirroring the cascade systemd performs on shutdown for user-initiated
lock. See `manual/guides/sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle`
for the user-facing example.
```

Do not touch the ADR's `Status: ...` header or any other paragraph.
This is a surgical one-paragraph correction.

> Implementer notes (do not commit these into the manual):
>
> 1. `systemd_lifecycle` in the config JSON is the internal flag the
>    Rust dispatch reads. It is set to `true` unconditionally by
>    `modules/braid/cli.nix:17` when the NixOS module is enabled and is
>    not exposed as a `braid.*` option. The condition the manual frames
>    is simply "did I install braid via the NixOS module?".
>
> 2. The systemd dependency triad must be `wantedBy + bindsTo + after`.
>    `BindsTo` alone is *stop/condition* coupling -- a consumer with only
>    `BindsTo + After` would not auto-start when `braid-online.service`
>    activates on `braid unlock`. Start propagation requires `WantedBy`
>    (or `Wants` from the other side). This is the same triad
>    `modules/braid/storage.nix:65-67` uses for the scrub timer, and the
>    same shape the bound-consumer VM test
>    (`tests/module/lock-stops-bound-consumers.nix:61-65`) verifies.
>    See `reference/systemd/man/systemd.unit.xml` `BindsTo=` /
>    `Wants=` entries for the upstream semantics.
>
> 3. The NixOS systemd unit name for the SMB file daemon under
>    `services.samba.enable = true` is `samba-smbd.service`, not
>    `smbd.service`. Verified by the existing braid Samba test
>    (`tests/samba.py:28-29`), which `restart`s and `wait_for_unit`s
>    `samba-smbd`. The example must be `systemd.services.samba-smbd`,
>    and any `systemctl ...` command in proposed text must reference
>    `samba-smbd`. "smbd" can stand in for the daemon binary in prose
>    where no unit lookup is implied, but every concrete unit reference
>    in this plan should use `samba-smbd`.

## Critical files to read before editing

- `manual/commands/lock.md:33-54` -- existing "What happens under the
  hood", "Error handling", "Related commands".
- `manual/commands/unlock.md:61-95` -- existing "What happens under the
  hood", "Degraded mode", "Safety checks / refusal cases", "Related
  commands".
- `manual/guides/sharing-and-permissions.md:73-158` -- existing Samba
  and NFS sections; the new subsection slots between "Multiple shares"
  and "NFS", and the NFS pointer goes after the example.
- `manual/guides/troubleshooting.md:154-169` -- existing
  "Scrub won't start" symptom entry; mirrors the desired voice for the
  new SMB/NFS entry.
- `docs/decisions/018-systemd-lifecycle.md:178-186` -- the "Consumer
  dependency contracts" section. Line 184 already names the scrub
  triad correctly; line 186 (long-running consumers) is the one
  paragraph the plan rewrites. Cross-check the Status header above this
  section is left untouched.
- `cli/src/lock.rs:996-1050` -- `run_lock_pre_steps`,
  `stop_unit_silent`, `stop_unit_warn_on_error` (confirms which units
  are stopped and in what order).
- `cli/src/online_state.rs:294-313` -- `mark_offline` (post-success
  stop of `braid-online.service`).
- `cli/src/main.rs:1005-1014` -- the plain-`braid lock` call site that
  wires `cmd_lock` + `mark_offline`.
- `cli/src/main.rs:600-603` -- the `braid unlock` call site that wires
  `mark_online`.
- `modules/braid/cli.nix:17` -- proves `systemd_lifecycle` is wired
  unconditionally when the module is enabled, so "did I install via
  the NixOS module?" is the correct user-facing condition.
- `modules/braid/storage.nix:63-73` -- canonical example of the
  `wantedBy + bindsTo + after` triad on `braid-online.service` (the
  scrub timer); the new sharing-and-permissions example mirrors this.
- `tests/module/lock-stops-bound-consumers.nix:61-66` -- the
  dummy-pool-consumer VM fixture that exercises the same triad and is
  the executable specification for the cascade the manual now teaches.
- `tests/samba.py:28-29` -- existing braid Samba test; pinning evidence
  for the NixOS unit name `samba-smbd.service` (not `smbd.service`)
  under `services.samba.enable`. Every concrete unit reference in the
  new manual/troubleshooting/smoke-test text must match this.
- `reference/systemd/man/systemd.unit.xml` (search for
  `<term><varname>BindsTo=` and `<term><varname>Wants=`) -- upstream
  reference confirming `BindsTo` is stop/condition-side only and
  `WantedBy`/`Wants` is what propagates start.

## Verification

No automated tests cover the manual; verification is by reading.

1. Render and read each touched page (run `just manual-serve` if the
   recipe exists -- otherwise open the source `.md` files directly):
   - `manual/commands/lock.md`
   - `manual/commands/unlock.md`
   - `manual/guides/sharing-and-permissions.md`
   - `manual/guides/troubleshooting.md`
   - `docs/decisions/018-systemd-lifecycle.md` (the rewritten paragraph
     should read in the same voice as the surrounding "Consumer
     dependency contracts" paragraphs and reach the same conclusion as
     the new manual subsection).
2. Voice / style check: each new subsection should read like its
   neighbors -- numbered or bulleted lists with short lines, no
   decision-doc prose. Compare against the existing "Scrub won't
   start" troubleshooting entry and the existing "Multiple shares"
   Samba subsection.
3. Internal-link sanity:
   - From `unlock.md`, the link
     `../guides/sharing-and-permissions.md` resolves.
   - From `troubleshooting.md`, the anchor
     `sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle`
     resolves to the new subsection.
4. Cross-check the wording against the implementation:
   - `git grep -n 'mark_offline\|run_lock_pre_steps' cli/src/` matches
     the three side effects enumerated in the lock.md subsection.
   - `grep -n 'systemd_lifecycle = true' modules/braid/cli.nix`
     confirms the gating is "is the NixOS module enabled" with no
     user-facing option to toggle, so the new subsections frame the
     condition correctly.
   - `grep -nE 'wantedBy|bindsTo|after' modules/braid/storage.nix` and
     `grep -nE 'wantedBy|bindsTo|after' tests/module/lock-stops-bound-consumers.nix`
     match the triad the new sharing-and-permissions example teaches.
   - `grep -nE 'WantedBy|BindsTo|After' docs/decisions/018-systemd-lifecycle.md`
     shows the long-running-consumer paragraph (line 186) now names the
     full triad, matching the scrub paragraph at line 184 and the
     manual.
5. Optional smoke test on a NixOS VM with the full triad wired per the
   new sharing-and-permissions example:
   - `sudo braid unlock`, then `systemctl status samba-smbd` -- should
     show `active` (proves the `wantedBy` start propagation).
   - `sudo braid lock`, then `systemctl status samba-smbd` -- should
     show `inactive` (proves the `bindsTo` stop cascade).
   This is the cascade the manual now promises end-to-end. The braid
   Samba test (`tests/samba.py:28-29`) is the existing reference for
   the unit name on the pinned nixpkgs.

## Risks and edge cases

- **NFS over-promise.** The Samba example targets
  `samba-smbd.service`, the unit NixOS's `services.samba.enable`
  creates and the one braid's own `tests/samba.py` exercises. NFS users
  may see `nfs-server.service`, `nfs.service`, or
  `nfs-kernel-server.service` depending on distro/module. The plan
  keeps the NFS pointer one-liner and tells the user to apply the same
  triad pattern to *their* unit name -- no copy-paste promise.
- **Stop-only legacy wiring.** Existing users who followed the
  *pre-correction* ADR text literally have `BindsTo + After` only (no
  `WantedBy`). Their `braid lock` cascade still works -- but on
  `braid unlock` they will have to start the service manually. The
  troubleshooting entry covers this case explicitly and points them at
  the sharing guide for the missing `wantedBy`. The ADR correction in
  this PR is what eliminates the source of the legacy wiring; no code
  regression.
- **Pre-existing `wantedBy + bindsTo + after` users.** No regression
  risk -- the docs only describe behavior that already exists. Users
  who already wired the full triad keep working.
- **Search-index lag.** `manual/book/searchindex.js` is a build
  artifact; do not hand-edit. Verify it rebuilds when the manual is
  next built but treat it as out of scope for this PR.
