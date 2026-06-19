# Decouple btrfs scrub terminal-state classification from a parseable `started_at`

## Context

`cli/src/parse/btrfs_scrub_status.rs#parse_btrfs_scrub_status` classifies the
terminal scrub state (`finished` / `aborted` / `interrupted`) *inside*
`if let Some(started_at) = acc.started_at { ... }`. `acc.started_at` is `None`
whenever the `Scrub started:` / `Scrub resumed:` timestamp fails to parse
(`helpers.rs#parse_ctime` uses a hardcoded English ctime `format_description!`
and returns `None` on any mismatch) or the start line is absent. When that
happens the authoritative `Status:` word is ignored and the function falls
through to `ScrubState::Unknown`.

The `Status:` word -- not the timestamp -- is the authoritative signal for
whether a scrub is resumable. The current coupling has two concrete
consequences when a terminal scrub has an unparseable/missing start time:

- `scrub_needs_resume.rs#cmd_scrub_needs_resume` maps `ScrubState::Unknown` to
  `Err(ScrubNeedsResumeError::StatusUnknown)`, hard-failing the
  `braid-scrub-resume-trigger.service` path and stranding resumable progress.
- `idle.rs#cmd_idle` maps `ScrubState::Unknown` to `Busy(Unknown)` and blocks
  auto-suspend on a scrub that is actually terminal (safe to suspend).

This is latent today because `cmd.rs` pins `LC_ALL=C`, so btrfs emits English
ctime and `parse_ctime` succeeds. It is structural fragility, not a live bug:
any future btrfs-progs format drift, a `Scrub started: not available` line, or a
sparse terminal block re-introduces it. A missing *display* timestamp must never
flip a *resumable* state into a hard error.

**Goal:** classify the terminal state from the `Status:` word alone, regardless
of whether `started_at` parsed -- mirroring how `Running` already tolerates
`started_at: None`. Preserve all existing behavior: `no stats available` ->
`Never`; empty output -> `Unknown`; an unrecognized status word (`weird`) ->
`Unknown`.

This change makes the code match a contract the docs *already* state: ADR 016's
fail-closed clause defines `ScrubState::Unknown` as "empty stdout or an
unrecognized `Status:` word" -- which is exactly the post-fix set of Unknown
causes. The fix removes the undocumented third cause (terminal word + unparseable
timestamp).

## Design decision (recommended approach)

Make `started_at` an `Option<ScrubTimestamp>` on the `Finished` / `Aborted` /
`Interrupted` variants of `ScrubState`, and lift the `match acc.status` classifier
out from under the `if let Some(started_at)` guard. This mirrors the already-
`Option` `ScrubState::Running.started_at` and keeps state determination
orthogonal to timestamp parsing.

**Why this over the alternatives:**

- *Sentinel/default `ScrubTimestamp` (keep field required):* rejected. Any
  placeholder (e.g. epoch) renders as a real-looking but wrong date in `status`
  and TUI output and would leak into `--json` `started_at` as a valid-looking
  ISO string -- actively misleading. `Option` is the honest representation and
  the field is genuinely absent.
- *Making `parse_ctime` multi-locale/format-tolerant:* out of scope and
  unnecessary given `LC_ALL=C`. It would not fix the structural coupling -- a
  missing start line still yields `None` regardless of how tolerant the parser
  is. The decoupling is the correct robustness fix; broadening `parse_ctime` is a
  separate, larger change.

The reporting DTO `status.rs#ScrubReport` already pre-formats the timestamp into
three derived strings (`started_at` ISO for JSON, `started_at_human` and
`journal_since` serde-skipped for the renderer). All three share the same
Some/None fate, so they all become `Option<String>` and are populated together.
JSON omits `started_at` when `None` via `skip_serializing_if = "Option::is_none"`
-- the same pattern `ScrubReport::Running { pct: Option<u8> }` already uses in
this enum.

## Implementation steps

### 1. Type change -- `cli/src/parse/types.rs#ScrubState`

