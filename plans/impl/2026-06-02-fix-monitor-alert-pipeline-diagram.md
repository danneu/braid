# Fix the monitor.md "Alert pipeline" diagram (beeper trigger accuracy)

## Context

`docs/commands/monitor.md` ("Alert pipeline" section) draws the beeper as if it
were driven by the alert-latch file:

```
braid monitor (timer) --> alert-latch.json --> braid-alert.service (beeper)
                                           --> braid status / braid tui (display)
```

with prose "When monitor writes an active alert latch, the systemd alert service
activates the PC speaker beeper."

This is wrong. Verified against code:

- The beeper is started by **monitor's exit code 1**, not by any read of the
  latch. The wrapper `braid-monitor.service` runs `braid monitor` and, only on
  exit 1, runs `systemctl start braid-alert.service`
  (`modules/braid/monitor.nix`, the `braid-monitor` service `script`).
- `braid-alert.service` only modprobes pcspkr, runs the optional `alertCommand`,
  and beeps in a loop. It **never reads `alert-latch.json`**
  (`modules/braid/monitor.nix`, the `braid-alert` service `script`).
- `smartd` is a **second, independent** beeper trigger. Its notifier hook
  (`smartdAlertScript` in `modules/braid/monitor.nix`) does two separate things:
  it runs `systemctl start braid-alert.service` directly (the beep), and it
  touches `/var/lib/braid/smartd-alert` -- a flag the *next* `braid monitor`
  cycle reads (`cli/src/alert.rs#smartd_alert_active`) to latch a `SmartdAlert`
  cause (`status` and `ack` read it too); the alert service never reads it.
- `alert-latch.json` is written by `cli/src/monitor.rs#cmd_monitor` (via
  `cli/src/alert.rs#save_alert_latch`) and **read back by monitor itself** every
  cycle (`cli/src/alert.rs#load_alert_latch_or_quarantine`, then
  `cli/src/alert.rs#merge_into_latch`): because `cmd_monitor` re-exits 1 while the
  merged latch holds any cause, that read-and-merge is the sticky-beep mechanism,
  not the alert service. It is also read by `cli/src/ack.rs#cmd_ack` (to clear)
  and by `cli/src/status.rs` plus the TUI (for display). The systemd **alert
  service** never reads it; `rg` over `modules/` confirms nothing in the systemd
  layer reads the latch.
- `braid ack` (`cli/src/ack.rs`) clears the latch and runs
  `systemctl stop braid-alert.service` to stop the beeper.

**Impact of the bug:** an operator debugging "why is it beeping" or "why won't
the beep stop" inspects or deletes `alert-latch.json` expecting it to gate the
beeper. It does not -- the real trigger is monitor's exit-1 (or smartd's direct
start), and the only thing that stops the beep is `braid ack` (`systemctl stop`).

**Intended outcome:** redraw the diagram and prose so the exit code and smartd's
direct `systemctl start` drive the beeper; the latch is shown feeding display
*and* monitor's sticky re-exit (never the alert service); smartd's flag is shown
feeding only the next monitor cycle; and `braid ack` is named as what stops the
beep -- bringing the command page into agreement with the already-correct guide
(`docs/guides/monitoring-and-alerts.md`) and ADRs 014/018.

## Scope

Single file: `docs/commands/monitor.md`, the `## Alert pipeline` section only
(the fenced diagram plus the one prose paragraph that follows it). A repo-wide
sweep confirmed every other doc that describes this pipeline
(`docs/guides/monitoring-and-alerts.md`, `docs/design/decisions/014-alerts.md`,
`docs/design/decisions/018-systemd-lifecycle.md`, and the `status`/`ack`/`doctor`
command pages) is already correct. No code changes; no other docs.

The rest of `monitor.md` is accurate and must be left as-is. In particular,
do **not** touch:

- The "What triggers an alert (exit 1)" list (line ~36, the `ComputationError`
  sentence) -- "latches a `ComputationError` cause so the beeper fires" is an
  accurate causal chain (latch active -> exit 1 -> beeper), not the same bug.
- "What happens under the hood" step 6 -- correctly describes the sticky latch
  without claiming it drives the beeper.

## The change

Replace the fenced diagram and the paragraph at line ~58. Keep the
`## Alert pipeline` heading.

**New diagram** (command-page terse style; both `braid monitor` and `smartd` are
drawn as one source with two labeled outputs, so the smartd path mirrors the
monitor path instead of collapsing the flag and the direct start into one edge):

```
braid monitor      --writes--> alert-latch.json --> braid status / braid tui (display)
(timer, every 5m)  --exit 1--> braid-alert.service (beeper + alertCommand)

smartd  --start-->  braid-alert.service (beeper)
        --writes--> smartd-alert --> next braid monitor cycle (latches SmartdAlert)
```

**New prose** (replaces the old "When monitor writes an active alert latch..."
sentence):

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

### Conventions to honor

- ASCII only: `--` (double hyphen), straight quotes -- no em-dash, no curly
  quotes (AGENTS.md "CLI Output Style" / writing style).
- No new cross-links and no line-number references are introduced, so the
  `path#anchor` File-References rule and `mdbook-linkcheck2` are unaffected.
- Backtick unit names, file names, and commands in the prose (matches the
  page's existing use of backticks for `alert-latch.json`, `braid ack`, etc.).

## Verification

- `mdbook build docs` -- must succeed (linkcheck/build gate per AGENTS.md). No
  new links are added, so this is a sanity check that the page still builds.
- Visual: render `docs/commands/monitor.md` and eyeball the diagram. Both source
  nodes show two aligned outputs: `braid monitor`'s `--writes-->` / `--exit 1-->`
  and `smartd`'s `--start-->` / `--writes-->`. Confirm no edge runs from
  `smartd-alert` (or `alert-latch.json`) *into* `braid-alert.service` -- the flag
  and the latch must never be drawn as reaching the service.
- `rg -n "alert-latch\.json --> .*beeper|writes an active alert latch" docs/commands/monitor.md`
  returns nothing (old misleading phrasing is gone).
- Consistency: confirm the new model matches
  `docs/guides/monitoring-and-alerts.md` "How the pieces fit together" (exit-1
  trigger + smartd direct path) -- already verified during planning.

No new tests: docs-only change, no code or behavior touched. The behavioral
claims this section now encodes are already pinned by VM tests -- exit-1 starts
the beeper (`tests/module/braid-alert.py`, `tests/cli/braid-monitor.py`), the
smartd direct-start + flag path (`tests/cli/braid-smartd-alert.py`), and `braid
ack` stopping the beep (asserted in `tests/cli/braid-smartd-alert.py`) -- so a
future code change that diverged from this diagram would fail CI independently of
the prose.
