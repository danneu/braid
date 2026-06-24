# Plan: document that `braid ack` stops the Warning advisory unit too

## Context

`braid ack` silences alerts by stopping **two** systemd units:

- `braid-alert.service` -- the Critical beeper, which cascades via `BindsTo`
  to `braid-beep.service` (started on monitor exit 1, the SMART hook, and the
  scrub-failed hook).
- `braid-alert-advisory.service` -- the non-beeping Warning advisory, a
  `oneshot` + `RemainAfterExit` unit started on monitor exit 3 (the proactive
  ENOSPC/capacity path). Its repeated 5-minute `systemctl start` is a no-op
  until `braid ack` stops it.

The code is correct: `stop_beeper` in `cli/src/ack.rs` stops both units, and its
doc comment already says so ("Stops both alert units ... so one ack silences
whichever tier the last monitor cycle started"). This was introduced by commit
`cf28ce7f` (feat(monitor): raise proactive ENOSPC risk as a non-beeping warning),
which added both the advisory unit and the second stop call.

That commit did **not** propagate the advisory mention into the two end-user docs
that enumerate what ack stops. They still describe ack as stopping only
`braid-alert.service`. For an ENOSPC Warning ack, `braid-alert.service` may never
have started -- the only active unit is the advisory -- so a reader reconciling
"why did the advisory stop firing `alertCommand`" against the docs finds no
mention of it. In `monitoring-and-alerts.md` it is also a self-contradiction: the
same diagram shows the advisory on the *start* side but omits it on the *stop*
side.

Outcome: the two divergent end-user docs match the code and the already-correct
sibling docs.

## Scope

Documentation-only correction: two Markdown pages plus one Rust `///` doc comment.
The Rust edit is comment-only -- it changes no runtime behavior -- so there are
**no** behavior, test, or `README.md` changes, and no new tests are required. The
behavior is already pinned by the VM test `tests/cli/braid-monitor-enospc.py`,
which drives the real systemd advisory path: it asserts the ENOSPC Warning starts
`braid-alert-advisory.service` but not `braid-alert.service` (lines 73-74) and
that `braid ack` stops the advisory (line 96). It is registered in `flake.nix`
(`braid-monitor-enospc`).

A completeness sweep of the whole doc tree + `cli/src/**` confirmed exactly these
locations describe what ack stops:

- Already correct (mirror their wording): `cli/src/ack.rs` `stop_beeper` doc
  comment ("Stops both alert units ... so one ack silences whichever tier the last
  monitor cycle started"); `docs/commands/monitor.md` ("Alert pipeline" section)
  -- "The same ack also stops `braid-alert-advisory.service`, so a Warning-tier
  advisory is silenced too."
- Generic, intentionally no unit enumeration (leave as-is): `README.md`,
  `docs/index.md`, `docs/commands/ack.md` intro, `docs/commands/status.md`,
  `docs/commands/tui.md`, `docs/guides/troubleshooting.md`,
  `docs/guides/day-to-day-nas-usage.md`.
- Wrong (omit the advisory) -- the three edits below: two end-user Markdown pages
  plus one stale source comment, `cli/src/ack.rs#cleanup_alert_files_and_beeper`.
  (The reviewer caught this third one; the sweep's first pass classified
  `cli/src/ack.rs` by its correct `stop_beeper` comment and missed the stale
  sibling comment in the same file.)

## Changes

### 1. `docs/commands/ack.md` -- "What happens under the hood" step 3

Replace the current step 3:

> 3. Stops `braid-alert.service`, best-effort. That cascades through `BindsTo` to
>    stop the `braid-beep.service` loop when beeping is enabled. This runs first
>    so the stop attempt is reached before any later file-removal I/O error can
>    short-circuit the rest of cleanup.

with:

> 3. Stops both alert units, best-effort: `braid-alert.service` (the Critical
>    beeper -- that stop cascades through `BindsTo` to the `braid-beep.service`
>    loop when beeping is enabled) and `braid-alert-advisory.service` (the
>    non-beeping Warning advisory started on the proactive ENOSPC/capacity path).
>    One ack silences whichever tier the last monitor cycle started. This runs
>    first so the stop attempt is reached before any later file-removal I/O error
>    can short-circuit the rest of cleanup.

Rationale: preserves every existing detail (best-effort, the `BindsTo` ->
`braid-beep.service` cascade, the "runs first" ordering note), adds the advisory
unit and when it is started, and mirrors the `stop_beeper` doc comment's
"whichever tier the last monitor cycle started." The ordering note stays accurate:
`stop_beeper` stops *both* units first in `cleanup_alert_files_and_beeper`, before
any `remove_*`.

### 2. `docs/guides/monitoring-and-alerts.md` -- "How the pieces fit together" diagram

In the final `braid ack` box (currently):

```
braid ack
  -> clears alert state
  -> stops braid-alert.service
    -> cascades to stop braid-beep.service
```

add the advisory as a sibling stop, matching the two-tier start side of the same
diagram (`on exit 1: start braid-alert.service` / `on exit 3: start
braid-alert-advisory.service`):

```
braid ack
  -> clears alert state
  -> stops braid-alert.service
    -> cascades to stop braid-beep.service
  -> stops braid-alert-advisory.service (Warning/ENOSPC tier, no beep)
```

Keep the ASCII `->` arrows and indentation already used by the diagram.

### 3. `cli/src/ack.rs` -- `cleanup_alert_files_and_beeper` doc comment

The helper's doc comment still describes the production beeper stop as a single
unit, contradicting the `stop_beeper` it calls (which stops both). Replace:

> /// The beeper stop is best-effort: production issues `systemctl stop
> /// braid-alert.service`, logs a warning when spawning `systemctl` fails or it
> /// exits non-zero, and returns no error to cleanup. The ordering guarantees the
> /// hook is invoked on every cleanup call, not that the audible alert was
> /// silenced.

with wording that names both units:

> /// The beeper stop is best-effort: production issues `systemctl stop` for both
> /// alert units (`braid-alert.service` and `braid-alert-advisory.service`), logs
> /// a warning when spawning `systemctl` fails or a unit exits non-zero, and
> /// returns no error to cleanup. The ordering guarantees the hook is invoked on
> /// every cleanup call, not that the audible alert was silenced.

Comment-only edit -- no behavior change. The "logs a warning ... when a unit
exits non-zero" wording matches `stop_unit`, which checks each unit's spawn and
exit independently. Leave the per-unit `format_systemctl_stop_failure` tests
untouched: they exercise the generic single-unit failure formatter with a sample
unit name, not a "what ack stops" enumeration.

## Considered and rejected

- **Extract a shared `{{#include}}` snippet** for the three descriptions
  (ack.md prose step, monitoring-and-alerts.md ASCII node, monitor.md prose). The
  three presentations differ in form (numbered prose / diagram node / paragraph);
  a single include fits none cleanly and would read worse. The right fix is to
  correct the two divergent spots to match the already-correct prose.
- **Add an automated drift check** (a `scripts/docs/check-*.py`) asserting every
  doc that names ack's stopped units names both. There is no structured anchor to
  check against; a semantic prose check over two instances is over-engineering and
  brittle. Out of scope.
- **Touch `README.md`.** Its monitoring bullet is deliberately conceptual ("beep
  ... until acknowledged ... proactive capacity (ENOSPC) risk raises a quieter
  non-beeping warning") and enumerates no units, so it stays in sync without edit.

## Verification

- `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`; confirms no
  broken links and the pages render (the edits add no new links, so this is a
  regression guard).
- `scripts/docs/check-output-ascii.py` is scoped to `cli/src/**/*.rs` and
  `modules/**/*.nix` echo lines but exempts comments and tests, so the `///`
  comment edit is unaffected; the Markdown edits use ASCII (`--`, `->`) regardless.
- The `cli/src/ack.rs` edit is comment-only, so it cannot affect the build or any
  test. The standing behavioral guard is the already-registered VM test
  `tests/cli/braid-monitor-enospc.py` (asserts the advisory is started on the
  Warning path and stopped by `braid ack`); no new test is needed.
- Stale-wording sweep -- confirm all three edited locations now name
  `braid-alert-advisory.service`: `grep -rn "braid-alert-advisory"
  docs/commands/ack.md docs/guides/monitoring-and-alerts.md cli/src/ack.rs`. The
  two Markdown files return one new hit each; `cli/src/ack.rs` now also returns the
  corrected `cleanup_alert_files_and_beeper` comment alongside the existing
  `stop_beeper` comment and `stop_unit` calls.
- Read the three edited sections to confirm the ack.md step still reads as a
  coherent numbered step, the diagram box stays aligned with its start-side
  counterpart, and the Rust comment still states the best-effort + ordering
  contract correctly.
