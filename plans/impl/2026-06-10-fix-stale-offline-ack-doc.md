# Fix the stale offline-ack behavior in `docs/commands/ack.md`

## Context

Commit `2ababfdd` ("fix(ack): persist missing-device acks taken while pool is
offline") reworked offline `braid ack` into a per-cause branch and documented it
in the authority ADR (`docs/design/decisions/014-alerts.md#offline-ack-policy`),
but never updated the end-user command page. `docs/commands/ack.md` still
describes the pre-`2ababfdd` behavior: its offline paragraph claims ack "still
clears the latch and flag" for any offline alert, with a smartd-only example.

That is wrong in two ways against `cli/src/ack.rs#ack_offline`:

1. **btrfs device errors are refused, not cleared.** When the latch holds any
   `BtrfsDeviceErrors` cause, offline ack returns `OfflineBtrfsErrorsRefused`
   (`cannot ack btrfs device errors while pool is offline -- unlock the pool
   first`) and touches nothing -- all-or-nothing, so a co-latched `MissingDevice`
   is not partially acked. An operator who locked a degraded pool and runs
   `braid ack` hits a hard refusal the doc says cannot happen, so it reads like a
   bug.
2. **Missing devices now persist.** Offline ack writes `missing_acked = true` to
   `acked-stats.json` for each latched `MissingDevice` before clearing the latch.
   The doc's "clears the latch and flag without snapshotting device stats" framing
   implies `acked-stats.json` is untouched, which is no longer true.

The code is correct and covered end-to-end (unit test
`ack_offline_refuses_when_btrfs_errors_mixed_with_missing` at `cli/src/ack.rs`;
integration subtest "Offline ack refused on mixed BtrfsDeviceErrors +
MissingDevice latch" at `tests/cli/braid-monitor.py`). Only the command page is
stale. Intended outcome: the command page matches the four-way per-cause reality
already in ADR 014, so the refusal no longer looks like a bug and the
missing-device persistence is visible.

## Scope

- **`docs/commands/ack.md` -- three localized edits:** rewrite the offline
  paragraph, plus two Safety-checks changes (reword the existing offline-refusal
  bullet, add a btrfs-error refusal bullet).
- **`cli/src/ack.rs` -- one focused unit test:** the broadened Safety-checks
  enumeration documents the offline-refusal gate over latch causes, `smartd-alert`,
  corrupt latch, and cleanup-pending. Three of those four offline cases are already
  pinned; a bare `smartd-alert` flag with no latch is not. Add a regression test
  that pins it (see Verification).
- **No README change; no other docs.** README only carries a one-line table entry
  for `ack` (no offline prose). The exploration found no other doc
  (`docs/guides/*`, `docs/internals/*`, other command pages) asserting the stale
  "offline ack always clears" claim, so the doc fix is isolated to this page.

## Edit 1 -- rewrite the offline paragraph

Replace the single offline paragraph (currently: "If the pool is offline but
alerts exist (e.g., a latched smartd alert), ack still clears the latch and flag
without snapshotting device stats. Offline means there is no mount at the
configured mount point. If that path is occupied by a non-btrfs filesystem, ...")
with a lead sentence + flat bullet list (house style: command pages use flat
lists for conditional behavior, never nested sub-lists; strictly ASCII `--` and
straight quotes), keyed by which alert signal is present:

> When the pool is offline (no mount at the configured mount point), `braid ack`
> cannot run `btrfs device stats`, so what it can clear depends on which alert
> signals are present:
>
> - A smartd alert -- a latched smartd cause, a bare `smartd-alert` flag present
>   at ack entry, or both -- clears any latch and removes the `smartd-alert` flag;
>   no `acked-stats.json` write is needed.
> - A latched computation error clears the latch; it re-fires on the next monitor
>   cycle only if the underlying computation still fails.
> - A latched missing device is recorded as acknowledged in `acked-stats.json`
>   (so the next monitor cycle stays quiet) and the latch is cleared, without
>   querying btrfs.
> - A latched btrfs device error is refused: ack exits non-zero with `cannot ack
>   btrfs device errors while pool is offline -- unlock the pool first` and leaves
>   all alert state untouched, because re-baselining the error counters needs live
>   `btrfs device stats`, which requires the pool mounted. The refusal is
>   all-or-nothing -- a co-latched missing device is not partially acknowledged,
>   so unlock and re-run to clear everything.
>
> If that mount point is occupied by a non-btrfs filesystem, `braid ack` returns a
> probe error naming the fstype and preserves `alert-latch.json`, `smartd-alert`,
> and `acked-stats.json`.
>
> See [ADR 014: Offline ack policy](../design/decisions/014-alerts.md#offline-ack-policy)
> for the rationale.

Notes:
- The smartd bullet covers both offline smartd signals because `ack_offline`'s
  gate is `has_alert = !causes.is_empty() || smartd_active || latch_corrupt`: a
  bare `smartd-alert` flag (`smartd_active`) with no latch is acknowledged on its
  own, and `remove_smartd = smartd_active || latch_had_smartd` removes the flag in
  either form. This is exactly the gate term the new unit test pins (see
  Verification), so the bullet, the Safety-checks wording in Edit 2(a), and the
  test all describe the same signal.
- The trailing non-btrfs-fstype sentence is preserved verbatim from the current
  paragraph (it is about offline *detection*, still accurate).
- The error string in backticks must match `AckError::OfflineBtrfsErrorsRefused`
  exactly: `cannot ack btrfs device errors while pool is offline -- unlock the
  pool first`.
- The ADR cross-link uses the same relative form already used by
  `docs/commands/status.md` (`../design/decisions/014-alerts.md`); the
  `#offline-ack-policy` anchor matches the `### Offline ack policy` heading.

## Edit 2 -- correct the Safety-checks list

Two changes in the `## Safety checks` list.

**(a) Reword the existing offline-refusal bullet.** It currently reads "If the
pool is not mounted and no alerts are latched, ack refuses with 'pool is not
mounted -- nothing to acknowledge'". That is too narrow: `cmd_ack_impl` /
`ack_offline` gate on *every* snapshotted alert source, so an offline
`smartd-alert` flag or a corrupt latch is still acknowledged with no latch cause
present -- the refusal fires only when all four sources are absent (the
cleanup-pending case is handled by the hoisted retry branch before the mount
check). Match the enumeration style the adjacent mounted-no-op bullet already
uses:

> - If the pool is offline and no alert signal is present -- no latch entries, no
>   smartd alert flag, no corrupt latch, and no pending ack cleanup -- ack refuses
>   with "pool is not mounted -- nothing to acknowledge"

**(b) Add the btrfs-error refusal bullet** immediately after (a), grouping the two
offline-refusal cases together:

> - If the pool is offline and any latched cause is a btrfs device error, ack
>   refuses with "cannot ack btrfs device errors while pool is offline -- unlock
>   the pool first" and leaves all alert state untouched (a co-latched missing
>   device is not partially acknowledged).

## Leave unchanged

- The intro (`Acknowledges active alerts ... When there is an active alert source
  on a mounted pool, it also sets the current device error counts as the new
  baseline ...`) already scopes baselining to a mounted pool -- correct, no edit.
- The "## What happens under the hood" numbered list is explicitly the mounted
  path (step 2: "If the pool is mounted") -- correct, no edit.

## Verification

- `just docs-build` -- mdBook build + `mdbook-linkcheck2`; this is the validator
  for the new in-book `#offline-ack-policy` cross-link (a broken link fails CI).
  (`just check-doc-links` does *not* cover this: `scripts/docs/check-doc-links.py`
  only scans `AGENTS.md` and `README.md`, the root files outside the book src that
  mdbook-linkcheck2 never sees.)
- Manual read-through of the rendered `ack.md` (`just docs-serve`) to confirm the
  bullet list renders and the offline refusal reads as intended behavior, not a
  bug.
- `just test-rust` -- the doc describes behavior, so each documented offline case
  must stay pinned by a unit test. Existing coverage already maps to the doc and
  remains green:
    - btrfs-error refusal: `ack_offline_refuses_when_btrfs_errors_mixed_with_missing`
      (plus the `braid-monitor.py` "Offline ack refused on mixed BtrfsDeviceErrors
      + MissingDevice latch" VM subtest).
    - missing-device persistence: `ack_offline_with_missing_device_cause_marks_missing_acked`.
    - corrupt latch cleared offline: `ack_offline_corrupt_latch_still_clears_files`.
    - cleanup-pending resumes (no refusal): `ack_offline_retry_after_cleanup_failed_completes_recovery`.
- **Add one focused unit test** in `cli/src/ack.rs` for the one gate case the
  broadened Safety-checks bullet documents but nothing pins: an offline pool with a
  bare `smartd-alert` flag present at entry and **no** latch must clear the flag and
  exit `Ok`, not return `PoolNotMounted`. Suggested name
  `ack_offline_smartd_flag_no_latch_clears_flag_not_pool_not_mounted`; mirror the
  mounted sibling `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path`
  but with `ack_fs_not_mounted()` + `AckPanicRunner`. Assert: result is `Ok`,
  `smartd_alert()` is gone, `acked_stats_json()` is absent. This pins the
  `smartd_active` term of `ack_offline`'s `has_alert` gate -- the existing offline
  smartd tests all either carry a `SmartdAlert` *cause* in the latch (gate satisfied
  by `causes`, not `smartd_active`) or have the flag arrive mid-probe (asserting
  `PoolNotMounted`), so a regression dropping that term slips through today. The
  test pins existing, already-correct behavior, so it passes as written (optionally
  confirm it bites by transiently removing the `smartd_active` term).
