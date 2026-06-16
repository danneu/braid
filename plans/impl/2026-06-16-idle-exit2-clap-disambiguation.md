# Plan: clarify the shared exit-2 / clap-usage case in `braid idle` docs

## Context

A review finding flagged that `docs/commands/idle.md` presents exit 2 as
unambiguously "config could not be read," when in fact exit 2 is *shared*
between two sources:

1. braid's deliberate config-load failure -- `load_config_or_exit(&path, 2)`
   in `cli/src/main.rs` (Idle arm), whose `ConfigError::{Read,Parse}` Display
   (`cli/src/config.rs`) embeds the config-file path, printed to stderr via
   `print_cli_error` as `error: failed to {read,parse} config file <path>: ...`.
2. clap's default usage-error code. Verified empirically against the built
   binary: `braid idle --badflag` (unknown flag) and `braid idle --config`
   (missing value) both exit **2**. Their stderr format *varies* -- the
   unknown-flag case prints a `Usage: braid idle ...` line, the missing-value
   case prints only `error: a value is required ... / For more information, try
   '--help'.` with no `Usage:` line -- but neither prints the
   `failed to read/parse config file <path>` message a config-load exit 2 emits.
   That absence is the stable distinguisher (clap's `Usage:` line is not).

Every `cmd_idle` *probe* failure maps to `Busy(BusyReason::Unknown)` -> exit 1
(`cli/src/idle.rs`, `cli/src/main.rs` Idle arm), so exit 2 is never reachable
from a probe. The VM test `tests/cli/braid-idle.py:30-41` already relies on this
disambiguation: it asserts the injected config path appears in output
(`"/tmp/bad.json" in output`) precisely because the exit code alone cannot
distinguish a config-load exit 2 from a clap usage exit 2.

**Why this is the only doc that needs changing.** braid frames exit 2 as
"braid's own setup phase failed" -- `monitor.md` literally says "*Pre-monitor*
setup error (e.g. ...)". Under that framing the exit-code *tables* are correct
as written: `ack.md` is a bare headline table and `monitor.md` hedges with
"e.g.", so neither claims to enumerate every observable exit. `idle.md` is
different: it carries a prose paragraph (the end of the Exit codes section) that
*does* teach the reader to tell every exit apart by its stdout/stderr signature,
and that enumeration is the one place where omitting clap is genuinely
incomplete. Fixing exactly that over-claim is both the smallest and the
most-correct change.

## Scope decision

- **In scope:** the stream-disambiguation paragraph at the end of
  `docs/commands/idle.md`'s `## Exit codes` section.
- **Deliberately NOT changed** (correct under braid's "exit 2 = braid's setup
  failure" framing; adding a framework-level clap caveat would conflate
  framework behavior with command semantics and erode terse-table clarity):
  - `docs/commands/idle.md` exit-code *table* row (`| **2** | Setup error --
    config could not be read |`) -- describes braid's intentional exit 2; stays.
  - `cli/src/main.rs` Idle `--help` text (`... exit 2 = setup error`) -- accurate,
    not claiming exhaustiveness.
  - `docs/commands/ack.md`, `docs/commands/monitor.md` exit-code tables.
  - `docs/design/decisions/016-auto-suspend.md` exit-code table (design intent).

## The change

File: `docs/commands/idle.md`, the paragraph that currently ends:

> ... and config-load exit 2 emits a config-error diagnostic.

Append a note covering the clap case and reassuring the integration audience.
Draft wording (final prose can be tightened at implementation; keep ASCII). Key
the distinction on the stable invariant -- the absence of the config-file
diagnostic -- NOT on clap's `Usage:` line, which clap prints for an unknown flag
but omits for a missing value:

> Argument errors (an unknown flag, a missing value) also exit 2, but clap
> writes its own `error:` diagnostic to stderr and never the
> `failed to read/parse config file <path>` line a config-load exit 2 prints --
> so the stderr text still tells the two exit-2 cases apart. The autosuspend
> check runs a fixed argument list, so its only exit 2 is a config-load failure.

The second sentence matters: the documented integration (autosuspend's
`braid idle` with fixed argv -- see `idle.md` "Autosuspend integration" and
ADR 016) can never hit a clap usage error, so for the doc's primary audience
exit 2 remains unambiguously config-load. The note completes the disambiguation
guide without implying the integration is ambiguous.

## Verification

- **Runtime spot-check.** The config-load side mirrors
  `tests/cli/braid-idle.py:30-41` (already passing). The clap side stays a
  docs/runtime check only -- do NOT add an exact-output assertion (`Usage:` text)
  to the VM test, since clap's format is version- and error-kind-dependent:
  - `braid idle --badflag` (unknown flag) -> exit 2; stderr has an `error:`
    diagnostic and no `failed to read/parse config file` line.
  - `braid idle --config` (missing value) -> exit 2; same `error:` diagnostic, no
    config-file line, and -- unlike the unknown-flag case -- no `Usage:` line.
    This is the case that proves the prose must not promise a `Usage:` line.
  - `braid idle --config /tmp/nonexistent.json` -> exit 2; stderr names the path
    (`failed to read config file /tmp/nonexistent.json`).
  - `braid idle --config /tmp/bad.json` (malformed JSON) -> exit 2; stderr names
    the path (`failed to parse config file /tmp/bad.json`).
- **No code/test change.** `braid-idle.py` already pins the config-load side of
  the disambiguation; this change only brings the prose in line with behavior the
  test already verifies.
- **Docs build:** run `just docs-build` -- the change adds no links, so this is a
  sanity check that `mdbook-linkcheck2` still passes.
- **ASCII:** keep the added prose ASCII-only (`--`, `'`/`"`, `...`), per project
  convention.
