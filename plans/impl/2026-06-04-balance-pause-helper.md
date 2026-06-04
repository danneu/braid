# Plan: extract `pause_balance_with_remaining_work` shared test helper

## Context

`tests/cli/braid-unlock.py` "Test 8: paused balance survives unlock" is flaky
(`Could not pause balance with remaining work after 3 full attempts`).

**Root cause (verified against `reference/linux/fs/btrfs/volumes.c#__btrfs_balance`):**
btrfs honors a balance pause request only *between* block-group relocations
(the `(!counting && pause_req)` check at the top of the chunk loop), and a
balance only pauses with "remaining work" (`current < total`) when there are
`>= 2` block groups. Test 8 runs `btrfs balance start -dconvert={target}` on a
32 MB payload = a single data block group, so it can only pause at
"1 out of 1" -> no remaining work -> the `current < total` check never holds ->
3 attempts exhaust. dm-delay makes that single relocation slow (good) but
slowness cannot help when there is only one chunk.

**Why a helper, not a one-line patch.** The start/pause/retry idiom is
hand-copied across four test scripts; Test 8 drifted (an incomplete
512MB->32MB+dm-delay migration that never added the compensating `-mconvert`).
Three of the four copies are identical pass/fail uses; extracting them into one
helper that always converts data **and** metadata fixes Test 8 by construction
and removes the drift class from those three. The extraction also corrects a latent
misfire in the copied idiom: it alternated the convert target to `single` on two
of every three attempts, but `-mconvert=single` reduces metadata and system
redundancy on a raid1/raid1 pool, which the kernel rejects without `--force` -- so
those attempts never started a balance and each test had one effective attempt.
The helper converts to `raid1` every attempt; a hard (non-`soft`) convert rewrites
all block groups regardless of current profile, so each retry still has fresh
work. The fourth
(`capture-tool-fixtures.py`) is a fixture-capture flow that keeps its own
`-dconvert`-only copy and stays separate (see below). It is the copy that most
resembles the original bug -- a `-dconvert`-only loop whose data-chunk count is
unverified (512 MB of data can fit in a single ~1 GB data block group) -- so if
it ever flakes, adding `-mconvert` is the fix.

**Outcome:** one documented helper; Test 8 reliably reaches a
paused-with-remaining-work state; the two already-passing balance-pause tests
keep passing on the shared helper (which also drops their latent single-convert
misfire, so they get three real attempts instead of one).

## Scope decision (settled)

- **In:** `braid-status-during-balance.py`, `braid-exclop-paused-balance.py`,
  `braid-unlock.py` (Test 8) -> route through the new helper.
- **Out:** `tests/capture-tool-fixtures.py`. It is a golden-fixture capture
  *pipeline* (not a pass/fail test): it pauses a balance, remounts
  `-o skip_balance` to reset counters to `0/0`, captures
  `btrfs-balance-status-paused-skip-balance.txt` (the `nan%`-formatting canary),
  then `mkfs`-rebuilds a clean `raid1/raid1` filesystem because the balance's
  leftover mixed profile would otherwise break the later replace captures with
  ENOSPC. It is wired without a `braid` arg. The captured fixture does **not**
  depend on the convert flags -- after the `skip_balance` reset it is "0 out of
  about 0 chunks" regardless -- so this is not a fixture-shape constraint. The
  file stays out of scope because it is a different artifact class with its own
  tuned capture sequence; its working `-dconvert`-only loop has no reason to
  change. Its `# Reuses the proven pattern from tests/cli/braid-unlock.py`
  comment (line 149) still goes stale on migration (braid-unlock.py will call
  the helper, not inline the pattern), so repoint it to note it mirrors
  `tests/module/balance_helpers.py` but stays inline as a separate
  capture-pipeline artifact -- do **not** encode a fixture-shape or mixed-profile
  rationale. See section 2's comment-cleanup list.

## 1. New helper file: `tests/module/balance_helpers.py`

New file beside `tests/module/dm_delay_helpers.py`. Helper files are not test
scripts -- no Intent/Why/Scenario preamble (module docstring only). Keep module
scope to exactly the two imports + one function: these files are joined by **text
concatenation** into a single Python module, so any stray top-level name (e.g. a
hoisted constant or loop variable) could clobber a test fragment's global. Re-`import` is
idempotent and safe; all loop locals stay inside the function. First arg is
`node` (matches the `dm_delay_*` convention; callers pass `machine`).

