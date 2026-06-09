# Fix and de-duplicate ENOSPC balance-recovery guidance

## Context

braid surfaces "how to recover from balance/unallocated ENOSPC" advice in several
places, and the copies have **drifted**. The proactive advisory in
`capacity.rs#enospc_risk_advisory` recommends:

```
btrfs balance start -dusage=0 -musage=0 <mount>
```

This is wrong on two counts:

1. **`-musage=0` balances metadata.** Even at usage=0 it strips metadata
   block-group headroom; in the low-unallocated regime this advisory fires in,
   that can push the pool toward a metadata ENOSPC -> read-only. The reactive
   sibling `pool.rs#balance_error` and the `doctor.rs` metadata-pressure check
   both deliberately stay **data-only** and have tests asserting
   `!contains("musage") && !contains("mconvert")`. `capacity.rs` is the *only*
   site with no such test -- which is exactly why it drifted.
2. **`-dusage=0` is a near no-op** for the advisory's stated goal (free
   unallocated so RAID1 chunks can allocate). The kernel auto-reclaims fully
   empty groups already; you need `-dusage=50` to compact partially-full data
   chunks. `doctor.rs:1109` already uses `-dusage=50` for the analogous case.

The advisory also omits braid's **own** primary remedy. `braid add` adds
capacity and auto-rebalances (`add.rs#execute`), and `remove.rs` already tells
users *"add a new device first with `braid add`"*. The proactive advisory should
lead with it.

This was prompted by reviewing GitHub issue #22 (balance ENOSPC handling).
Issue #22's "Option B" (auto-`-dusage=0` recovery) was judged not worth building;
this plan is the genuinely-correct subset that fell out of that review.

**Outcome:** one canonical data-only balance command shared across the proactive
sites, correct content everywhere, the data-only invariant made *structural*
(unrepresentable in the helper) and *test-locked* at the site that lacked it,
and docs in sync. No new CLI verb (see Out of scope).

## Approach

### 1. Shared data-only command helper (new)

Add to `cli/src/capacity.rs` (already the home of `enospc_risk_advisory`, and
already a dependency of both `status.rs` and `doctor.rs`):

```rust
/// Canonical data-only balance command for the proactive ENOSPC guidance in
/// `enospc_risk_advisory` and `doctor`'s metadata-pressure check (not pool.rs's
/// reactive hint). No metadata parameter, so `-musage`/`-mconvert` is unrepresentable here.
pub(crate) fn compact_data_command(mount: &str, usage: u8) -> String {
    format!("btrfs balance start -dusage={usage} {mount}")
}
```

Note `mount: &str` so `capacity.rs` can pass the literal placeholder `"<mount>"`
(it has no `MountPoint` in scope and the current advisory already uses `<mount>`),
while `doctor.rs` passes the real mount path. The helper has **no metadata
parameter** -- that is the structural guarantee. Name is a suggestion.

### 2. Fix `capacity.rs#enospc_risk_advisory` content

Replace the message (keep the `"ENOSPC risk:"` prefix and `"{count} of {N}
devices"` shape so existing assertions still hold):

```
ENOSPC risk: {count_below} of {N} devices have less than {T} unallocated -- if a
disk fails, the pool may be unable to allocate RAID1 chunks to restore
redundancy. Add capacity with 'braid add', delete unneeded files or snapshots,
or compact data chunks with '{cmd}' (data only; do not balance metadata).
```

where `cmd = compact_data_command("<mount>", 50)`. (Per decision: keep the raw
`btrfs` command as the fragmentation escape hatch; lead with `braid add`.)

### 3. Route `doctor.rs` metadata-pressure check through the helper

In `check_metadata_enospc_pressure` (message ~`doctor.rs:1108`), replace the
inline `` `btrfs balance start -dusage=50 {mount_point}` `` with the helper output
(`compact_data_command(&mount_point.0, 50)`), preserving the surrounding backtick
quoting and prose. Text content is unchanged; this just makes the literal shared.
`pool.rs#balance_error` is intentionally **left as-is** (reactive, multi-line
`0 -> 20 -> 50` ladder, already correct and test-locked).

### 4. Lock the invariant at `capacity.rs` (the gap)