Change `started_at: ScrubTimestamp` to `started_at: Option<ScrubTimestamp>` on
the `Finished`, `Aborted`, and `Interrupted` variants (leave `Running`, which is
already `Option`, untouched). No doc comment exists on these variants; if one is
added, state the invariant: "terminal state is classified from the `Status:`
word; `started_at` is `None` when btrfs reported no parseable start time."

### 2. Parser control flow -- `cli/src/parse/btrfs_scrub_status.rs#parse_btrfs_scrub_status`

Lift the classifier out of the `if let Some(started_at)` guard. After the
`running` early-return, bind the start time once and let every terminal arm carry
it as `Option`:

```rust
let started_at = acc.started_at; // Option<ScrubTimestamp>, moved out before the match
let state = match acc.status.as_deref() {
    Some("finished") => ScrubState::Finished {
        started_at,
        error_count: acc.error_count,
        duration_secs: acc.duration_secs,
        total_bytes: acc.total_bytes,
        rate_bytes_per_sec: acc.rate_bytes_per_sec,
    },
    Some("aborted") => ScrubState::Aborted {
        started_at,
        error_count: acc.error_count,
        duration_secs: acc.duration_secs,
        total_bytes: acc.total_bytes,
        rate_bytes_per_sec: acc.rate_bytes_per_sec,
    },
    Some("interrupted") => ScrubState::Interrupted {
        started_at,
        error_count: acc.error_count,
        duration_secs: acc.duration_secs,
        total_bytes: acc.total_bytes,
        rate_bytes_per_sec: acc.rate_bytes_per_sec,
    },
    _ => ScrubState::Unknown,
};
Ok(BtrfsScrubStatusOutput { state })
```
(All fields are spelled out per arm: `{ started_at, .. }` is pattern syntax and
will not compile in struct construction. `started_at` is non-`Copy` so it is
moved into whichever single arm runs; the sibling fields are `Copy` and the `_`
arm drops the local.)

(Binding `started_at` to a local before the match sidesteps any field-borrow
friction with `acc.status.as_deref()`; only one arm runs, so the single move is
fine, and the `_` arm simply drops it.) Update the function's `///` to state the
invariant: terminal classification depends only on the `Status:` word; a missing
or unparseable start timestamp yields the terminal variant with `started_at:
None`, never `Unknown`. Behavior preserved: `None`/`weird` status -> `_ =>
Unknown`; `no stats available` -> `Never` (unchanged early return).

### 3. Reporting DTO + renderer -- `cli/src/status.rs`

- `ScrubReport::{Finished,Aborted,Interrupted}`: change `started_at: String` ->
  `started_at: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]`;
  change `started_at_human: String` and `journal_since: String` ->
  `Option<String>` (both keep `#[serde(skip)]`).
- `get_scrub_report`: each terminal arm now binds `started_at:
  Option<ScrubTimestamp>`. Factor a small helper that maps
  `Option<&ScrubTimestamp>` -> `(Option<String>, Option<String>, Option<String>)`
  using the existing `format_scrub_timestamp_iso`, `format_scrub_timestamp`, and
  `format_scrub_timestamp_for_journalctl`, so the three arms stay DRY. `None` ->
  `(None, None, None)`.
- `format_status_human`: render the missing start time as the ASCII placeholder
  `"unknown start"` (chosen over silently omitting so the terminal state and
  `error_count` stay visible, and chosen over bare `"unknown"` to disambiguate
  from the `ScrubReport::Unknown` line which already renders `"unknown"`). E.g.
  `let start = started_at_human.as_deref().unwrap_or("unknown start");` then
  `format!("{start} cancelled (will resume)")` etc.
- `scrub_hint` (the `scrub error details:` journalctl line, shown only when
  `error_count > 0`): build it only when `journal_since` is present --
  `journal_since.as_deref().map(format_scrub_journal_command)`. With no start
  time there is no `--since` anchor, so the hint is omitted (the `(N errors)`
  count is still shown).

### 4. TUI -- `cli/src/tui/view/mod.rs` and `cli/src/tui/demo.rs`

The TUI consumes `ScrubState` directly (not `ScrubReport`), so it must handle the
new `Option`:

