# Exponential backoff for alert beep

## Context

The braid-alert service beeps every 15 seconds in a tight loop. If the NAS maintainer isn't nearby, this is obnoxious for anyone else in earshot. The user wants exponential backoff capping at once per 15 minutes, so the first beeps are still urgent but it calms down if nobody responds.

## Change

**File:** `modules/braid/monitor.nix` (lines 76-80)

Replace the fixed-interval beep loop:

```bash
while true; do
  beep ...
  sleep 15
done
```

With exponential backoff:

```bash
delay=5    # seconds
max_delay=900
while true; do
  beep ...
  sleep "$delay"
  delay=$((delay * 2))
  if [ "$delay" -gt "$max_delay" ]; then
    delay=$max_delay
  fi
done
```

Progression: 5s → 10s → 20s → 40s → 80s → ~3m → ~5m → ~11m → 15m → 15m → ...

That's it — one shell variable change in the rendered script. No new options, no new files.

## What doesn't change

- The backoff resets automatically when the alert service is stopped (`braid ack`) and re-triggered — each `systemctl start` runs the script fresh.
- `braid-alert-no-beep` path is unaffected (no beep loop at all).
- Existing tests pass as-is — they check script shape (modprobe, setpriv, reuid) not timing.

## Verification

1. `just test braid-alert braid-alert-no-beep` — existing tests still pass.
2. Inspect rendered script: `systemctl cat braid-alert.service` should show the `delay`/`max_delay` variables in the loop.
