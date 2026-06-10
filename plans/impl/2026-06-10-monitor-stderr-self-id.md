# Plan: self-identify monitor's headless stderr lines

## Context

`braid monitor` is a headless systemd-timer surface ("you normally don't run
this by hand" -- `docs/commands/monitor.md`); its stderr lands in
`journalctl -u braid-monitor.service`, the journal `docs/guides/monitoring-and-alerts.md`
tells operators to grep for triage. On a fail-closed cycle
(probe/parse/stats/mountinfo/acked-stats/latch failure) `cmd_monitor` prints a
detail, latches an `AlertCause::ComputationError`, and returns
`MonitorResult::Alert` -> exit 1 -> the wrapper starts the beeper.

Three stderr sites reachable from `cmd_monitor`'s fail-closed paths are
unqualified:

- `cli/src/monitor.rs#cmd_monitor` prints `error: {detail}` on the classified
  failure arm. The wrapper's *other* failure path (`modules/braid/monitor.nix`,
  exit >= 2) echoes `braid monitor failed (exit $rc)`. So this one unit reports
  its two failure paths inconsistently: exit 2 names `braid monitor`, exit 1
  (the common runtime path) emits a bare `error:` that names neither binary nor
  subcommand in the message body.
- `cli/src/monitor.rs#cmd_monitor` prints `Warning: failed to write alert latch:
  {e}` -- same unqualified-prefix problem. This line is also the *only* trace
  that a latch write failed; when it does, `braid status` can't display the
  alert, so the breadcrumb matters.
- `cli/src/alert.rs#load_alert_latch_or_quarantine` -- the helper `cmd_monitor`
  calls to load the prior latch -- prints `warning: alert latch unreadable --
  quarantining: {parse_detail}` when the latch is corrupt. This is a *documented
  fail-closed beep path* (corrupt latch -> `ComputationError` -> exit 1; pinned
  by `cmd_monitor_corrupt_alert_latch_latches_computation_error`). On a
  pure-corrupt-latch cycle (healthy probe/stats) it is the *only* stderr
  breadcrumb, because the primary `error:` line above does not fire
  (`failure_detail` is `None`). Its prefix is unqualified.

Inventory note: these three are the complete set. Greps for
`eprintln!`/`println!`/`print!` across every `cmd_monitor`-reachable file
(`monitor.rs`, `alert.rs`, `probe.rs`, the btrfs-stats parser, `cmd.rs`,
`state_paths.rs`) surface no others. `parse/smartctl.rs` ("SKIP: fixture not
captured yet") is fixture-capture tooling monitor never reaches (it parses btrfs
stats, not smartctl), and `cmd.rs`'s dry-run preview `print!` is unreachable
from monitor (a real detector, never dry-run).

The other two headless surfaces already self-identify: `online_state.rs` (7/7)
and `mountpoint_guard.rs` (2/2) prefix with `braid: ` / `braid: WARNING: `.
monitor is the lone drifting headless surface, and it drifts at all three sites
(two in `monitor.rs`, one in the latch helper it calls).

This is a Low/cosmetic **correctness** fix. Exit codes, the `ComputationError`
latch, the beep, quarantine behavior, and `braid status` are all unchanged --
only the journal message text changes. No ADR mandates a journal-line format
(ADR 014 / ADR 018 define exit code + latch, not stderr); the latch is the
load-bearing surface.

## Decision

Prefix all three sites with `braid monitor:` so the message body self-identifies
the command.

Why `braid monitor:` and not bare `braid:` (the sibling form):

1. **Within-unit coherence.** The wrapper already says `braid monitor failed`;
   the CLI's exit-1 lines should agree. Bare `braid:` would fix "unqualified" but
   create a *new* exit-1-vs-exit-2 mismatch for the same unit.
2. **Maximal self-id, same convention.** The siblings use bare `braid:` only
   because they are shared library code (`mark_online`, `seal_offline_mountpoint`)
   invoked under many subcommands and cannot name one. `cmd_monitor` always knows
   it is `monitor`. Same `braid <context>:` shape, more specific context because
   it is available. It also names the one thing `journalctl -u` scopes but the
   message body omits (the subcommand).