- `scrub_terminal_rows`: change the `started_at: &ScrubTimestamp` parameter to
  `Option<&ScrubTimestamp>`; render the "Last run" row value as `"unknown"` when
  `None`. **Always emit the "Last run" row** so the row count is unchanged and the
  height-calc arm (which uses `..` and is otherwise untouched) stays correct.
- `scrub_table`: the three terminal arms bind `started_at: &Option<ScrubTimestamp>`;
  pass `started_at.as_ref()` into `scrub_terminal_rows`.
- `scrub_hint_command`: binds `started_at: &Option<ScrubTimestamp>`; build the
  hint only when `Some` -- `started_at.as_ref().map(|ts|
  scrub_journal_command(&format_scrub_journal_since(ts)))` under the existing
  `error_count > 0` guard.
- `tui/demo.rs`: wrap the constructed `started_at: ScrubTimestamp(...)` as
  `Some(...)`.

### 5. Downstream confirmations (no code change)

- `scrub_needs_resume.rs#cmd_scrub_needs_resume` matches `Aborted { .. } |
  Interrupted { .. }` and `Finished { .. }` with `{ .. }` -- unaffected by the
  field becoming `Option`. After the fix, a terminal-but-no-timestamp scrub
  reaches these arms instead of `Unknown`, so it correctly returns `Yes`/`No`
  instead of `Err(StatusUnknown)`.
- `idle.rs#cmd_idle` matches the terminal variants with `{ .. }` -> `Idle`, and
  keeps `ScrubState::Unknown -> Busy`. Unaffected structurally; the behavior for
  a terminal-no-timestamp scrub correctly shifts from `Busy` to `Idle`.

## Tests to add

All are Rust `#[test]` units (no VM harness; this is a pure parsing/state unit).
Each opens with the `//` Intent / Why it exists / Scenario preamble. Assertions
are behavioral (assert the resulting state / observable JSON / observable text),
not structure-coupled.

**Parser -- `cli/src/parse/btrfs_scrub_status.rs`:**

1. `scrub_aborted_without_started_is_aborted` -- aborted block with **no**
   `Scrub started:` line asserts `ScrubState::Aborted` with `started_at: None`.
   - *Intent:* an aborted scrub missing its start line still classifies as Aborted.
   - *Why it exists:* terminal state must come from the `Status:` word; a missing
     start line must not downgrade a resumable state to `Unknown` (which would
     hard-fail the resume trigger).
   - *Scenario:* btrfs emits a sparse terminal block after a cancelled scrub.
2. `scrub_interrupted_with_unparseable_started_is_interrupted` -- interrupted
   block whose `Scrub started:` value is non-ctime garbage asserts
   `ScrubState::Interrupted`, `started_at: None`.
   - *Intent:* an interrupted scrub whose start timestamp fails to parse still
     classifies as Interrupted.
   - *Why it exists:* `parse_ctime` is format/locale-fragile; future drift must
     not flip a resumable state into `Unknown`.
   - *Scenario:* btrfs prints the start line in an unexpected format.
3. `scrub_finished_without_started_is_finished` -- finished block with no start
   line asserts `ScrubState::Finished`, `started_at: None`.
   - *Intent:* a finished scrub with no parseable start time still classifies as
     Finished, not Unknown.
   - *Why it exists:* completion is authoritative from the `Status:` word; the
     start timestamp is decoration.
   - *Scenario:* sparse terminal block on a completed scrub.

Keep the existing negatives green (no change to their assertions):
`scrub_status_unknown_status_word_is_unknown` (`weird` -> `Unknown`),
`scrub_unknown_on_empty_output` (empty -> `Unknown`),
`scrub_parses_nixos_26_05_never` (no stats -> `Never`). Existing terminal tests
that read `started_at.0` (e.g. `scrub_status_aborted_inline`,
`scrub_status_interrupted_inline`, `scrub_finished_with_errors_inline`,
`scrub_resumed_parses_timestamp`, `scrub_parses_nixos_26_05_completed`) need a
mechanical update to unwrap the now-`Option` field (their fixtures carry valid
timestamps, so the value is `Some`).

**Resume -- `cli/src/scrub_needs_resume.rs`:**

