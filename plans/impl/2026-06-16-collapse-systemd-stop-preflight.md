# Pivot: collapse the systemd-stop preflight onto its single live entry point

## Context

`cli/src/preflight.rs` carries two sibling functions, born together in commit
`518a82bd` ("fix(lock): pause shutdown balances before unmount"), that wrap the
*identical* `ExclusiveOpPolicy::AllowBalanceElseReject` policy:

- `require_systemd_stop_lock_preflight` -> `Result<(), String>` (discards the op).
- `systemd_stop_lock_requires_balance_pause` -> `Result<bool, String>` (maps a
  running balance to `true` so the executor knows to pause before unmount).

Only the bool-returning one is wired to production -- `cli/src/lock.rs#lock_preflight_pause_decision`
calls it for `LockMode::SystemdStop`. `require_systemd_stop_lock_preflight` has
**never** had a production caller (verified by reading `lock.rs` at `518a82bd`); it
is a vestige of the impl plan's earlier `()`-returning sketch that the final wiring
discarded because the executor needs the pause bool. It survived unnoticed because
it is `pub`, so no dead-code lint fired. Its only references today are its own two
unit tests.

A code-review finding proposed "delete the dead function **and its two tests**."
That is half right: the function is genuinely dead, but one of those two tests is
the only *direct preflight-boundary* coverage of the `AllowBalanceElseReject`
non-balance *reject* branch -- it pins the full non-balance op matrix and the exact
refusal wording, which the command-level `cli/src/lock.rs#systemd_stop_rejects_non_balance_op`
test (a single op, `device remove`) does not. Deleting it silently narrows that
coverage. This plan deletes the dead function, deletes the one genuinely redundant
test, and **re-points** the reject test onto the surviving public boundary so the
boundary coverage is conserved.

Outcome: `systemd_stop_lock_requires_balance_pause` becomes the single
systemd-stop preflight entry point, with no loss of test coverage and one fewer
trap for a future caller to pick the wrong (bool-discarding) function.

## Coverage analysis (why re-point, not delete)

The `AllowBalanceElseReject` arm of `check_exclusive_op_with_policy`
(`cli/src/preflight.rs`, ~lines 217-223) has three outcomes:
`Balance -> Ok(Some)`, `BalancePaused -> Ok(None)`, everything else ->
`Err("cannot lock: {op} is in progress. ...")`.

Three unit tests currently sit over this:

| Test (preflight.rs) | Calls | Covers | Disposition |
|---|---|---|---|
| `systemd_stop_lock_preflight_allows_none_and_balance` (~1788-1803) | dead fn, `.is_ok()` | allow: none/balance/balance-paused | **DELETE** -- subsumed |
| `systemd_stop_lock_preflight_reports_pause_requirement` (~1805-1823) | live fn, `Ok(false/true/false)` | allow + pause-bool | **KEEP** as-is |
| `systemd_stop_lock_preflight_rejects_non_balance_ops` (~1825-1846) | dead fn, `.unwrap_err()` | **reject** branch (5 ops) | **RE-POINT** to live fn |

- Test A is fully subsumed: `Ok(false)`/`Ok(true)` from the "reports" test logically
  imply `.is_ok()` for the same three bodies. Deleting it loses nothing.
- Test C is **not** redundant. It is the only *direct* preflight-boundary test of
  the reject branch, and the only test covering the **full** non-balance matrix
  (`device add`/`remove`/`replace`, `resize`, `swap activate`) and the exact
  `cannot lock: {op} is in progress` wording. The "reports" test exercises only the
  allow/pause path; there are **no** direct unit tests of the private
  `check_exclusive_op_with_policy`. The closest other coverage,
  `cli/src/lock.rs#systemd_stop_rejects_non_balance_op`, is command-level -- it
  drives `cmd_lock_systemd_stop` and additionally asserts refuse-before-pause/umount
  -- but exercises only the single `device remove` op and asserts only `device
  remove` + `in progress`, not the other four ops and not the `cannot lock` prefix.
  The systemd-stop VM tests (`tests/module/braid-lock-systemd-stop.py`, etc.) assert
  ExecStop *orchestration* outcomes ("aborting --systemd-stop", exit-code), not the
  refusal string at all. So deleting Test C would narrow reject coverage to one op
  at the command layer and drop the wording assertion entirely.
