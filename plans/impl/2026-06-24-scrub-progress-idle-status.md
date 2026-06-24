# Plan: unify scrub-progress percentage across `idle` and `status`

## Context

A Low/Simplicity finding asked to weaken idle's `busy_when_scrub_running`
assertion from `pct: Some(45)` to `matches!(.., Some(_))`, arguing the concrete
`45` re-tests percentage arithmetic that `pct_from_bytes` already owns.

Verification found the finding's *fix* wrong but its *instinct* right:

- **The weakening, as proposed, loses coverage.** The fixture counters
  (`13683916800 / 30408704000`) are an exact `0.45`, so `Some(45)` is the only
  end-to-end pin that `cmd_idle` passes the right counters in the right order to
  `pct_from_bytes`; an argument swap yields `Some(100)`, which `Some(_)` would
  silently accept. Because `45.00%` is exact, no rounding-policy change to
  `pct_from_bytes` could ever move this assertion off `45` -- the "misattributed
  failure" the finding cites cannot occur for this fixture.
- **But the wiring layer genuinely should not own the math.** The reason
  `Some(45)` is load-bearing is that the mapping "running-scrub pct =
  `scrubbed / total`, `None` if either counter is absent" is **duplicated
  inline** -- byte-for-byte identical -- in the two `u8` commands, owned by
  neither:
  - `cli/src/idle.rs#cmd_idle` (the `ScrubState::Running` arm)
  - `cli/src/status.rs#get_scrub_report` (the `ScrubState::Running` arm)

  A third site, `cli/src/tui/view/mod.rs#scrub_table`, selects the same two
  counters and divides them again, but renders a deliberately higher-precision
  `f64`; it is left as-is and the unification is scoped to the two `u8` surfaces
  (see the Scope note below).
- **The status copy has no end-to-end test at all.** Every `status_scrub_*`
  test drives only terminal-state fixtures (never/finished/aborted/interrupted,
  no byte counters); the sole running test, `scrub_report_json_running_with_pct`,
  hand-builds `ScrubReport::Running { pct: Some(42) }`. So a counter swap in
  `get_scrub_report` ships a wrong percentage to `braid status` JSON/human
  output with every test still green.