Where the third line is emitted: move it *out* of the generic
`alert.rs#load_alert_latch_or_quarantine` and *into* `cmd_monitor`, rather than
baking a `braid monitor` prefix into the helper. The helper is `pub`, and its
doc comment already frames it as "return a detail string so the caller can plant
a `ComputationError` cause" -- the `eprintln!` was an undocumented side effect.
`ack` deliberately does *not* call it (the monitor/ack quarantine asymmetry is a
pinned invariant: `cmd_ack_mounted_corrupt_latch_does_not_quarantine_when_cleanup_fails`),
so a command-specific prefix inside the shared primitive would be wrong. monitor
is its sole non-test caller and already receives the detail it needs to print.

Severity stays visible across the sites:

- The primary fail-closed error leads with the detail, no severity word: several
  details already contain "error" (`mountinfo error -- ...`), so an `error:` tag
  would stutter, and the detail reads as a failure on its own.
  -> `braid monitor: {detail}`.
- The two secondary, non-fatal notes -- the latch-*write* failure in
  `cmd_monitor`, and the latch-*read*/quarantine warning relocated from the
  helper -- keep a lowercase `warning:` tag, so an operator distinguishes "failed
  closed, beeping" from "ran fine but couldn't persist/parse the latch."
  Lowercase matches the more common `main.rs` / `ack.rs` / `alert.rs` casing (the
  uppercase `WARNING` is the shared-helper sibling form we are deliberately not
  adopting here).

Not adopted: `status_tag.rs` (`status_line`/`[wait]`/`[ok]`). That is the
*interactive* surface (Principle 13: 7-column tags, color, terminal padding) and
is wrong for headless journal lines.

## Scope

`cli/src/monitor.rs#cmd_monitor` (the two direct lines) plus the emission
relocated from `cli/src/alert.rs#load_alert_latch_or_quarantine` into
`cmd_monitor`. Scope is defined by reachability, not by file: every
stderr/stdout site on `cmd_monitor`'s fail-closed paths, which the completed
inventory shows is exactly these three.

Out of scope: the ~9 sibling sites in `online_state.rs` / `mountpoint_guard.rs`
-- already correct, and bare `braid:` is right for them (shared code serving
other units). `modules/braid/monitor.nix` is unchanged: the exit-1 branch stays
quiet because the CLI is now verbose *and* self-identifying, and the exit-2
branch's `braid monitor failed` now harmonizes with the CLI lines.

