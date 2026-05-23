# Plan: Document the scrub -> device-stats -> alert pipeline

## Context

A code review filed a "high severity" finding claiming braid's scrub
failure escalation pathway was incomplete -- that
`scrub_resume_or_start.rs` only propagates exit 3, and that
`doctor`/monitor don't surface scrub errors. The reviewer proposed a new
monitor task that polls `btrfs scrub status` and alerts on
`uncorrectable count > 0`.

The finding's code claim is wrong: the kernel hook at
`reference/linux/fs/btrfs/scrub.c:985-993` increments
`BTRFS_DEV_STAT_READ_ERRS / CORRUPTION_ERRS / GENERATION_ERRS` whenever
scrub discovers an error, and braid's monitor already polls
`btrfs device stats` every 5 minutes (`cli/src/monitor.rs:78-122`) and
latches `AlertCause::BtrfsDeviceErrors` for any counter above the acked
baseline (`cli/src/alert.rs:103-143`). The result drives the beeper via
`braid-monitor.service` exit 1 -> `braid-alert.service`
(`modules/braid/monitor.nix:122-141`). ADR 014's "rejected alternatives"
section already explains why a parallel probe is redundant
(`docs/design/decisions/014-alerts.md:152`).

The proposed code change would duplicate an existing data flow and
contradict an Active ADR. But the underlying signal -- that a careful
reviewer cannot trace scrub-to-alert in the docs -- is real. The
explanation only appears buried inside a rejected-alternative bullet,
and the operator-facing surfaces (alerts guide, monitor command page,
exit-3 propagation in Rust) never name scrub as a source. This plan
closes that documentation gap so the same finding does not recur.

## Approach

Four small additions, each at the narrowest authoritative surface for
its audience. No code behavior changes. Every prose citation to
`reference/...` source is an unlinked backtick code path -- not a
markdown link -- because `just check-docs` rejects links that escape
the `docs/` subtree (`justfile:238-244`) and existing ADRs (010, 016,
020) already follow this convention.

### 1. ADR 014 -- canonical authority

File: `docs/design/decisions/014-alerts.md`

Under the existing section **"All five btrfs device stat counters
trigger alerts"** (around line 37), add one short paragraph naming the
two sources that increment those counters and pointing at the kernel
hook for scrub. Suggested shape (final wording TBD during impl):

> Two kernel paths increment these counters: ordinary I/O (the lower
> block layer returns `EIO`/`EREMOTEIO` for read/write/flush errors)
> and scrub (`fs/btrfs/scrub.c:985-993` increments `READ_ERRS` on
> `init_nr_io_errors`, `CORRUPTION_ERRS` on `init_nr_csum_errors`, and
> `GENERATION_ERRS` on `init_nr_meta_gen_errors`). The monitor polls
> the same counters either way, so scrub-discovered uncorrectable
> errors reach the operator through the same `BtrfsDeviceErrors` cause
> and beep as everyday I/O errors -- no separate scrub-status probe is
> needed (and would be redundant; see Rejected alternatives).

Keep the existing rejected-alternative bullet at line 152 as-is; the
new paragraph is the positive statement that bullet implicitly relies
on.

### 2. Operator guide -- audience-facing trace

File: `docs/guides/monitoring-and-alerts.md`

Around the existing list at line 13 ("non-zero error counters (read,
write, flush, corruption, generation errors)"), add one sentence (or a
short parenthetical right after the list) noting that scrub is one of
the paths that produces those counters, so a monthly scrub that finds
unrepairable corruption will surface through the same beep / `braid
status` flow as an everyday read error. No reorganization; just enough
that an operator searching "scrub error" lands here.

### 3. Monitor command reference -- close the silent surface

File: `docs/commands/monitor.md`

The page already lists "btrfs device errors -- any device in the pool
has read, write, flush, corruption, or generation errors above the
acknowledged baseline" at line 33. Extend that bullet with a half
sentence naming scrub as one of the producers of those counters, so a
reviewer or operator reading the command reference sees the connection
without having to chase ADR 014. Keep it to one bullet edit -- no new
section, no new diagram.

### 4. Rust doc comment -- code-side breadcrumb

File: `cli/src/scrub_resume_or_start.rs`, the doc comment on
`cmd_scrub_resume_or_start` (lines 20-23).

Extend the existing two-line comment with a third line clarifying that
the `uncorrectable_errors: bool` (and the exit-3 propagation in
`main.rs:845-850`) is parity with btrfs's own exit convention -- it
makes the condition visible in `systemctl status braid-scrub.service`
-- and that the *primary* alert path is the monitor's `device stats`
poll, not this exit code. One line, references ADR 014 by number.

Skip a doc-comment edit on `main.rs:834-856` dispatch -- the new line
on `cmd_scrub_resume_or_start` is the right place; a second copy on
the dispatcher is duplication.

## Critical files

| File | Change |
| --- | --- |
| `docs/design/decisions/014-alerts.md` | Add one paragraph under "All five btrfs device stat counters trigger alerts" naming scrub + ordinary I/O as sources, citing the kernel scrub.c line range as an unlinked backtick code path. |
| `docs/guides/monitoring-and-alerts.md` | Add one sentence near the existing error-counter list noting scrub-discovered errors flow through the same channel. |
| `docs/commands/monitor.md` | Extend the line 33 "btrfs device errors" bullet with a half sentence naming scrub as one of the producers of those counters. |
| `cli/src/scrub_resume_or_start.rs` | Add one line to the existing doc comment on `cmd_scrub_resume_or_start` explaining exit-3 is parity-with-btrfs, not the primary alert path; reference ADR 014. |

Out of scope (decided with the user):

- `docs/design/decisions/018-systemd-lifecycle.md` -- alerts belong in
  ADR 014; a cross-ref here would be noise.
- `docs/internals/` -- no new file. ADR 014 plus the guide line is
  sufficient.
- Any change to `compute_alert_state`, monitor cadence, or the scrub
  service.

## Verification

This is docs-only; tests are not the right gate. The verification is
whether the four entry points a careful reviewer would land on now
make the scrub-to-alert connection traceable in one hop.

1. `nix develop .#docs -c just check-docs` passes. This is the gate
   that rejects markdown links escaping `docs/`; the new citations to
   `reference/linux/fs/btrfs/scrub.c` must be unlinked backtick code
   paths, matching the precedent set by ADR 010, 016, and 020.
2. `nix develop .#docs -c mdbook build docs` succeeds (mdbook-linkcheck
   on all internal cross-links).
3. Re-read each touched file directly (not via the rendered book for
   the Rust file, which is not part of mdBook) and confirm: opening
   from ADR 014, the alerts guide, the monitor command page, or
   `cli/src/scrub_resume_or_start.rs` each surface the same one-hop
   answer to "where do scrub-discovered errors go?".
4. `just test-rust` runs cleanly (no behavior change, but the
   doc-comment edit must still compile; the crate is `braid-cli`).
5. Grep sweep:
   `rg -n "BTRFS_DEV_STAT_CORRUPTION_ERRS|fs/btrfs/scrub\.c" docs/`
   should show one citation in ADR 014.
6. Verify the finding cannot recur: re-read the original finding text
   against the updated ADR 014 paragraph and confirm the prescription
   ("Add monitor task to periodically check scrub status...") is now
   visibly answered ("redundant; see Rejected alternatives, and the
   shared device-stats path").

No VM tests, no fixture refresh, no code paths exercised.