Both commands report scrub progress to the user. If the two copies drifted,
`braid idle` and `braid status` would show different percentages for the same
scrub. That is exactly the "two paths that must agree" contract braid shares
rather than duplicates (e.g. `BusyReason::Exclop` sharing exclop identity with
preflight; `status.rs#format_scrub_report_timestamps` extracted "so every
representation shares the same Some/None fate"). AGENTS.md mandates the most
correct solution regardless of refactor cost.

**Intended outcome:** one tested owner of the scrub-progress `u8` mapping, used
by `idle` and `status` so the two cannot drift; the status gap closed; and the
finding's instinct honored -- with the math owned in one place, the call-site
tests legitimately shrink to wiring assertions. The TUI's higher-precision view
is a separate, intentional surface (see Scope) and stays as-is.

## Scope: a third counter-selection site (the TUI), left as-is

`cli/src/tui/view/mod.rs#scrub_table` selects the same two counters from
`ScrubState::Running` and divides them a third time:

```rust
(Some(scrubbed), Some(total)) if total > 0 =>
    format!("running ({:.2}%)", scrubbed as f64 / total as f64 * 100.0),
_ => "running".to_owned(),
```

It is deliberately *not* folded into `scrub_running_pct`, so the "cannot drift"
claim is scoped to the two `u8` surfaces (`idle`, `status`), not codebase-wide:

- **Different, intentional precision.** The TUI renders a two-decimal `f64`
  (`running (14.78%)`); `status` renders the `u8` contract `pct: Option<u8>`
  (`running (14%)`, `status.rs#format_status_human`) that JSON consumers depend
  on. A `u8` owner cannot carry the TUI's precision, and collapsing the TUI to
  `u8` would regress its live view. The displayed strings differ by design.
- **Agrees on the absent-counter case.** Its `total > 0` guard and
  `_ => "running"` arm match `pct_from_bytes`'s `total == 0 -> None` /
  both-absent semantics, so the divergence is presentational. (Precision is not
  the *only* difference: `scrub_table` divides unclamped, where `pct_from_bytes`
  caps at 100 (`pct_from_bytes_clamps_above_100`), so a `scrubbed > total` input
  would render `222.00%` rather than `100`. That input is an unreachable btrfs
  state, so it never surfaces -- but the honest claim is "presentational," not
  "precision only.")
- **Its swap is already guarded.** The pct renders into
  `snapshot_scrub_tab_running.snap`, so a transpose there changes the snapshot
  and fails CI -- unlike the (until now) untested `status` path. This is why the
  plan's `rg 'pct_from_bytes'` check is not sufficient on its own: the TUI does
  not call `pct_from_bytes`. The verification step below therefore also greps for
  the raw `f64` division, so an implementer sees this third site and confirms it
  is intentionally left.

Future option (only if cross-surface consistency becomes an explicit goal): make
the single owner return the *validated counters or ratio*
(`scrub_running_progress(&ScrubState) -> Option<(u64, u64)>`, or `-> Option<f64>`),
then let each surface format to its own precision -- `pct_from_bytes` for the
`u8` commands, `{:.2}` for the TUI (which already has `&ScrubState` in hand).
That extends the unexpressible-swap property to all three sites. Out of scope
here: it touches a third subsystem and re-baselines the TUI snapshot for a
presentational value that is already correct and snapshot-guarded.

## Approach

### 1. Extract the shared mapping into `progress.rs`

The helper takes the parsed `&ScrubState`, **not** two positional `Option<u64>`
counters. `ScrubState::Running` (`cli/src/parse/types.rs#ScrubState`) declares
`total_bytes` *before* `bytes_scrubbed`, so two same-typed positional args are an
easy transpose -- and a reversed call would ship `100%` instead of `45%` with
every call-site test still green (the swap-gap the review caught). Passing the
whole parsed state moves field selection *into* the single owner: there is no
positional argument at the call sites to reverse, so the bug is unexpressible at
the call site as refactored. (Not globally impossible -- a future edit could
re-inline a swapped division at a call site; that residual is named as an
accepted trade in section 4.) This keeps the dependency direction clean
(`progress.rs` already imports parse types such as `BalanceState`).

Add next to `pct_from_bytes` (`cli/src/progress.rs#pct_from_bytes`):

```rust
/// Single owner of the running-scrub progress mapping: `bytes_scrubbed` over
/// `total_bytes`, `None` when either counter is absent or the scrub is not
/// running, so `braid idle` and `braid status` cannot report different
/// percentages for the same scrub. Takes the parsed `ScrubState` by reference
/// -- not two `Option<u64>` counters -- so field selection lives here and call
/// sites cannot transpose the two same-typed counters (the parser declares
/// `total_bytes` before `bytes_scrubbed`). Do not "simplify" this back to
/// positional counter args: the `&ScrubState` boundary is what keeps the swap
/// unexpressible at the call sites. Truncation/clamp arithmetic is owned by
/// `pct_from_bytes`.
pub(crate) fn scrub_running_pct(state: &ScrubState) -> Option<u8> {
    let ScrubState::Running { bytes_scrubbed, total_bytes, .. } = state else {
        return None;
    };
    match (bytes_scrubbed, total_bytes) {
        (Some(scrubbed), Some(total)) => pct_from_bytes(*scrubbed, *total),
        _ => None,
    }
}
```

`pub(crate)` (both call sites are in-crate; this is not external API); add
`use crate::parse::ScrubState;` to `progress.rs`.

### 2. Replace both inline blocks with the call

Both call sites already `match` the parsed state to make a per-command decision
(idle: Busy-vs-Idle; status: which `ScrubReport` variant), and use the byte
counters *only* to compute `pct`. So hoist the helper call above the existing
by-value `match` and leave every other arm byte-identical:

- `cli/src/idle.rs#cmd_idle`: before `match scrub.state`, add
  `let running_pct = scrub_running_pct(&scrub.state);` (a shared borrow that ends
  before the by-value match); the `ScrubState::Running { .. }` arm becomes
  `IdleResult::Busy(BusyReason::ScrubRunning { pct: running_pct })`. Swap the
  `use crate::progress::pct_from_bytes` import for `scrub_running_pct`.
- `cli/src/status.rs#get_scrub_report`: before `match out.state`, add
  `let running_pct = scrub_running_pct(&out.state);`; the `ScrubState::Running { .. }`
  arm becomes `ScrubReport::Running { pct: running_pct }`. Adjust the
  `use crate::progress::...` import. The Finished/Aborted/Interrupted arms are
  untouched (they still move `started_at` / copy `error_count` by value).

Pure behavior-preserving refactor. There is no positional counter argument at
either call site, so a transposition is not expressible there -- field selection
is owned and tested once in `scrub_running_pct` (per section 3). (A future edit
could still re-inline a swapped division at a call site; section 4 names that
residual as an accepted trade.)

### 3. Test the helper (sole owner of the exact value + field selection)

This is where the **exact** `Some(45)` lives -- the helper test owns counter
selection, argument order, *and* the composed arithmetic, so no call-site test
has to. Construct each input with **named fields** -- `ScrubState::Running {
bytes_scrubbed: ..., total_bytes: ..., <six other fields None/0> }` -- so the
mapping is explicit at the assertion site, and the real-ratio value also proves
the order (the reverse clamps to 100). Do **not** route through a positional
`running(a, b)` constructor: two same-typed `Option<u64>` args reintroduce, at
the test site, the very transpose the `&ScrubState` boundary exists to
eliminate. Naming the constructor's params `bytes_scrubbed` / `total_bytes` does
not rescue this -- the call site is still `running(Some(a), Some(b))`, so the
caller can transpose the two arguments regardless of what the params are named.
There is no non-transposable two-`Option<u64>`-arg constructor, so do not offer
one: mandate the named-field struct literal per case and accept the six-field
boilerplate as the price of keeping the transpose unexpressible at the test site
too. (Rust functional-update `..base` does not apply to enum variants, so there
is no one-line spread to trim that boilerplate -- spell the `Running` variant out
each time.)

- both present -> `Running { bytes_scrubbed: Some(13683916800), total_bytes: Some(30408704000), <six other fields None/0> }` -> `Some(45)`.
- scrubbed absent -> `Running { bytes_scrubbed: None, total_bytes: Some(30408704000), <six other fields None/0> }` -> `None`.
- total absent -> `Running { bytes_scrubbed: Some(13683916800), total_bytes: None, <six other fields None/0> }` -> `None`.
- not running -> `ScrubState::Never` (fieldless; or `ScrubState::Finished { <five fields> }`) -> `None`,
  pinning the non-Running fallback.

The truncation/clamp arithmetic edges remain owned by the existing
`pct_from_bytes_*` tests. Each new test gets the `Intent / Why it exists /
Scenario` preamble (see `docs/dev/testing.md`); the "Why" names this as the
single owner of the bytes->pct mapping that `idle` and `status` both defer to.

### 4. Reduce idle's call-site tests to wiring assertions

These can assert at the wiring level because the swap is unexpressible at the
call site as refactored (section 1), not as a tolerated weakening. The call-site
test's only remaining job is "running scrub -> `Busy(ScrubRunning)` with the
helper's pct surfaced." In `cli/src/idle.rs`:

- `busy_when_scrub_running`: change the assertion to
  `assert!(matches!(result, IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(_) })))`.
  `Some(_)` still catches a dropped/`None` pct against the bytes-bearing fixture;
  the exact `45` is owned by the section-3 helper test. Update the preamble's
  "Why it exists" to state it pins the running-scrub -> `Busy(ScrubRunning)`
  wiring with a percentage *present*, and that the bytes->pct mapping/arithmetic
  is owned by `progress.rs#scrub_running_pct` / `pct_from_bytes` (sibling-
  ownership phrasing, matching `busy_when_scrub_running_no_bytes`'s existing
  reference back to this test).
- `busy_when_scrub_running_no_bytes`: keep (its `pct: None` assertion still pins
  idle's Busy-vs-Idle decision for the no-counters case, which lives outside the
  mapping); add a one-line note that the mapping itself is owned by
  `scrub_running_pct`.

Not adopting the round-1 alternative of keeping exact `Some(45)` at both call
sites. The justification is **single-owner/DRY**: the exact value has one home
(the section-3 helper test); duplicating it into two wiring tests re-asserts a
value those tests do not own. (We do *not* lean on the "a rounding change would
break them" argument -- the Context shows the exact-`0.45` fixture is immune to
rounding-policy changes. A change to the pct's *type/scale*, e.g. `u8` to
tenths, would still break an exact assertion, but single-ownership already
covers that: update one helper test, not two.)

**Consciously accepted residual** (the over-claim the review flagged): because
the call sites assert only `Some(_)`, a future edit that re-inlines a swapped
`pct_from_bytes(total, scrubbed)` at a call site -- bypassing the helper -- is
caught by neither the call-site tests nor the helper test. The swap is
unexpressible *as refactored*, not globally impossible. The trade buys
single-ownership; the deterrent is the shared helper sitting right there
(hand-rolling the division when the owner exists is a reviewable smell), not a
test. Keeping exact `Some(45)` at idle would close this residual at the cost of
re-duplicating the value -- a defensible choice, but the plan takes the
single-owner side deliberately.

### 5. Close the status gap: new fixture + end-to-end test

- `cli/src/test_fixtures/status.rs`: add `status_btrfs_scrub_running()` mirroring
  the sibling fixtures' style (UUID `aaaaaaaa-...`, `mock_ok("btrfs scrub status --raw", ...)`)
  with `Status: running` and the same 45% counters
  (`Total to scrub: 30408704000`, `Bytes scrubbed: 13683916800  (45.00%)`).
  Re-export it from `cli/src/test_fixtures.rs`.
- `cli/src/status.rs` tests: add `status_scrub_running_reports_pct`, modeled on
  `status_scrub_finished` -- seed the fixture via `MockRunner::with_output`, call
  `get_scrub_report(&runner, &status_mp())`, and assert
  `ScrubReport::Running { pct: Some(_) }`. `Some(_)` (not exact) for the same
  reason as section 4: the swap is unexpressible at the call site, so this test
  pins only that `get_scrub_report`'s `Running` arm reaches `scrub_running_pct`
  and surfaces a pct -- a path no existing status test exercises (today's lone
  running test hand-builds `ScrubReport::Running { pct: Some(42) }`, bypassing
  the wiring). The exact value stays with the helper test.
- **No separate status no-bytes e2e test** (unlike idle's
  `busy_when_scrub_running_no_bytes`). Idle needs its no-bytes case because pct
  presence gates a Busy-vs-Idle *decision*; `get_scrub_report`'s `Running` arm is
  uniform -- every running scrub maps to `ScrubReport::Running { pct }` regardless
  of the counters -- so the `pct: None` path is covered by composition (the
  section-3 helper `None` cases x this test proving the arm is reached). If
  symmetric coverage is preferred, add a cheap second case: a counterless running
  fixture asserts `Running { pct: None }` (not `Unknown`). Defaulting to the
  composition note; the second case is optional.

## Files

- `cli/src/progress.rs` -- new `scrub_running_pct(&ScrubState)` helper (+ `use
  crate::parse::ScrubState;`) + its tests.
- `cli/src/idle.rs` -- hoist the helper call above the `match`; `Some(_)` +
  recomment two tests; swap the `progress::pct_from_bytes` import.
- `cli/src/status.rs` -- hoist the helper call above the `match`; new
  `status_scrub_running_reports_pct` test; adjust the `progress::` import.
- `cli/src/test_fixtures/status.rs` + `cli/src/test_fixtures.rs` -- new running fixture + re-export.

## Alternatives considered (smaller fallbacks)

- **Comment-only:** add a clarifying preamble to idle's `busy_when_scrub_running`
  and reject the weakening. Resolves the finding minimally but leaves the
  duplication and the untested `status` copy -- the next anti-drift reviewer
  refiles it.
- **Pin both inline:** keep both copies, keep idle's `Some(45)`, add a
  status end-to-end test that also asserts a concrete `Some(45)`. Closes the
  coverage gap but doubles the guard instead of removing the duplication, and
  leaves two concrete-value tests coupled to the formula's representation.
- **Positional helper + exact call-site assertions** (round-1 `Fix`):
  keep `scrub_running_pct(Option<u64>, Option<u64>)` and guard the transpose with
  exact `Some(45)` at both call sites. Rejected in favor of the round-1 `Pivot`:
  a `&ScrubState` boundary keeps the transpose *unexpressible at the call site*
  rather than test-caught, and keeps the call-site tests at the wiring level
  (formula owned once). The type removes the swap from the call site; the
  residual (a future re-inline) is named in section 4.
- **Unify the TUI too** (the Medium finding's `Pivot`): make the owner return
  validated counters / a ratio so all three surfaces share counter selection.
  Deferred to the Scope note's future-option -- the TUI's precision divergence is
  intentional and already snapshot-guarded, so the cost (third subsystem + TUI
  snapshot re-baseline) is not justified unless cross-surface consistency becomes
  an explicit goal.

All rejected for the same reason: braid shares contracts that two commands must
agree on -- and prefers an API where the bug is unexpressible at the call site --
over duplicating the mapping and multiplying the tests that guard each copy.

## Verification

- `just test-rust` -- all Rust unit tests green; specifically the new
  `scrub_running_pct_*`, the updated idle `busy_when_scrub_running*`, the new
  `status_scrub_running_reports_pct`, and the **unchanged**
  `snapshot_scrub_tab_running` (the TUI is untouched, so its snapshot must not
  move).
- Confirm the refactor is behavior-preserving by checking the diff in
  `cmd_idle` / `get_scrub_report` is only the hoisted `let running_pct = ...` plus
  the `Running` arm now using it (terminal arms untouched), and that no scrub
  caller of `pct_from_bytes` remains in the two commands (`rg 'pct_from_bytes'
  cli/src` should show only `progress.rs`: the `pct_from_bytes` definition/tests
  and the `scrub_running_pct` body).
- Account for the third site -- the `pct_from_bytes` grep is **blind** to it,
  since the TUI does its own `f64` division: `rg 'scrubbed as f64' cli/src/tui`
  should still show `tui/view/mod.rs#scrub_table`, confirming it was *consciously
  left* per the Scope note, not silently missed.
- Confirm the swap is unexpressible at the call sites: `scrub_running_pct` takes
  a single `&ScrubState`, so there is no positional counter argument in either
  command to reverse. (The transpose the review caught -- `100%` for `45%` -- is
  removed from the call site; field selection is asserted once in the section-3
  helper test. The residual re-inline gap is acknowledged in section 4.)
- ASCII-only output check is unaffected (no user-facing strings change);
  `just test-rust` covers the parser fixtures the new scrub fixture feeds.
