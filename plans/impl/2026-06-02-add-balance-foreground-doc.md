# Fix: `braid add` balance is foreground, not "background"

## Context

`docs/guides/day-to-day-nas-usage.md:134-136` tells readers that `braid add`
"runs a background balance" and that "existing data gradually rebalances. You
can check progress with `braid status` or `braid tui`." This implies `braid add`
returns immediately and the balance proceeds asynchronously, so you poll for
progress afterward.

That is false. The RAID1 balance runs in the **foreground** and `braid add`
blocks until it finishes (potentially hours on a large pool). Verified against
the code path and btrfs behavior:

- `cli/src/add.rs:1513-1527` -- prints `pool: balancing to RAID1...`, calls
  `pool_balance_raid1(...)`, and only prints `pool: RAID1 balance complete`
  after it returns.
- `cli/src/pool.rs:358-379` -- `pool_balance_raid1` calls `run_with_progress`
  (synchronous).
- `cli/src/progress.rs:204-262` -- `run_with_progress` spawns the `btrfs`
  command in a scoped thread and loops until `handle.is_finished()`, writing a
  **live** progress line each second. So it blocks *and* shows progress inline.
- `cli/src/cmd.rs:670-680` -- issues `btrfs balance start --enqueue
  -dconvert=raid1 -mconvert=raid1 <mp>` with **no** `--background`.
- `reference/btrfs-progs/Documentation/btrfs-balance.rst:97-100` -- `start`
  "runs in the foreground"; `--background|--bg` (line 133) is the opt-in flag
  braid does not pass.
- `docs/design/decisions/001-btrfs-raid1.md:41` -- the balance "can take hours
  on large pools."

Intended outcome: the guide accurately states the balance is foreground and
blocking, while preserving the genuinely useful "you can watch progress" intent
(progress is shown live in the terminal, not polled separately).

## Scope

Single file: `docs/guides/day-to-day-nas-usage.md`, lines 134-136 only.

A completeness sweep of all user-facing surfaces (README, `docs/commands/`,
clap `--help` strings in `cli/src/`, TUI strings, all other guides) found **no
other** inaccurate location:

- `docs/commands/add.md:90-91,95` -- already correct ("balances data to RAID1,
  then clears the journal"; accurate `--enqueue` explanation). No change.
- `README.md`, clap help strings, TUI -- no "background"/async/poll-later
  language for add. No change.
- `docs/guides/auto-unlock.md:110` ("data rebalances") -- about `replace` in
  degraded mode; implies neither background nor polling. Leave as-is.

No code change. The code is correct; only this guide is wrong.

## The change

Replace the two paragraphs at `docs/guides/day-to-day-nas-usage.md:134` and
`:136` (currently separated by a blank line 135).

**Before:**

```
braid formats the new drive with LUKS (using your existing passphrase), adds it to the btrfs pool, and runs a background balance to spread data across all drives. No `nixos-rebuild` required.

After adding a disk, existing data gradually rebalances. You can check progress with `braid status` or `braid tui`.
```

**After:**

```
braid formats the new drive with LUKS (using your existing passphrase), adds it to the btrfs pool, and rebalances data across all drives. No `nixos-rebuild` required.

The balance runs in the foreground -- `braid add` holds the terminal and does not return until it finishes, which can take hours on a large pool. braid shows live balance progress while it runs.
```

### Why this wording

- "runs in the foreground ... does not return until it finishes" is the core
  correction and matches `add.rs` + the btrfs default.
- "which can take hours on a large pool" preserves the decision-001 caveat the
  original glossed over.
- "braid shows live balance progress while it runs" replaces the false "check
  progress with `braid status`/`braid tui`" sentence with the accurate version
  of the same helpful fact (per `progress.rs`'s live progress loop), rather than
  dropping it. Directing users to a second session to poll is both unnecessary
  (progress is already on screen) and confusing.
- Uses `--` per the repo's CLI/doc style (the file already uses `--`, e.g.
  lines 138, 165). No em-dashes, no other Unicode.

Optional (implementer's call): append a pointer to the `--progress
auto|always|never` flag (documented at `docs/commands/add.md:71`). Recommended
to omit -- the guide is a day-to-day overview, and the command reference
already covers the flag.

## Verification

- Re-read the edited section in context (lines ~122-138) to confirm it flows and
  the surrounding "Adding disks over time" section stays coherent.
- `mdbook build docs` -- confirms the book still builds and no cross-links broke
  (none are added/removed here, but this is the project's doc gate per
  AGENTS.md / `docs/book.toml` with `mdbook-linkcheck2`).
- No tests to run: docs-only change, no code path touched.