- Re-pointing is exact: `systemd_stop_lock_requires_balance_pause` propagates the
  `Err` unchanged through `.map(...)`, so
  `systemd_stop_lock_requires_balance_pause(&fs, &fsid()).unwrap_err()` yields the
  identical string Test C asserts. The assertion body is unchanged.

Rejected alternatives: deleting Test C (narrows reject coverage to the single
command-level `device remove` case and drops the full-matrix + exact-wording
boundary coverage); merging it into the "reports" test (fuses two distinct intents
under one `//` Intent/Why/Scenario preamble, against the one-intent-per-test
convention); adding direct tests of the private helper (violates the "behavioral,
structure-insensitive, test through public boundaries" rule in `docs/dev/testing.md`).

## Changes

All edits are in **`cli/src/preflight.rs`**. No other source file changes; no
public re-export names the function (`cli/src/lib.rs` exposes the whole module via
`pub mod preflight;`, `cli/src/main.rs` imports the module, neither names the fn),
no `#[allow(dead_code)]` to remove, no doc `## See` citation references it.

1. **Delete `require_systemd_stop_lock_preflight`** and its doc comment
   (~lines 689-698, from the `/// Guard for the systemd shutdown stop path.`
   opener through the closing `}`).

2. **Delete Test A** `systemd_stop_lock_preflight_allows_none_and_balance` with its
   `//` preamble (~lines 1788-1803).

3. **Re-point Test C** `systemd_stop_lock_preflight_rejects_non_balance_ops`
   (~lines 1825-1846): change the single call at ~line 1840 from
   `require_systemd_stop_lock_preflight(&fs, &fsid()).unwrap_err()` to
   `systemd_stop_lock_requires_balance_pause(&fs, &fsid()).unwrap_err()`.
   Add one line to its preamble noting it is the direct preflight-boundary coverage
   of the full non-balance op matrix and the exact refusal wording (the
   command-level `cli/src/lock.rs#systemd_stop_rejects_non_balance_op` test exercises
   only the `device remove` case), so a future editor does not delete it as
   redundant. Do **not** rename the test -- its
   `systemd_stop_lock_preflight_*` prefix is a behavior namespace parallel to the
   `lock_preflight_*` and `mutation_preflight_*` test families, and "systemd-stop
   lock preflight rejects non-balance ops" remains accurate of the surviving
   boundary.

4. **Tighten the surviving doc comment** on `systemd_stop_lock_requires_balance_pause`
   (~lines 700-704): add one line marking it the sole systemd-stop exclusive-op
   preflight gate (the pause bool is a side product of that single sysfs read).
   This records the WHY of the now-merged boundary and discourages re-introducing a
   parallel `require_*` guard. Keep the existing line documenting the reject
   behavior.

Net: -1 `pub fn`, -1 test, +0 new symbols, 0 coverage lost.

## Verification

- `cargo test -p braid-cli` (or `just test-rust`) -- the systemd-stop *preflight*
  unit-test trio drops to two, both green; the re-pointed reject test still asserts
  the `cannot lock: ... is in progress` string across the full matrix (device
  add/remove/replace, resize, swap activate). The command-level complement
  `cli/src/lock.rs#systemd_stop_rejects_non_balance_op` is untouched and stays green.
- `cargo build -p braid-cli` -- confirms no dangling reference and no newly-unused
  import.
- `grep -rn require_systemd_stop_lock_preflight cli/` -- expect zero hits.
- `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) -- confirms
  no dead-code or unused-import warning introduced.
- Behavior is otherwise unchanged: `cli/src/lock.rs` already calls only
  `systemd_stop_lock_requires_balance_pause`, so the systemd-stop VM tests
  (`tests/module/braid-lock-systemd-stop.py`) need no edit and should be unaffected.

## Implementation notes

- The plan called for "one line" both in Test C's preamble and in the surviving
  doc comment, but neither fit on a single physical line. Test C's note was folded
  into its existing `// Why it exists:` block (continuation lines), and the doc
  comment gained a three-line `///` paragraph, to match the file's multi-line
  comment conventions rather than introduce a literal one-liner.
