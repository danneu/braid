# Fix backwards beep-stickiness claim in monitor.md "Alert pipeline"

## Context

`docs/commands/monitor.md` (the "Alert pipeline" paragraph) tells a reader
that monitor's per-cycle latch read-back -- "not the alert service" -- is what
keeps both the alert *and the beep* sticky. That is backwards for the beep, and
the paragraph contradicts its own closing sentence.

Verified against the code this session:

- `braid-alert.service` keeps the beep going on its own. With beep enabled it is
  a `Type=simple` unit running a `while true; do beep; sleep; done` backoff loop;
  with beep disabled it is a `Type=oneshot` unit with `RemainAfterExit=true`
  (`modules/braid/monitor.nix`, the `braid-alert` service block). Either way it
  stays active until `braid ack` runs `systemctl stop braid-alert.service`
  (pinned by `tests/module/braid-alert-no-beep.py`, subtest "Service latches
  active after exit").
- The wrapper's per-cycle `systemctl start braid-alert.service` on exit 1
  (`modules/braid/monitor.nix`, the `braid-monitor` service script) re-starts an
  already-running service -- a systemd no-op.
- The read-back is `merge_into_latch` (`cli/src/monitor.rs#cmd_monitor` ->
  `cli/src/alert.rs#merge_into_latch`): it carries latched causes forward and
  re-returns `Alert`, which `cli/src/main.rs` (the `Commands::Monitor` arm) maps
  to exit 1. That is what keeps the *latch and exit-1* sticky -- not the beep.
- A skipped cycle proves the split: on a contended pool lock monitor exits 0
  with no `systemctl stop` (`cli/src/main.rs#acquire_per_policy`,
  `LockPolicy::MonitorSilent` -> `Err(AlreadyHeld) => exit(0)`), and an offline
  pool exits 0 too -- yet an already-running beep keeps beeping. If the read-back
  held the beep, a skipped read-back would silence it; it does not.

The sibling docs already get this right and are the model for the fix:
`docs/guides/monitoring-and-alerts.md` ("How the pieces fit together":
`braid-alert.service` beeps until acknowledged; `braid ack -> braid-alert.service
stops (beeping stops)") and ADR 014 (`docs/design/decisions/014-alerts.md`,
which credits latch stickiness to merge + ack and never claims the read-back
holds the beep). `monitor.md` is the lone outlier.

Intended outcome: a reader debugging "why is it still beeping after the disk
recovered?" is pointed at the running `braid-alert.service` (stopped only by
`braid ack`), and the read-back is credited only with the latch / exit-1 /
status banner. The fix also targets readability: the current paragraph is a
wall of prose that buries the distinction mid-sentence, so it is reflowed into
a lead sentence plus a two-item bulleted contrast that makes the "two sticky
things, two different holders" point obvious at a glance.

## Scope

Documentation only. One paragraph in one file, reflowed in place. No code
change. No sibling-doc change: the guide and ADR 014 are already correct, and
per `AGENTS.md` the `docs/commands/` pages are self-contained reference, so
restating the pipeline here (rather than deferring to the guide) is intentional.
The reflow stays inside that boundary -- it is a within-paragraph regrouping for
accuracy and readability, not the page consolidation the section explicitly
rules out. The command page stays self-contained.

## The change

File: `docs/commands/monitor.md`, the single paragraph directly under the
"Alert pipeline" fenced diagram.

Reflow the whole paragraph rather than swapping one sentence: keep the verified
content but regroup it so the latch-vs-beep distinction is carried by structure.
The new shape is a lead sentence (the wrapper starts the service), a two-item
bulleted contrast (latch + exit 1 held by monitor; beep held by the service
itself), then two short trailing lines (the `smartd` second trigger, and the
`braid ack` stop). This subsumes the earlier "drop the bare beep loop label"
point -- the lead sentence no longer labels the service a beep loop at all --
and relocates the load-bearing "never reads `alert-latch.json` or the
`smartd-alert` flag" claim (true in both modes) into the beep bullet.

**Before** (the backwards clause is the middle sentence):

> On exit 1, the `braid-monitor.service` wrapper starts `braid-alert.service`
> (the beeper, plus any `alertCommand`); that service is a bare beep loop and
> never reads `alert-latch.json` or the `smartd-alert` flag. Monitor writes the
> active causes to `alert-latch.json` and re-reads it every cycle, re-exiting 1
> while any cause remains -- that read-back, not the alert service, is what keeps
> the alert and beep sticky; `braid status` and the TUI read the same file for
> display. `smartd` is a second, independent trigger: on a SMART fault it starts
> `braid-alert.service` directly *and* writes the `smartd-alert` flag that the
> next monitor cycle latches as a `SmartdAlert` cause. The beep stops only when
> `braid ack` clears the latch and runs `systemctl stop braid-alert.service`.

**After:**

> On exit 1, the `braid-monitor.service` wrapper starts `braid-alert.service`
> (the beeper, plus any `alertCommand`). After that, two things stay active
> until you `braid ack`, each held by a different mechanism:
>
> - **The latch and exit 1** -- held by **monitor**. Each cycle it writes the
>   live causes to `alert-latch.json`, merging them into the existing latch, and
>   re-exits 1 while any cause remains. `braid status` and the TUI read the same
>   file for display.
> - **The beep** -- held by **`braid-alert.service` itself**, not the read-back.
>   Once started it stays active on its own (the backoff beep loop when beep is
>   enabled, or a `RemainAfterExit` oneshot when it's off), so the wrapper's
>   per-cycle `systemctl start` is a no-op and a skipped cycle (offline or
>   lock-contended exit 0) does not silence it. The service never reads
>   `alert-latch.json` or the `smartd-alert` flag.
>
> `smartd` is a second, independent trigger: on a SMART fault it starts
> `braid-alert.service` directly *and* writes the `smartd-alert` flag, which the
> next monitor cycle latches as a `SmartdAlert` cause.
>
> The beep stops only when `braid ack` clears the latch and runs
> `systemctl stop braid-alert.service`.

## Wording constraints for the implementer

Do not "simplify" these away -- each carries a verified distinction, now mapped
onto the new structure:

- The latch-vs-beep split must stay **two distinct bullets**. The whole point of
  the reflow is to make the correction structural; re-collapsing it into one
  sentence would reintroduce the buried, error-prone phrasing.
- Latch bullet: "merging them into the existing latch" (not "re-reads it") --
  merge-not-replace (`alert.rs#merge_into_latch`) is what carries a cleared
  cause forward; "re-reads" understates it.
- Beep bullet: "stays active on its own (the backoff beep loop when beep is
  enabled, or a `RemainAfterExit` oneshot when it's off)" -- beep-off does
  **not** beep, so say "stays active," not "beeps," for that branch.
- Beep bullet: keep both "per-cycle `systemctl start` is a no-op" and "a skipped
  cycle ... does not silence it" -- together they are the concrete proof the
  beep is service-held, and they answer the debugging scenario that motivated
  the fix.
- Beep bullet: keep "never reads `alert-latch.json` or the `smartd-alert` flag"
  (true in both modes, `monitor.nix`). The lead sentence must not re-label the
  service "a bare beep loop"; the "(the beeper, ...)" appositive may stay as
  common-case naming.
- ASCII only: `--` not em-dash, straight quotes (matches the file and the repo
  CLI/output style rule).
- Emphasis: `*...*` for italics (matches the paragraph's existing `*and*`); the
  bold bullet lead-ins (`**The latch and exit 1**`, `**The beep**`) match the
  existing `- **btrfs device errors** -- ...` list already in this file.

## Verification

- `mdbook build docs` -- the authoritative gate. It runs `mdbook-linkcheck2`
  (per `docs/book.toml`); a clean build confirms no cross-links broke. This edit
  adds no links, so the risk is only that surrounding link syntax was disturbed.
- Read the rendered section end-to-end and confirm the new shape renders cleanly
  under `mdbook build docs`: lead sentence, the two bullets (latch / beep), the
  `smartd` line, then the closing `braid ack` stop line. Confirm internal
  consistency -- the beep bullet must agree with the closing "beep stops only
  when `braid ack` ... runs `systemctl stop`" line (both credit the service) and
  with the diagram above it.
- Cross-check the two factual anchors against the tree so the prose stays true:
  the `braid-alert` service shape in `modules/braid/monitor.nix` (simple beep
  loop vs. `RemainAfterExit` oneshot) and the lock-contended/offline exit-0
  paths in `cli/src/main.rs` (`acquire_per_policy` `MonitorSilent`, and the
  `Commands::Monitor` `PoolOffline => exit(0)` arm).

No Rust unit tests or NixOS VM tests are needed -- behavior is unchanged; this is
a docs accuracy fix.
