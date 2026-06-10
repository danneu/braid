# Plan: centralize the command-failure "empty-stderr tail" rule

## Context

`braid` renders command failures as `... (exit N): <stderr>`. When a tool exits
non-zero but writes nothing to stderr, the trailing `: <stderr>` clause collapses
to a dangling, contentless `: ` (or, in `doctor`, a doubled-space `):  --`).

A review finding flagged one instance of this at the `upsc` query boundary
(`cli/src/ups.rs`). Investigation showed it is really **one of four divergent
renderings of the same `UpsQueryError::QueryFailed { exit_code, stderr }` data**,
and that the codebase already solves this exact problem **four more times by hand**
(`cmd.rs` signal-kill, `wol.rs`, `doctor.rs` ethtool, `ack.rs`) -- each with its own
inline `if stderr.is_empty()` branch, none sharing a helper.

For `upsc` the empty-stderr branch is currently **unreachable** (NUT's `upsc.c`
always writes `Error: ...` to stderr before any `EXIT_FAILURE`), so this is a
**robustness + consistency + dedup** refactor, not a live bug fix. The intended
outcome: the "drop the contentless `: x` tail" rule lives in exactly one tested
place, every UPS render site uses it, and the eight hand-rolled copies collapse to
one -- with **zero change to any reachable, non-empty output**.

## Approach

Introduce one tiny shared helper and route every site through it. Keep each call
site's own prose prefix, so non-empty output stays byte-identical and every frozen
contract (docs table, insta snapshot, substring tests) holds. Done as two commits so
the behavior fix and the pure dedup stay separately revertable.

### The helper (reuse target for all eight sites)

Add to `cli/src/util.rs`, next to `format_duration_secs` (same `pub`/doc/test style):

```rust
/// Centralizes the "drop the trailing `: <detail>` clause when detail is
/// blank" rule so command-failure messages never trail a contentless colon
/// at a tool boundary. Hand-rolled at several sites before this existed.
/// Callers pass already-trimmed text (capture sites trim -- see
/// `query_ups` and `output_to_raw`); the helper keys off `is_empty()` only.
pub(crate) fn detail_suffix(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}
```

### Commit 1 -- UPS empty-stderr fix (the finding's root cause)

Keep `UpsQueryError::QueryFailed { exit_code, stderr }` **structured** (do not
pre-render into a `detail: String`): the variant's doc-comment says callers need
`exit_code`/`stderr` to tell "daemon vs name" problems apart, and pre-rendering would
force a single rendering that breaks `doctor`'s parenthesized form. Compose the helper
at each of the four UPS render sites:

- `cli/src/ups.rs` (Display, ~L50): `#[error("upsc query failed (exit {exit_code}){}", detail_suffix(.stderr))]` -- the **dot-field** `.stderr` is required: `thiserror` trailing format args reference variant fields as `.field`, not bare identifiers (a bare `stderr` would be an undefined local and fail to compile). Precedent: `format_uuid_list(.members)` in `cli/src/membership.rs` (`MembershipError::DuplicateDevid`). The inline `{exit_code}` shorthand needs no dot. Makes the (currently dead, never-printed) Display honest.
- `cli/src/ups.rs` (`cmd_ups_status`, ~L151): `emit_query_failed(json, format!("exit {exit_code}{}", detail_suffix(&stderr)))` -- preserves the documented `exit N: stderr` detail; empty -> `exit N`.
- `cli/src/doctor.rs` (UPS check, ~L1392): keep the parens, append the helper: `"upsc {} failed (exit {exit_code}){} -- check 'systemctl status upsd.service' or verify the UPS name"` with `detail_suffix(&stderr)`. Parens preserved -> the `failed (exit 1)` substring test stays green; empty -> `(exit 1) --` (no doubled space).
- `cli/src/preflight.rs` (refuse arm, ~L626): keep `{ stderr, .. }` (do **not** capture `exit_code` -- that would be an undocumented prose change). Drop the hard-coded colon from the prefix: `refuse(&format!("upsc query failed{}", detail_suffix(&stderr)))`. Non-empty -> `upsc query failed: {stderr}` (byte-identical); empty -> `upsc query failed`.

Imports: add `use crate::util::detail_suffix;` in `ups.rs` and `preflight.rs`; in
`doctor.rs` use fully-qualified `crate::util::detail_suffix(&stderr)` to match that
file's `crate::ups::...` style.