```python
"""Shared helper for VM tests that need a btrfs balance paused mid-flight.

Extracted because the start/pause/retry idiom was hand-copied across four test
scripts and one copy (braid-unlock Test 8) drifted into a flaky form.
"""

import re
import time


def pause_balance_with_remaining_work(node, *, mount_point="/mnt/storage", attempts=3):
    """Start a btrfs balance and pause it with remaining work still to do.

    Returns the paused `btrfs balance status` output (str). Raises if no
    attempt pauses with remaining work, or a failed attempt cannot be
    cancelled cleanly.

    Always converts BOTH data and metadata (`-dconvert -mconvert`; `-m` also
    covers the system group). btrfs honors a pause request only *between*
    block-group relocations, and only pauses with remaining work
    (current < total) when >= 2 block groups exist. Converting data alone on a
    small payload yields a single data block group, which can only pause at
    "1 out of 1" -- the original Test 8 flake. See
    reference/linux/fs/btrfs/volumes.c __btrfs_balance (the
    `(!counting && pause_req)` check at the top of the chunk loop).

    Caller owns payload size and any dm-delay slowdown; this helper only owns
    the start/pause/verify/retry loop.
    """
    for _ in range(attempts):
        # Start in background, then tight-loop pause attempts natively on the
        # VM (no Python roundtrip overhead -- a fast balance finishes in <1s).
        # Fixed hard raid1 convert (no `soft`): on a raid1/raid1 pool a hard
        # convert rewrites every block group regardless of its current profile,
        # so each retry has fresh work without alternating the target. Do NOT
        # switch to `-dconvert=single -mconvert=single` to force work: -mconvert
        # also rewrites system chunks, and reducing metadata/system redundancy
        # (raid1 -> single) makes the kernel reject the start with -EINVAL unless
        # --force is given (reference/linux/fs/btrfs/volumes.c btrfs_balance, the
        # reducing_redundancy gate).
        node.execute(
            f"btrfs balance start -dconvert=raid1 -mconvert=raid1 {mount_point} "
            f"> /tmp/balance.log 2>&1 & "
            f"for i in $(seq 1 200); do "
            f"  btrfs balance pause {mount_point} 2>/dev/null && break; "
            f"  sleep 0.02; "
            f"done"
        )

        output = node.execute(f"btrfs balance status {mount_point}")[1]
        if "paused" in output.lower():
            match = re.search(r"(\d+)\s+out of about\s+(\d+)\s+chunks", output)
            if match and int(match.group(1)) < int(match.group(2)):
                return output

        # Completed or paused with no remaining work -- cancel and retry. The
        # hard raid1 convert above rewrites every block group again, so the next
        # attempt always has fresh work.
        node.execute(f"btrfs balance cancel {mount_point} 2>/dev/null || true")
        for _ in range(30):
            if "no balance" in node.execute(
                f"btrfs balance status {mount_point}"
            )[1].lower():
                break
            time.sleep(0.2)
        else:
            raise Exception(
                "balance did not terminate after cancel -- cannot retry safely"
            )

    raise Exception(
        f"could not pause balance with remaining work after {attempts} full attempts"
    )
```

## 2. Migrate the three call sites

Pattern: delete the inline `targets=[...]` / `for attempt in range(3)` loop
(and its trailing `assert paused` / `else: raise`) and replace with a single
`pause_balance_with_remaining_work(machine)` call. Caller-owned setup
(dm-delay activate/deactivate, payload `dd`) and all post-pause assertions stay.
After removing each loop, grep the file and drop any now-unused `import re`
(and the inline `import time` that lived in the removed block).

**Repoint stale comments left by extraction.** Moving the start/pause/retry
mechanism out of each test leaves stale every comment that *describes* that
mechanism, points at another file by line number, or misattributes why the test
is stable. Per AGENTS.md File References,
the line-number ban "applies to docs and comments." Fix these in the same
migration (verified against current line numbers):
- `braid-exclop-paused-balance.py:37-38` -- `# Reuse the retry pattern from
  braid-status-during-balance.py:56-102.` survives the body-only replacement and
  becomes doubly wrong (shifted lines + pattern moved to the helper). Replace the
  two-line `# 3.` block with: `# 3. Start and pause a balance via the shared
  balance_helpers.pause_balance_with_remaining_work helper.`
- `braid-status-during-balance.py:63-67` -- the `# 4. Start balance ...` block
  details inline mechanism the helper now owns ("single shell command", "Python
  roundtrip overhead", and a "retry with the opposite conversion target" line
  that no longer applies -- the helper retries the same raid1 convert). Trim to a
  caller-relevant note naming the helper and the dm-delay's role; drop the
  internal-mechanism lines.
- `braid-unlock.py:581-582` -- `# Write enough data to create multiple btrfs
  chunks ...` is wrong under the verified root cause: 32 MB is one data block
  group (the original flake), so the payload never created "multiple chunks." The
  multiple block groups that make a pause-with-remaining-work possible come from
  the helper's `-dconvert -mconvert` (data + metadata + system), not payload size.
  Rewrite the comment above the kept `dd` to: the small payload gives the balance
  real data to relocate; the helper's data+metadata hard raid1 convert is what
  yields multiple block-group types; dm-delay keeps each relocation slow enough to
  catch the pause.
- `braid-exclop-paused-balance.nix:10-12` -- `# How:` header reads "(using the
  retry pattern from braid-status-during-balance)"; repoint to "(via the shared
  balance_helpers retry pattern)".
- `tests/capture-tool-fixtures.py:149` -- `# Reuses the proven pattern from
  tests/cli/braid-unlock.py` -> repoint to mirror
  `tests/module/balance_helpers.py`, noting the loop stays inline as a separate
  capture-pipeline artifact. Do **not** encode a fixture-shape or mixed-profile
  reason (see the scope-decision bullet -- the captured fixture is convert-flag
  independent).