Extend the existing advisory tests (`enospc_risk_advisory_fires_on_2_disk_pool_with_one_low`,
`..._3_disk_loss_simulation`) -- or add one focused test -- to assert the rendered
advisory:
- `!contains("musage") && !contains("mconvert")` (mirrors `pool.rs:1251`, `doctor.rs:5078`)
- `contains("braid add")`
- `contains("-dusage=50")`

### 5. Sync docs (hand-mirrored, not generated)

- `docs/commands/status.md` (~line 336): update the example `warning:` line to the
  new advisory text verbatim.
- `docs/guides/troubleshooting.md` ("Balance fails with No space left on device"):
  drop `-musage` from both commands (`-dusage=0 -musage=0` -> `-dusage=0` at ~line 16;
  `-dusage=10 -musage=10` -> `-dusage=10` at ~line 37). Revise the prose (~line 19)
  so it states the recovery is **data-only** and briefly why (metadata block groups
  are kept as write headroom; balancing them risks metadata ENOSPC -> read-only).
  Keep the accurate "no work space" point about `usage=0` and the existing
  cause-explanation about braid's internal convert balances touching both profiles.

## Critical files

- `cli/src/capacity.rs` -- new `compact_data_command` helper; fix advisory; add invariant test
- `cli/src/doctor.rs` -- route metadata-pressure message through the helper
- `docs/commands/status.md` -- mirror new advisory text
- `docs/guides/troubleshooting.md` -- drop `-musage`, fix prose
- (unchanged, by decision) `cli/src/pool.rs#balance_error` -- already correct + tested

## Reuse / existing patterns

- `braid add` recommendation tone: copy `remove.rs` ("add a new device first with
  `braid add`") -- established precedent, not novel.
- `braid add` auto-rebalances (`add.rs#execute`, ~line 1575), so recommending it as
  a complete remedy is honest -- no manual balance follow-up needed.
- Data-only invariant test shape already exists at `pool.rs#balance_error` tests and
  `doctor.rs` metadata-pressure tests -- copy it to `capacity.rs`.
- `format_bytes` (already imported in `capacity.rs`) for the threshold.

## Verification

- `just test-rust` -- exercises the updated `capacity.rs` tests plus the unchanged
  `pool.rs` / `doctor.rs` / `remove_missing.rs` invariant tests (regression guard
  that the data-only rule still holds at every site).
- ASCII guard: run `scripts/docs/check-output-ascii.py` over `cli/src` (new strings
  use `--`, `'...'`, ASCII only -- should pass).
- `just docs-build` -- mdbook + `mdbook-linkcheck2` for the two edited `.md` files
  (prose-only edits; no link changes expected).
- Drift guard (scoped to the changed recovery surfaces; drop `mconvert` -- every
  one of its ~26 repo hits is a legitimate `-mconvert=raid1`/`,soft`/`dup`
  conversion or a `!contains("mconvert")` test assertion, pure noise here):
  - `rg -n "musage" docs/commands/status.md docs/guides/troubleshooting.md` --
    expect **zero** matches (both docs fully de-`musage`'d).
  - `rg -n "musage" cli/src/capacity.rs` -- expect matches **only** inside the test
    module (the new `!contains("musage")` invariant assertion), never in a
    user-facing string.
  - Targeted eyeball of the two correct-but-unchanged sites: re-read
    `pool.rs#balance_error` and `doctor.rs#check_metadata_enospc_pressure` hint text
    to confirm they remain data-only (they are this plan's correctness baseline).
- Eyeball: confirm `build_status_warns_on_enospc_risk` (status.rs) still passes and
  the rendered `warning:` line reads as intended.

## Out of scope (explicit non-goals)

- **No new `braid balance` / `braid compact` verb.** ADR 012 (Active, Intent CLI)
  deliberately removed ad-hoc operation surface; a maintenance verb would regress
  it. The raw `btrfs balance` stays a documented escape hatch, exactly as braid
  already leaks `btrfs filesystem usage` for diagnosis and `btrfs scrub cancel`.
- **No managed/auto compaction** (timer-driven like scrub, or threshold-triggered).
  Defensible but a real feature with I/O-load and idle/autosuspend interactions --
  its own ADR if telemetry ever shows operators hit fragmentation often.
- **No ADR/principle change.** The data-only invariant already exists (test-enforced);
  this hardens it (structural helper + the missing test + helper doc comment), it
  does not introduce or alter an invariant.
- `pool.rs#balance_error` text is not refactored (per scope decision -- correct and
  test-locked; routing it through the helper would churn working code).