**Docs** (behavior change must update them -- AGENTS.md): the empty-stderr rendering
is newly defined. Append a one-line note to the `query_failed` row in both
`docs/commands/ups-status.md` (~L109) and `docs/guides/ups.md` (~L94): when stderr is
blank, `detail` is just `exit <code>` (the `: <stderr>` clause is omitted). Plain
prose, no new link (keeps `mdbook-linkcheck2` happy). Grep `README.md` for
`query_failed`; update only if it mirrors the table (it likely does not).

Also reword the mutation-preflight bullet in `docs/guides/ups.md` (the
`## Mutation refusal when utility power is not verified` section, ~L142): it currently
reads "the message includes `upsc`'s stderr when it exits non-zero", which overclaims
once the blank-stderr clause can be dropped. Change it to say the stderr is included
when present and omitted when that stderr is blank -- consistent with the
`query_failed` table note above and with the preflight refuse-arm edit.

**Tests** (behavioral, structure-insensitive -- 5 tests + 1 fixture). Commit 1 changes
**four independently-revertable render paths** (`ups.rs` `UpsQueryError` Display ~L50,
`cmd_ups_status` ~L151, `doctor.rs` UPS check ~L1392, `preflight.rs` refuse arm ~L626),
so each empty-stderr edit gets its own pin -- a revert of any one must turn a test red.
Every new test carries the `//` Intent/Why/Scenario preamble (AGENTS.md).

Fixture: `ups_query_empty_stderr_exit_1()` in `cli/src/test_fixtures/ups.rs` (mirror
`ups_query_connection_refused_no_newline` but `stderr: String::new()`,
`exit_status: 1`); add a `///`; re-export it in the `ups::{...}` list in
`cli/src/test_fixtures.rs` (~L235). Consumed by test 2; the doctor/preflight tests
build their mock inline to match their sibling tests.

1. `detail_suffix` unit test in `util.rs` (Intent/Why/Scenario block) -- the
   source-of-truth pin for the rule itself: `""` -> `""`, `"x"` -> `": x"`, and a
   whitespace case (`"  "` -> `":   "`) pinning the caller-trims contract. Add
   `detail_suffix` to the test module's `use super::{...}`.
2. `cmd_ups_status` empty-stderr test (`ups.rs`): drive
   `cmd_ups_status(&runner, &cfg, false)` with the new fixture, match
   `Err(UpsError::QueryFailed { detail })`, assert `detail == "exit 1"` (the
   JSON-facing string -- no tail) and `err.to_string() == "upsc query failed: exit 1"`
   (the human wrap -- no dangling colon). Pins the ~L151 site + the **outer**
   `UpsError` Display. Extend the `use crate::test_fixtures::{...}` import at the top
   of the test module.
3. `UpsQueryError::QueryFailed` Display unit test (`ups.rs`) -- pins the ~L50 edit,
   which test 2 does **not** cover: test 2 exercises the outer `UpsError`, but L50 is
   the **inner** `UpsQueryError`, a distinct enum whose Display is otherwise unpinned
   (a revert of L50 alone leaves test 2 green). No harness -- construct the variant
   directly and lock both shapes in one test:
   `UpsQueryError::QueryFailed { exit_code: 1, stderr: String::new() }.to_string()`
   `== "upsc query failed (exit 1)"`, and
   `{ exit_code: 1, stderr: "boom".into() }.to_string() == "upsc query failed (exit 1): boom"`.
4. doctor empty-stderr warn test (`doctor.rs`) -- pins the ~L1392 edit. Clone
   `ups_daemon_check_warns_when_upsc_query_fails` (it already inlines its `MockRunner`)
   with `stderr: String::new()`; assert `r.status == CheckStatus::Warn`,
   `r.message.contains("failed (exit 1) -- check")` (single space, no colon, no doubled
   space -- the buggy form is `(exit 1):  --`), and
   `r.message.contains("verify the UPS name")` (the remediation text survives).
5. preflight empty-stderr refuse test (`preflight.rs`) -- pins the ~L626 edit. The
   shared `upsc_mock` helper hard-codes a **non-empty** stderr, so build the mock
   inline: `MockRunner::default().with_output(CmdRequest::UpscQuery { name: "ups".into() }, RawCommandOutput { cmd: "upsc ups".into(), stdout: String::new(), stderr: String::new(), exit_status: 1 })`.
   Call `check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err()`; assert it
   contains `utility power` and `(upsc query failed)` (no dangling colon inside the
   parens -- the buggy form is `(upsc query failed: )`).