- `braid-status-during-balance.nix` needs **no** change: its `# How:` header
  describes behavior generically, with no cross-file or mechanism reference.

- **`tests/cli/braid-status-during-balance.py`** (passing; canonical loop source).
  Inside `with subtest("start and pause balance"):`, keep `dm_delay_activate(...)`,
  replace the loop body (`targets=[...]` through `assert paused, ...`) with
  `pause_balance_with_remaining_work(machine)` then `dm_delay_deactivate(...)`.
  Drop top-level `import re` (only use was the removed `re.search`). Keep
  `import json`, `import shlex`. The `with subtest("status during balance")`
  block and final cancel are untouched.

- **`tests/cli/braid-exclop-paused-balance.py`** (passing). The 512 MB `dd`
  above the subtest stays (this test's slowness source -- no dm-delay). Replace
  the whole `with subtest("start and pause balance"):` body with the single
  helper call. Drop `import re`; keep `import shlex`. Subtests 4/5 (add/lock
  fail-fast) and final cancel are untouched.

- **`tests/cli/braid-unlock.py` Test 8** (the fix). Keep the `dd ... count=32`,
  `sync`, and `dm_delay_activate(..., write_delay_ms=500)` -- but rewrite the
  misleading payload comment above the `dd` (comment-cleanup list above). Replace
  the block
  from `import re` through the `else: raise Exception("Could not pause balance
  ... after 3 full attempts")` with `pause_balance_with_remaining_work(machine)`.
  Delete the `paused_status` variable (assigned, **never read** -- the post-unlock
  "still paused" check at the end of Test 8 runs a fresh `btrfs balance status`).
  Drop the now-orphaned `import re` and inline `import time`. Everything after
  (`dm_delay_deactivate`, lock/re-unlock, "still paused" assert, "paused balance"
  warning assert, cleanup) is untouched. This adds the missing `-mconvert` by
  construction.

## 3. Wire the helper into the three `.nix` files

Helpers are prepended by text concatenation (`readFile A + "\n\n" + readFile B`).
Order between two helpers does not matter (function defs only); list
`dm_delay_helpers.py` first for consistency. No `flake.nix` change -- composition
is self-contained per `.nix`, and there is no per-test helper manifest.

- `tests/cli/braid-status-during-balance.nix` and `tests/cli/braid-unlock.nix`
  (already prepend `dm_delay_helpers.py`): insert
  `+ builtins.readFile ./../module/balance_helpers.py + "\n\n"` between the
  dm-delay helper and the test script.
- `tests/cli/braid-exclop-paused-balance.nix` (currently prepends nothing):
  ```nix
  testScript =
    builtins.readFile ./../module/balance_helpers.py + "\n\n"
    + builtins.readFile ./braid-exclop-paused-balance.py;
  ```

## 4. Gotchas

- **Delete the `assert paused` line too**, not just the loop -- a dangling
  `assert paused` would `NameError` (the var is gone). It's the last line of the
  subtest in both passing tests.
- **Use `node` inside the helper, never a bare `machine`** -- in the concatenated
  global namespace a stray `machine` would resolve by accident and mask the param.
- **f-string lint:** the build-time linter flags `f-string is missing
  placeholders`. The `attempts` message has a placeholder (fine); the
  cancel-failure message is a plain string (fine).
- **No assertion reads the loop's failure text** -- callers only used the
  `paused` bool (or ignored it), so the helper's `raise` message wording cannot
  break a test. ASCII `--` standardizes the message (existing copies use an
  em-dash).
- **Timing unchanged** -- the helper does not touch dm-delay; the two dm-delay
  tests keep their `write_delay_ms=500` around the call, so catch-rate is
  preserved.

## 5. Verification

Run the three migrated tests:

```sh
just test-vm braid-status-during-balance braid-exclop-paused-balance braid-unlock
```

Flake-proof Test 8 (one green run does not prove a flake fixed; `-rebuild` errors
when there is no valid prior output, so force fresh runs by deleting the store
path between iterations):

```sh
for i in $(seq 1 10); do
  echo "=== braid-unlock run $i ==="
  out=$(nix eval --raw .#checks.aarch64-darwin.braid-unlock.outPath)
  nix store delete "$out" >/dev/null 2>&1 || true
  just test-vm braid-unlock || { echo "FAILED on run $i"; break; }
done
```

Aim for ~10 consecutive clean `braid-unlock` runs; 3-5 each for the two siblings
to confirm no regression. Optional direct confirmation of the mechanism: a `-v`
run should now show Test 8's paused status as `X out of about N chunks` with
`N >= 2` (previously `1 out of about 1`).

## Critical files

- `tests/module/balance_helpers.py` (new -- the helper)
- `tests/cli/braid-unlock.py` (Test 8 -- the flake fix)
- `tests/cli/braid-status-during-balance.py`, `tests/cli/braid-exclop-paused-balance.py` (migrate loops)
- `tests/cli/braid-unlock.nix`, `tests/cli/braid-status-during-balance.nix`, `tests/cli/braid-exclop-paused-balance.nix` (testScript prepends)
