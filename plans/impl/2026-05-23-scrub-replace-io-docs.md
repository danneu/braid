# Plan: docs upgrade for scrub/replace I/O impact

## Context

`/verify-issue` ran against an `ultrareview`-style finding ("No guidance on
scrub and replacement in production NAS without downtime windows"). The
finding's premise -- that operators don't know how to keep scrubs from
interfering with NAS workloads -- is partly stale: braid already sets
`Nice=19` and `IOSchedulingClass=idle` on `braid-scrub.service` by default
(`modules/braid/storage.nix:89-90`, pinned by
`tests/module/auto-scrub.py:92-98`), and the scrub interval is already
operator-configurable via `braid.autoScrub.interval`. What is missing is
the *advertisement* of those facts where an operator would look: the
day-to-day guide and the recovery guide. The risk is misframed: not
"operators must configure idle priority themselves" but "operators don't
know braid already asks for low priority, so they assume scrubs will block
media streaming and either disable the timer or schedule downtime windows
they don't need."

The framing has to be honest about a block-layer limitation without
publishing a scheduler matrix that the docs would then have to keep
chasing across kernel versions: I/O-priority handling is scheduler-
and block-layer-dependent (the pinned kernel docs at
`reference/linux/Documentation/block/ioprio.rst:11` are explicit that
"support for io priorities is io scheduler dependent"; the exact set of
schedulers that honor `IOPRIO_CLASS_IDLE` evolves over time, and
`mq-deadline` currently maps it to `DD_IDLE_PRIO` at
`reference/linux/block/mq-deadline.c:108-113`). So braid's
`IOSchedulingClass=idle` is best-effort -- the strength depends on the
operator's hardware, kernel, and block-layer configuration. The CPU-side
`Nice=19` always applies. The docs must say "best-effort" plainly
without claiming a definitive scheduler matrix.

`braid replace` is also asymmetric and was wrongly framed as "background"
in earlier docs: `cli/src/cmd.rs:787` passes `-B` to `btrfs replace
start`, and `run_replace_with_progress` (`cli/src/progress.rs:980+`)
polls `btrfs replace status` in the foreground until completion. So
`braid replace` is a long-running foreground command that keeps the pool
online -- not a fire-and-forget background job. There is no priority
lever applied around it (no `ionice`/`nice` shim). The honest framing is
"online operation; command stays attached; pool stays usable; progress
visible from another shell."

Outcome: an operator reading `day-to-day-nas-usage.md` understands the
default scrub stance and its best-effort caveat in one short paragraph,
without having to dig into kernel I/O-scheduler docs to learn what
`IOSchedulingClass=idle` actually buys them; an operator reading
`recovery-scenarios.md` understands that `braid replace` is a foreground
command that runs for hours but leaves the pool online, and plans to
keep a shell open (or `tmux`/`screen`).

Explicitly out of scope: a new "Performance and scheduling" page (would be
a single short page parking lot), a scrub-duration estimate table (drive-,
fullness-, and concurrency-dependent guesswork that ages badly), and any
code change to add `IOSchedulingClass=idle` / `ionice` around `btrfs
replace start` (separate decision -- this plan is docs only).

## Files to modify

### 1. `docs/guides/day-to-day-nas-usage.md` -- extend the "Let scrubs complete" bullet

Current bullet (line 168):

> **Let scrubs complete** -- braid runs monthly scrubs by default. Scrubs
> verify every block's checksum and repair corruption from redundant copies.
> Do not interrupt them.

Replace with (preserving the existing wording, appending the
best-effort framing and a retiming pointer):

> **Let scrubs complete** -- braid runs monthly scrubs by default. Scrubs
> verify every block's checksum and repair corruption from redundant
> copies. braid starts them at low CPU priority (`Nice=19`) and idle I/O
> priority (`IOSchedulingClass=idle`). The CPU priority always applies;
> the I/O priority is best-effort -- how strongly the kernel honors it
> depends on your block-layer I/O scheduler -- so do not treat it as a
> guarantee that scrubs will never affect interactive workloads. The
> pool stays online throughout. If scrubs noticeably impact Samba, NFS,
> or local use on your hardware, retime them with
> `braid.autoScrub.interval` (any systemd calendar expression -- e.g.
> `"Sun *-*-* 02:00:00"`) to land in an off-peak window. Do not
> interrupt a scrub in progress.

Voice notes:
- Honest framing: "best-effort", not "you don't have to schedule a
  maintenance window" (the prior draft over-promised). No scheduler
  names in the prose; the kernel docs are clear that the exact list of
  schedulers honoring `IOPRIO_CLASS_IDLE` is itself a moving target,
  and we don't want the guide to become a stale scheduler matrix.
- Plain ASCII per project house style (`--`, not em-dash).
- Keep the `Nice=19` / `IOSchedulingClass=idle` quoted verbatim so a
  curious operator can grep code and unit output to verify -- but the
  *interpretation* of what the I/O class buys is left as "best-effort,
  depends on your scheduler" with no named winners and losers.

### 2. `docs/guides/recovery-scenarios.md` -- rewrite the "Replace runs" paragraph

Current text (line 273):

> Replace runs `btrfs replace` under the hood. This is a background
> operation that can take hours for large drives. Progress is visible in
> `braid status` and `braid tui`.

The "background operation" wording is inaccurate: braid invokes `btrfs
replace start -B` (`cli/src/cmd.rs:787`) and `run_replace_with_progress`
(`cli/src/progress.rs:980+`) keeps polling `btrfs replace status` in the
foreground until the kernel reports FINISHED. The command stays attached
to your terminal. Rewrite the paragraph to match:

> Replace runs `btrfs replace start -B` under the hood. `braid replace`
> is a long-running online operation: the command waits in the
> foreground and shows progress while the pool remains usable. It can
> take hours for large drives, so run it from a shell you can leave open
> (or a `tmux`/`screen` session). From another shell, `braid status` and
> `braid tui` can show progress independently.

Voice notes:
- Wording closely follows the reviewer's proposed Fix.
- No claim about I/O priority asymmetry with scrub: scrub's own priority
  is now framed as best-effort, so contrasting "replace runs at normal
  I/O priority" would imply a meaningful difference that the kernel may
  not actually deliver on `mq-deadline`. Drop the comparison; let each
  paragraph stand alone.
- `tmux`/`screen` reference is the same operator-cookbook tone used
  elsewhere in `recovery-scenarios.md`.

### 3. `docs/guides/troubleshooting.md` -- no change

Line 111 already says "It takes hours for large disks" in the same
voice. No need to duplicate the priority caveat here -- the recovery
guide is the canonical location.

### 4. `docs/guides/nixos-configuration.md` -- no change

The auto-scrub option table (line 74-83) is reference material. The
existing "lifecycle-aware" prose is enough; cross-referring the priority
defaults from here would be redundant with the day-to-day guide and would
clutter the option table.

### 5. `docs/guides/monitoring-and-alerts.md` -- no change

The finding nominated this file, but it covers *alerting*, not scrub
scheduling or I/O behavior. Adding scrub-priority copy here would muddy
the guide.

## What I deliberately did *not* add

- A scrub-duration estimate table (e.g. "12 TB at 100 MB/s = ~33 hours").
  Real scrub speed is dominated by drive throughput, fullness,
  fragmentation, and any concurrent I/O. A static table will be wrong
  for any specific pool and will rot. The "monthly default +
  best-effort low priority" framing is more durable.
- A new "Performance and scheduling" page in `docs/SUMMARY.md`. Two
  short paragraphs across two existing guides is the right granularity;
  a page would be padding.
- Any mention of `CPUSchedulingPolicy=idle` (the silvenga reference's
  other lever). braid uses `Nice=19` instead, which is approximately
  equivalent for the "yield CPU to interactive workloads" goal and is
  what the code actually does. Documenting a lever braid doesn't use
  would be misleading.
- A block-scheduler tuning recommendation (e.g. "switch to bfq").
  Pushes braid beyond its product boundary, and the kernel docs make
  clear the set of schedulers honoring `IOPRIO_CLASS_IDLE` evolves over
  time, so any specific recommendation we make now will age. The docs
  say "best-effort, depends on your scheduler" and leave host tuning to
  the operator.
- A code change to add `ionice` / `IOSchedulingClass=idle` around `btrfs
  replace start`. Separate decision (replace is meant to finish *fast*
  to restore redundancy; deliberately slowing it down to yield to user
  I/O is a tradeoff worth its own ADR, and the kernel-scheduler caveat
  above applies to replace too).

## Verification

1. `mdbook build docs` -- the unified docs tree builds and
   `mdbook-linkcheck` passes (no new cross-links added, but the build
   exercises the markdown).
2. `mdbook serve docs` and visually inspect the two paragraphs in a
   browser to confirm rendering (no broken backticks, no Unicode
   substitutes slipped in).
3. Grep verification:
   ```
   rg -n 'IOSchedulingClass|Nice=19' docs/guides/
   ```
   Should show exactly one hit in `day-to-day-nas-usage.md` (and zero
   elsewhere in `docs/guides/`), matching the single source of truth
   for the user-facing priority claim.
4. Grep verification for the replace rewrite:
   ```
   rg -n 'background operation' docs/guides/recovery-scenarios.md
   ```
   Should show zero hits (the inaccurate phrase is removed). And:
   ```
   rg -n 'replace start -B' docs/guides/recovery-scenarios.md
   ```
   Should show exactly one hit, matching `cli/src/cmd.rs:787`.
5. Cross-check against `modules/braid/storage.nix:89-90` and
   `tests/module/auto-scrub.py:92-98` that the `Nice=19` and
   `IOSchedulingClass=idle` values quoted in the doc still match the
   code and its test. If a future commit changes either, this doc claim
   needs to change with it (a follow-up could add a `// keep in sync`
   pointer at `storage.nix:89-90`, but that's out of scope for this
   plan). No scheduler matrix to spot-check -- the prose deliberately
   stays generic ("depends on your block-layer I/O scheduler") so the
   docs do not need to be revisited every time the kernel adds or
   removes priority handling in a given scheduler.