### Commit 2 -- converge the four hand-rolled copies (pure dedup, zero behavior change)

Replace each inline `if .is_empty()` branch with the helper. All produce byte-identical
non-empty **and** empty output (each already uses a `: ` separator), so existing tests
-- including `cmd.rs`'s `output_to_raw_signal_killed_empty_stderr_reports_signal` --
stay green with no edits:

- `cli/src/cmd.rs` (`output_to_raw`, ~L1367, signal-kill): `stderr` here is `&str` (post-`.trim()` at L1366), so pass it directly: `format!("{cmd_str}: killed by signal {sig} ({name}){}", detail_suffix(stderr))`.
- `cli/src/wol.rs` (`wol_not_ready_reason`, ~L118): the `QueryFailed { exit, detail }` arm binds `detail` as an owned `String` (the fn takes `readiness` by value), so **borrow it**: `format!("ethtool {interface} failed with exit {exit}{} -- cannot verify Wake-on-LAN", detail_suffix(&detail))`.
- `cli/src/doctor.rs` (`summarize_wol`, ~L1306, ethtool): `detail` is likewise an owned `String` here; drop the local `suffix` var and inline `crate::util::detail_suffix(&detail)` (fully-qualified `&detail`, matching this file's commit-1 style).
- `cli/src/ack.rs` (`format_systemctl_stop_failure`, ~L246): `stderr` is `&str` (post-`.trim()` at L245), so pass it directly: `format!("warning: systemctl stop braid-alert.service: {}{}", output.status, detail_suffix(stderr))`.

Imports (commit 2): add `use crate::util::detail_suffix;` to `cmd.rs`, `wol.rs`, and
`ack.rs` -- none currently import from `crate::util`, and each already uses
`use crate::...` imports, so the line fits the existing style. `doctor.rs` uses the
fully-qualified `crate::util::detail_suffix(...)` form at both its sites (commit 1 UPS
check + commit 2 `summarize_wol`), so it needs no new import.

**Explicitly out of scope:** the ~30 `bail!`/`#[error]` sites in `pool.rs`, `luks.rs`,
`mount.rs`, `recover.rs`, `online_state.rs`, `remove.rs` that inline
`result.stderr.trim()`. Those always have stderr (empty case unreachable) and do not
hand-roll the empty/non-empty branch, so there is no rule to dedup -- forcing the
helper there would *add* a guard that changes the (unreachable) empty output. Leave
them.

## Critical files

- `cli/src/util.rs` -- new `detail_suffix` helper + unit test (the single source of truth).
- `cli/src/ups.rs` -- Display + `cmd_ups_status` edits; new empty-stderr test.
- `cli/src/doctor.rs` -- UPS check edit + new empty-stderr warn test (commit 1); ethtool dedup (commit 2).
- `cli/src/preflight.rs` -- refuse-arm edit + new empty-stderr refuse test (commit 1).
- `cli/src/test_fixtures/ups.rs` + `cli/src/test_fixtures.rs` -- new fixture + re-export.
- `cli/src/cmd.rs`, `cli/src/wol.rs`, `cli/src/ack.rs` -- commit 2 dedup.
- `docs/commands/ups-status.md`, `docs/guides/ups.md` -- empty-stderr note.

## Verification

```sh
cargo fmt --manifest-path cli/Cargo.toml --check   # house line-wrapping
just clippy                                         # cargo clippy --tests
just test-rust                                      # lib + bin + golden/tty/confirm tests
just check-output-ascii                             # selftest + cli/modules echo-line scan
just docs-build                                     # mdbook + mdbook-linkcheck2
```

Expected: all green, **no pending insta snapshot** (the `snapshot_json_query_failed`
snapshot feeds a hard-coded literal and never touches the production path -- if it goes
pending, a composition leaked somewhere it should not have), and **no ASCII findings**
(every string uses only `:`, `()`, space, and the approved `--`; test/fixture code is
exempt from the scanner). The five `detail_suffix`/empty-stderr tests (util rule,
`cmd_ups_status`, `UpsQueryError` Display, doctor warn, preflight refuse) prove the
tail/colon/doubled-space is dropped at every render path the plan touches; the
unchanged non-empty tests (`ups_daemon_check_warns_when_upsc_query_fails`,
`ups_query_failed_refuses`, `output_to_raw_signal_killed_empty_stderr_reports_signal`)
prove the reachable non-empty paths stay byte-identical.
