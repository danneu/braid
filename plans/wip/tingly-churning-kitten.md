# Fix: status-during-balance test race condition

## Context

`braid-status-during-balance` fails because the btrfs balance completes before `braid status` runs. The host round-trip between confirming balance is running and running `braid status` is too slow.

## Fix

In `tests/cli/braid-status-during-balance.py`:

Replace steps 5+6 with a single VM-local polling loop that runs `braid status` (and `braid status --json`) repeatedly until an invocation observes a running balance. This eliminates the host round-trip race and preserves the original "status during a running balance" contract.

Concretely:

1. **Merge steps 5+6 (human-readable)**: One `machine.succeed(...)` call containing a shell loop that:
   - Runs `braid status` and checks stdout for `"Balance:"`
   - If found, saves the output and exits 0
   - If not found, sleeps briefly and retries
       - Times out with exit 1 after 10s (`seq 1 200` with `sleep 0.05`)
   - After the loop, the captured output is printed and assertions run on it in Python

2. **Merge step 5 + JSON subtest**: Same pattern — loop `braid status --json` inside the VM until `balance.state == "running"` is observed, capture and assert in Python, using the same 10s timeout.

3. **Keep assertions unchanged**: `"running"` state, all existing checks on human and JSON output.

## Files to modify

- `tests/cli/braid-status-during-balance.py` — lines 59-83

## Verification

`just test braid-status-during-balance`
