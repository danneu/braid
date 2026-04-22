# Exponential backoff for alert beep

Issue: https://github.com/danneu/braid/issues/33

## Context

`braid-alert.service` beeps every 15 seconds in a tight loop while the alert
condition is unacknowledged. If nobody is nearby the NAS, a steady 15s beep
is obnoxious for anyone else in earshot while still leaving the maintainer
free to miss it entirely during the first minute.

Goal: keep the early beeps urgent (so the maintainer still notices quickly)
but back off to once per 15 minutes if nobody acknowledges. Backoff resets
naturally because `braid ack` stops the service, and a fresh `systemctl start`
reruns the script from the top.

The behavior change is one shell loop, but it ships with a matching
rendered-script test assertion (so a future refactor cannot silently drop the
backoff or the cap) and doc updates everywhere the manual currently promises a
fixed 15s cadence.

## Code change

**File:** `modules/braid/monitor.nix` (the `beepEnabled` branch of
`systemd.services.braid-alert.script`, currently lines 98-103)

Replace:

```bash
while true; do
  ${braidBeepProbe}/bin/braid-beep-probe 2>/dev/null || true
  sleep 15
done
```

With:

```bash
delay=5
max_delay=900
while true; do
  ${braidBeepProbe}/bin/braid-beep-probe 2>/dev/null || true
  sleep "$delay"
  delay=$((delay * 2))
  if [ "$delay" -gt "$max_delay" ]; then
    delay=$max_delay
  fi
done
```

Progression: 5s -> 10s -> 20s -> 40s -> 80s -> 160s -> 320s -> 640s -> 900s
-> 900s ... (caps at 15 minutes).

Shell note: the script body is a multi-line Nix string. POSIX `$((...))` and
`"$delay"` are bare dollar signs (no `${...}`), so Nix passes them through
unchanged. No escaping needed.

## Test change

**File:** `tests/module/braid-alert.py`

Extend the existing `"Service script has modprobe fallback ..."` subtest to
also assert the rendered script body contains the backoff state machine. The
test already reads `script = machine.succeed(f"cat {exec_start}")` at line
41, so we add four assertions on `script`:

```python
assert "delay=5" in script, f"alert script must initialize delay=5:\n{script}"
assert "max_delay=900" in script, f"alert script must cap delay at 900s:\n{script}"
assert "delay * 2" in script, f"alert script must double the delay each iter:\n{script}"
assert "$max_delay" in script, f"alert script must clamp to max_delay:\n{script}"
```

Rationale: the existing test already inspects the rendered service script for
shape (modprobe, beep wrapper). Adding cadence assertions to the same subtest
is a 4-line addition that catches the exact regression class principles.md
calls out -- a refactor that silently changes alert behavior. No new
subtests, no timing waits.

## Doc changes

Three manual pages currently promise a fixed 15s cadence. Update each to
describe the front-loaded backoff capping at 15 minutes:

- `manual/guides/monitoring-and-alerts.md:23` -- "Beeps the PC speaker every
  15 seconds (if enabled) until acknowledged."
- `manual/guides/monitoring-and-alerts.md:156` -- ASCII flow diagram line
  "-> beep (PC speaker, every 15s)".
- `manual/guides/nixos-configuration.md:94` -- "The beep loops every 15
  seconds until acknowledged with `braid ack`."
- `manual/guides/troubleshooting.md:117` -- "The PC speaker is beeping every
  15 seconds due to a disk health alert."

Suggested replacement phrasing (use the natural form for each context, but
keep the substance consistent):

> Beeps the PC speaker on alert, starting at 5 second intervals and backing
> off (5s, 10s, 20s, 40s, ...) up to once every 15 minutes until
> acknowledged.

`manual/book/` is generated mdBook output (gitignored at `.gitignore:10`)
-- do not edit by hand; it regenerates from the sources above.

## What doesn't change

- `braid-alert-no-beep` path: no beep loop, untouched.
- `braidBeepProbe` derivation, udev rules, kmod overlay,
  `notifier-config.json`.
- `braid-monitor.service`, the timer, smartd hook.
- Reset semantics: `braid ack` stops the service; next trigger starts fresh
  at `delay=5`.
- `braid-alert-no-beep.py`, `monitor-lifecycle.py`, `smartd-hook.py` --
  none assert beep cadence.

## Files to modify

- `modules/braid/monitor.nix` -- the 5-line loop inside the `beepEnabled`
  branch of `systemd.services.braid-alert.script`.
- `tests/module/braid-alert.py` -- 4 added assertions in the existing
  rendered-script subtest.
- `manual/guides/monitoring-and-alerts.md` -- two lines (23, 156).
- `manual/guides/nixos-configuration.md` -- one line (94).
- `manual/guides/troubleshooting.md` -- one line (117).

## Verification

1. `just test-vm braid-alert braid-alert-no-beep` -- must pass. The new
   assertions in `braid-alert.py` would fail if the loop were reverted to
   `sleep 15` or if `delay`/`max_delay`/cap logic were dropped.
2. Inspect rendered unit on a VM (or via `nixos-rebuild build`):
   `systemctl cat braid-alert.service` should show `delay=5`,
   `max_delay=900`, `$((delay * 2))`, and the `$max_delay` cap inside the
   loop.
3. `grep -rn "15 second\|every 15s\|sleep 15" manual/guides README.md` --
   should return no hits referring to the alert beep cadence after the doc
   edits.
