# Plan: document ack's cleanup-pending sentinel in the manual

## Context

The `alert-cleanup-pending` sentinel landed in
`plans/impl/2026-05-19-ack-cleanup-pending-sentinel.md` and is now the
load-bearing retry signal after an ack cleanup I/O error: ack writes it
after `stop_beeper` and before any destructive removal
(`cli/src/ack.rs:208`), `cmd_ack_impl` snapshots it at entry and runs a
hoisted cleanup-only branch *before* `probe_pool_alerts`
(`cli/src/ack.rs:48,52-58`), and `resolve_alert_state` surfaces it on
`braid status` as a `ComputationError` with the literal detail
`ack cleanup pending -- re-run \`braid ack\` to resume`
(`cli/src/status.rs:594,615`).

`manual/commands/ack.md:43` mentions cleanup-error recovery in a single
vague sentence ("ack preserves retry state") but never names the
sentinel, never tells the operator what `braid status` will display
while recovery is pending, and never warns that the sentinel-only retry
prints `acknowledged current alerts` even though no actionable alert
was latched -- only the residual cleanup ran. The plan that introduced
the sentinel deliberately reused that success string for code
simplicity; the manual is the right place to disambiguate.

Per AGENTS.md, the manual must be kept current when behavior changes;
this is the only manual gap left over from the sentinel work. No
sibling page needs an update: `monitor.md` and
`guides/monitoring-and-alerts.md` correctly stay above the level of
ack-internal state files, and a grep across `manual/**/*.md` confirms
no other page mentions cleanup-pending.

Intended outcome: an operator who hits a cleanup I/O error can (a)
recognize the `ack cleanup pending` cause on `braid status` as
expected and not a new fault, (b) name the sentinel file when
diagnosing or debugging, and (c) read the retry's `acknowledged
current alerts` output without thinking something is still wrong.

## Change

Single docs edit. Insert one short paragraph into
`manual/commands/ack.md` between the current line 43 ("On a cleanup
I/O error...") and the offline-pool paragraph (line 45). No
subsection header -- the existing section uses bare paragraphs and a
new h3 would diverge from the page's style. No changes to code,
tests, decision records, or other manual pages.

Insert this paragraph after line 43, separated by a blank line on
each side:

> When ack reaches cleanup and a later cleanup step fails, it leaves
> `/var/lib/braid/alert-cleanup-pending`. `braid status` surfaces
> `ack cleanup pending -- re-run \`braid ack\` to resume` as an alert
> cause until cleanup finishes. If that sentinel is the only
> remaining alert signal, the next `braid ack` re-enters cleanup
> directly (no btrfs probe, no baseline rewrite) and prints
> `acknowledged current alerts` on success -- expected output because
> only leftover cleanup ran.

The "if that sentinel is the only remaining alert signal" clause is
load-bearing: the direct-cleanup branch in `cmd_ack_impl` is gated on
`cleanup_pending && causes.is_empty() && !smartd_active &&
!latch_corrupt` (`cli/src/ack.rs:52`). When other live signals
remain, ack runs the regular path (probe + baseline rewrite + tail
cleanup) and clears the sentinel as a side effect. The manual must
not promise the "no probe, no baseline rewrite" shortcut
unconditionally.

ASCII only (`--` not em-dash), naming the sentinel with the full path
to match how `cli/src/state_paths.rs:44-45` resolves it on disk, and
quoting the `braid status` detail and the retry's success line
verbatim from `cli/src/status.rs:594` and `cli/src/ack.rs:56`.

## Critical files

- `manual/commands/ack.md` -- the only file edited; insert the
  paragraph at line 44 (between the cleanup-I/O sentence and the
  offline paragraph).

## Reused references (read-only confirmation, not edited)

- `cli/src/state_paths.rs:44-45` -- canonical sentinel path
  `/var/lib/braid/alert-cleanup-pending`.
- `cli/src/status.rs:594,615` -- exact `ComputationError` detail
  string.
- `cli/src/ack.rs:48,52-58,111` -- the hoisted cleanup-only retry
  branch and the success message both branches print.
- `plans/impl/2026-05-19-ack-cleanup-pending-sentinel.md` and
  `docs/decisions/014-alerts.md:47,117-119` -- design context for the
  sentinel and the deliberate message reuse.

## Verification

Pure docs change; no code or test runs needed.

1. `git diff manual/commands/ack.md` -- diff is exactly one inserted
   paragraph plus surrounding blank lines.
2. Read the rendered section: the paragraph sits between the existing
   "On a cleanup I/O error..." sentence and the "If the pool is
   offline but alerts exist..." paragraph; the page still has no h3s
   inside `## What happens under the hood`.
3. Spot-check the three quoted strings against source:
   - Sentinel path matches `cli/src/state_paths.rs:44-45`.
   - Status detail matches `cli/src/status.rs:594` literally.
   - Retry success line matches `cli/src/ack.rs:56` literally.
4. Confirm the "no btrfs probe, no baseline rewrite" claim is scoped
   to the sentinel-only branch: the paragraph's "If that sentinel is
   the only remaining alert signal" clause must match the gating
   condition `cleanup_pending && causes.is_empty() && !smartd_active
   && !latch_corrupt` at `cli/src/ack.rs:52`.
5. Confirm ASCII discipline: no em-dashes, no curly quotes, no
   backticks left unescaped inside the rendered code spans.

## Out of scope

- Changing the retry's `acknowledged current alerts` message. The
  sentinel-design plan accepted message reuse as a tradeoff; the
  finding pitched a docs fix, not a code fix, and revisiting that
  choice would re-litigate a recent decision for a Low-severity UX
  smell.
- Cross-referencing the sentinel from
  `guides/monitoring-and-alerts.md` or other guide pages. Those pages
  intentionally stay above the level of ack-internal state files.
- Any update to `docs/decisions/014-alerts.md` -- it already
  documents the sentinel; the manual is the only missing surface.