Deferred (not now): a shared headless-log helper. With the prefix
context-dependent (`braid:` for shared code, `braid monitor:` for command code),
such a helper must be parameterized on context -- disproportionate machinery for
a Low cosmetic fix over currently-correct code. Revisit only if a third headless
*surface* (not a third call site of monitor's own path) drifts.

## Changes

`cli/src/monitor.rs` (`cmd_monitor`) -- two one-line prefix edits, no logic
change:

- `eprintln!("error: {detail}")` -> `eprintln!("braid monitor: {detail}")`
- `eprintln!("Warning: failed to write alert latch: {e}")`
  -> `eprintln!("braid monitor: warning: failed to write alert latch: {e}")`

`cli/src/alert.rs` (`load_alert_latch_or_quarantine`) -- remove the
`eprintln!("warning: alert latch unreadable -- quarantining: {parse_detail}")`.
The function keeps building and returning the detail; it just stops writing to
stderr, which makes it match its own doc comment. Update the `///` to note the
caller owns surfacing.

`cli/src/monitor.rs` (`cmd_monitor`) -- relocate that emission, qualified, right
after the `load_alert_latch_or_quarantine` call (before the fold), keyed on the
detail the helper already returns:

    let (existing_latch, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);
    if let Some(detail) = &latch_corrupt_detail {
        eprintln!("braid monitor: warning: alert latch unreadable -- quarantining: {detail}");
    }

Print the full returned `latch_corrupt_detail`: in the common first-corruption
case it equals the old `parse_detail`; in the rare repeat-corruption case it also
carries the sidecar-preservation suffix -- strictly more forensic detail in the
journal, no behavior change.

Unchanged: the `Err(detail)` classification arm, `folded_computation_error_detail`
and the latch fold (the corrupt-latch detail still folds into one
`ComputationError`), exit mapping in `main.rs`, and the systemd wrapper.

## Tests

No new test. This is cosmetic journal text; the load-bearing content (the detail
string) is already pinned via the `ComputationError` latch assertions in
`monitor.rs` -- e.g. `cmd_monitor_latches_computation_error_on_mountinfo_io_failure`,
`save_acked_stats_failure_latches_computation_error`,
`stats_failure_with_corrupt_alert_latch_folds_one_computation_error`,
`cmd_monitor_corrupt_alert_latch_latches_computation_error`.

Removing the helper's `eprintln!` breaks nothing: its tests
(`quarantine_moves_corrupt_file_aside_and_reports_detail`,
`quarantine_preserves_first_corrupt_sidecar`) assert on the *returned* `(state,
detail)` tuple and the sidecar bytes, never on stderr.

No existing test pins these monitor/helper lines as positive output either.
Several VM tests *do* capture and assert stderr -- e.g. `tests/cli/braid-lock.py`,
`tests/cli/braid-add-warnings.py`, `tests/cli/replace-preview-warnings.py` -- but
on `lock`/`add`/`replace` surfaces, not monitor's. Grepping `tests/` for the
distinctive substrings of the three changed lines (`failed to write alert latch`,
`alert latch unreadable`, `quarantining`) finds exactly one hit, and it is a
*negative* assertion: `tests/module/alert-state-lock.py` (flake check
`alert-state-lock`, run via `just test-vm alert-state-lock`, not `just
test-rust`).

That test ("monitor skips silently while pool lock is held") writes a corrupt
latch, starts `braid-monitor.service` while another process holds the pool lock,
and asserts both `"quarantining" not in journal` and `"braid monitor failed" not
in journal`. The relocation keeps it green by construction: under the
`MonitorSilent` lock policy a contended lock makes `main.rs` `exit(0)` at the
acquire step (`acquire_per_policy`) *before `cmd_monitor` runs at all*, so
`load_alert_latch_or_quarantine` is never called -- the same reason the test
asserts the corrupt latch is left un-quarantined (no `.corrupt` sidecar). The
relocated `eprintln!` is gated on the exact `Some(detail)` the helper returns, so
it fires under conditions identical to the in-helper line it replaces: text and
call site change, emit-conditions do not. (The later lock-*released* run does
quarantine and now logs `braid monitor: warning: alert latch unreadable -- ...`,
but no assertion reads the journal after that point; and `braid monitor:
{detail}` does not contain the substring `braid monitor failed`, so the primary
line's new prefix can't trip the `:229` assertion either.)

Quarantine behavior, the returned detail, the fold, exit codes, and the beep are
all unchanged, so there is nothing behavioral and structure-insensitive to add. A
test asserting a literal stderr prefix would be brittle and structure-sensitive
(the rubric's "don't pin cosmetic format" case).

## Verification

- `just test-rust` -- the monitor and alert *unit* suites stay green; they pin
  the `ComputationError` detail and the helper's returned tuple/sidecar, not
  stderr, so green confirms the logic is untouched.
- `just test-vm alert-state-lock` -- the one VM test that references a changed
  string (asserts `"quarantining" not in journal` in the lock-held phase) stays
  green; the relocation cannot flip it (see Tests for the lock-skip-before-latch
  argument). `just test-rust` does not run module VM tests, so this is a
  separate, required step.
- `scripts/docs/check-output-ascii.py` over `cli/src/**/*.rs` (see the `justfile`
  recipe) -- the new `braid monitor:` / `warning:` lines are plain ASCII and the
  details already use `--`; confirm the gate passes.
- Eyeball the rendered lines against representative details:
  - `braid monitor: mountinfo error -- Permission denied`
  - `braid monitor: acked-stats unreadable -- <io error>`
  - `braid monitor: warning: failed to write alert latch: <io error>`
  - `braid monitor: warning: alert latch unreadable -- quarantining: parse alert latch: <err>`
- Optional end-to-end (not required for merge; unit behavior is unchanged):
  - force a fail-closed cycle and confirm `journalctl -u braid-monitor.service`
    shows the `braid monitor:`-prefixed line, consistent with the wrapper.
  - write garbage to `alert-latch.json` with a healthy mounted pool, run
    `braid monitor`, and confirm the journal's *only* breadcrumb is the single
    qualified `braid monitor: warning: alert latch unreadable -- quarantining:
    ...` line.