4. `aborted_without_started_at_still_needs_resume` -- wire a `MockRunner` whose
   `BtrfsScrubStatus` output is an aborted block **without** a parseable `Scrub
   started:` line (inline `RawCommandOutput`, mirroring the existing
   `status_command_failure_propagates` test's inline style); assert
   `cmd_scrub_needs_resume` returns `Ok(ScrubNeedsResumeResult::Yes)`, **not**
   `Err(StatusUnknown)`.
   - *Intent:* an aborted scrub with no parseable start time still reports
     needs-resume.
   - *Why it exists:* `braid-scrub-resume-trigger.service` must not hard-fail and
     strand resumable progress over a missing display timestamp.
   - *Scenario:* pool-online trigger probes a scrub that `braid lock` aborted,
     whose status block lacks a parseable start line.
   (Interrupted shares the same match arm; an optional symmetric
   `interrupted_without_started_at_still_needs_resume` may be added.)

**Status mapping + rendering -- `cli/src/status.rs` (covers `get_scrub_report`,
the serde shape, and the renderer end-to-end):**

`get_scrub_report` has its *own* `ScrubState -> ScrubReport` mapping that
destructures `started_at` itself, so the missing-start case must be exercised
*through* `get_scrub_report` from raw output. A directly-constructed `ScrubReport`
would leave that mapping untested -- the parser, resume, serde, and render tests
could all pass while `status` still collapses a terminal/no-timestamp scrub to
`Unknown` or a placeholder, breaking `braid status --json`. Add a raw fixture
`status_btrfs_scrub_aborted_no_start()` to `cli/src/test_fixtures/status.rs` (a
copy of `status_btrfs_scrub_aborted()` with the `Scrub started:` line removed).
This is a Rust `#[cfg(test)]` helper string, **not** a file under
`tests/fixtures/`, so the no-fixture-refresh constraint holds.

5. `status_scrub_aborted_no_start` -- feed `status_btrfs_scrub_aborted_no_start()`
   through a `MockRunner` into `get_scrub_report` (mirroring the existing
   `status_scrub_aborted` test); assert the result is `ScrubReport::Aborted {
   started_at: None, started_at_human: None, journal_since: None, error_count: 0 }`.
   Then `serde_json` the **resulting** report and assert `"state": "aborted"`,
   `error_count` present, and the `started_at` key **absent**
   (`skip_serializing_if`).
   - *Intent:* `get_scrub_report` maps a terminal scrub with no parseable start to
     the terminal `ScrubReport` with `started_at: None`, and `--json` omits the key.
   - *Why it exists:* the parser fix is wasted if `status` still collapses the case
     to `Unknown`/a placeholder; this pins the end-to-end mapping and the JSON
     contract that `started_at` is optional for terminal states.
   - *Scenario:* `braid status --json` runs against a scrub that `braid lock`
     aborted, whose status block lacks a parseable start line.
6. `human_scrub_aborted_unknown_start_with_errors` -- render `format_status_human`
   for a no-start aborted report with **`error_count: 2`** and `journal_since:
   None` (construct `ScrubReport::Aborted { started_at: None, started_at_human:
   None, error_count: 2, journal_since: None }` directly -- test 5 already pins the
   `get_scrub_report` mapping, and the shared no-start fixture carries 0 errors).
   Assert the line contains `unknown start (2 errors) cancelled (will resume)` and
   that **no** `scrub error details:` journalctl line is emitted (ASCII-only
   output).
   - *Intent:* with a missing start time, the human renderer shows the `unknown
     start` placeholder plus the error count, and suppresses the journalctl hint.
   - *Why it exists:* the hint guard fires only on `error_count > 0`; with
     `error_count: 2` and no `journal_since`, `format_status_human` must not emit a
     broken `--since ''` command. A zero-error report (test 5's) never enters the
     guard, so the `journal_since: None` branch needs an errors>0 case to exercise it.
   - *Scenario:* operator runs `braid status` against an aborted scrub that
     recorded errors but whose status block lacks a parseable start line.

Mechanically update existing `status.rs` terminal tests (destructuring/JSON
assertions around the `ScrubReport` fields) to the `Option` shape -- e.g.
`assert_eq!(started_at.as_deref(), Some("..."))`; JSON assertions for the
valid-timestamp case are unchanged (`Some` serializes identically). The
`scrub_report_json_skips_renderer_only_fields` test's deserialize assertions for
`started_at_human` / `journal_since` change from `== ""` to `== None`.

**Optional symmetry:** `idle.rs` test that a terminal-no-timestamp scrub yields
`Idle` (same root cause as the resume test; lower priority).

## Doc updates

- **`docs/commands/status.md` (required):** the `last_scrub` JSON section states
  `started_at` is present for `finished`/`aborted`/`interrupted`. Add that
  `started_at` is **omitted** when btrfs reported no parseable start time (the
  terminal state is still classified from the `Status:` word), and qualify the
  existing "a `--json` consumer derives its own `--since` value from
  `started_at`" with "when present." The healthy-pool example (with `started_at`)
  stays valid -- `Some` serializes unchanged.
- **`docs/design/decisions/016-auto-suspend.md` (no change -- note in commit):**
  its fail-closed clause already defines `ScrubState::Unknown` as "empty stdout or
  an unrecognized `Status:` word," which matches the post-fix Unknown set. The fix
  aligns the code to the ADR; cite this as supporting evidence rather than editing
  the ADR.
- **`docs/design/decisions/018-systemd-lifecycle.md` (optional one-liner):** the
  resume-trigger bullet says the scrub starts "only when saved progress is
  resumable" and makes no claim the fix contradicts. Optionally add that
  resumability is keyed on the terminal `Status:` word, independent of
  start-timestamp parsing. Not strictly required; the invariant is better encoded
  in the parser and `cmd_scrub_needs_resume` `///` doc comments (step 2 / step 5).
  Touching the ADR's `## See` section is unnecessary (no new cross-reference).

## Risks

- **Low blast radius, fully enumerated.** A repo-wide grep for the three terminal
  variants confirms every binder of `started_at`: `status.rs#get_scrub_report`,
  `tui/view/mod.rs` (`scrub_hint_command`, `scrub_table`/`scrub_terminal_rows`),
  and the `tui/demo.rs` construct site. All other matches use `{ .. }` and are
  unaffected (`scrub_needs_resume.rs`, `idle.rs`, the TUI height-calc arm,
  `tui/view/mod.rs:1980`). `btrfs_scrub_status_per_device.rs` is a separate enum.
- **No fixture refresh.** This changes robustness, not which btrfs-progs output is
  parsed; `tests/fixtures/` is untouched.
- **Snapshot tests stay green.** TUI snapshots render valid-timestamp fixtures
  (`Some`) and the Some-case rendering is unchanged; only a new `None` branch is
  added. No existing snapshot exercises `None`.
- **Intentional behavior changes (both improvements, currently latent under
  `LC_ALL=C`):** a terminal scrub with an unparseable/missing start time now (a)
  resumes via `scrub-needs-resume` instead of erroring, and (b) is `Idle` for
  auto-suspend instead of `Busy`. Both are more correct; the new status tests and
  the `status.md` note document the observable surface.

## Verification

- `just test-rust` (or `cargo test -p <cli crate>`) -- the new parser,
  resume, and status tests pass; existing terminal/JSON/human tests pass after the
  mechanical `Option` updates; `scrub_status_unknown_status_word_is_unknown` and
  `scrub_unknown_on_empty_output` still pass.
- `cargo build` / `cargo clippy` -- confirm all `ScrubState` match sites compile
  (the TUI and demo updates).
- `just docs-build` -- `mdbook-linkcheck2` passes; the `status.md` edit renders.
- `scripts/docs/check-output-ascii.py` -- the new `"unknown start"` placeholder
  and any echo lines are ASCII-only.
- The status mapping test (test 5) drives `get_scrub_report` end-to-end from raw
  aborted-no-start output, so the `ScrubState -> ScrubReport` mapping and the
  `--json` omission are exercised without a btrfs host; test 6 covers the human
  render shape.
