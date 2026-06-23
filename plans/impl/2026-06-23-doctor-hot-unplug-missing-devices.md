# Fix doctor false-green on hot-unplugged (null_underlying) pool member

## Context

`braid doctor`'s `check_pool_missing_devices` prints a green `no missing devices`
when a pool member has been hot-unplugged but its LUKS mapper is still open
(`null_underlying`). The OK branch keys on `pool.missing_devids` (btrfs's
authoritative MISSING list), which stays empty in this state because btrfs still
sees the mapper node. Meanwhile `missing_count` is already >= 1.

This directly contradicts the rest of braid on the same pool:
- `status` reports the pool **Degraded** via `missing_count` (`status.rs:514`) and
  enumerates the devid via `alert_missing_devids()` (`status.rs:610`).
- `monitor`/`ack` fire `MissingDevice` alerts on the same union.
- doctor's own `check_enospc_risk` keys on `missing_count > 0` (`doctor.rs:873`),
  so the two doctor checks disagree about whether the pool is degraded.
- `principles.md` and `internals/tool-behavior/device-disappearance.md` both
  treat `null_underlying` as a real missing-device state (the *empirical first
  state* after a SATA hot-unplug, i.e. the common case).

Outcome: an operator running `braid doctor` after a cable/enclosure drop gets a
false all-clear on the exact check meant to catch lost redundancy.

This is a **pivot** on the original finding. The finding's OK-gate diagnosis is
correct, but its proposed remedy -- union `alert_missing_devids()` into the
existing `braid replace` recommendation -- is the wrong shape: `braid replace`
(and `remove-missing`) **refuse** a `null_underlying`-only devid
(`replace.rs:1873`, `remove_missing.rs:307`), so that guidance would tell the
operator to run a command braid rejects. The correct fix routes each missing-
device sub-state to its own remediation.

## Design decisions (grounded in braid's docs)

1. **OK gate keys on `missing_count == 0`**, not `missing_devids.is_empty()`.
   Equivalent to "both `missing_devids` and `null_underlying` empty" (probe
   derives `missing_count = total - devices.len()` and excludes both), and it
   makes `check_pool_missing_devices` agree with `check_enospc_risk` and
   `status`.

2. **Warn message routes by sub-state**, because the right next action differs:
   - btrfs-MISSING devids (`pool.missing_devids`) -> existing replace
     recommendation (unchanged shape; preserves current tests).
   - `null_underlying`-only devids -> hot-unplug remediation covering **both**
     correct paths, in likelihood order.

3. **Re-plug alone does not recover a `null_underlying` mapper.** Validated on
   real hardware (`internals/real-world/sata-hot-unplug.md`: the kernel
   re-attaches the disk under a new node; "the mapper is permanently broken until
   closed and reopened"; echoed by `remove.rs:402`, `pool.rs:53`). doctor's
   recover guidance must say re-plug **then** `braid lock`/`braid unlock` to
   close+reopen the mapper -- never a bare re-seat.

4. **Single source of truth for the promote-then-act sentence.** `replace.rs:1875`
   and `remove_missing.rs:309` are byte-identical except the command name. Extract
   them into one `repair_hint` helper and have doctor's if-gone branch reuse it.
   `remove.rs:323`/`:400` are *different* journal-state messages and stay as-is
   (merging them would flatten real distinctions). doctor reuses the helper
   **verbatim**, accepting its slightly command-centric tail ("...so btrfs
   promotes devid N, and retry"): the antecedent `braid replace` is named in the
   same sentence, and doctor's recover-branch prefix ("If the disk is healthy,
   ...") already brackets the helper as the if-gone path, so "retry" is inferable.
   Keeping one byte-identical string across all three commands outweighs forking a
   doctor-specific variant.

5. **Mixed pools tighten two facets of the guidance** (a pool with both
   btrfs-MISSING and `null_underlying` members):
   - The btrfs-MISSING `--missing-id` cross-check sources devids only from
     `pool.missing_devids`, and in mixed output names them explicitly rather than
     referring to "the listed IDs" -- because the lead lists the union, a vague
     reference would imply a `null_underlying` devid is replaceable, which
     `replace` rejects.
   - The hot-unplug recover branch uses `braid unlock --allow-degraded` when any
     btrfs-MISSING member remains: re-plugging the hot-unplugged disk does not
     make the pool whole, so a plain `braid unlock` is refused
     (`mount.rs#format_degraded_refused`, `unlock.md`). Pure hot-unplug (no
     btrfs-MISSING) keeps plain `braid unlock`, since re-plugging restores the
     full member set.

## Changes

### 1. New shared helper -- `cli/src/repair_hint.rs`

Add (module already imports `Devid`):

```rust
/// Operator remediation for a member btrfs has not yet promoted to MISSING:
/// the LUKS mapper is open but its backing device is gone (`null_underlying`).
/// `actor` names the command that only acts on btrfs-MISSING devids so the
/// promote-then-retry guidance points back at the right command. `replace`,
/// `remove-missing`, and `doctor` share this exact wording.
pub(crate) fn hot_unplug_not_yet_missing(devid: Devid, actor: &str) -> String {
    format!(
        "devid {devid} is hot-unplugged but btrfs has not yet promoted it to \
         MISSING (LUKS mapper open, backing device gone). `{actor}` only \
         operates on btrfs-authoritative MISSING devids. Confirm the disk is \
         truly gone, then relock and re-unlock the pool degraded (`braid lock` \
         then `braid unlock --allow-degraded`) so btrfs promotes devid {devid}, \
         and retry."
    )
}
```

The rendered output must stay **byte-identical** to today's strings so the
existing `replace`/`remove-missing` unit tests pass unchanged. Add one
`repair_hint` unit test (Intent/Why/Scenario preamble) asserting both actor
renderings exactly.

### 2. Rewire the two byte-identical call sites

- `cli/src/replace.rs` -- the `null_underlying_refusal` closure body (`~:1873`)
  becomes `ReplaceError::Validation(repair_hint::hot_unplug_not_yet_missing(devid, "braid replace"))`.
- `cli/src/remove_missing.rs` -- the inline `format!` (`~:308`) becomes
  `repair_hint::hot_unplug_not_yet_missing(missing_id, "braid remove-missing")`.

Both files already `use crate::repair_hint;`. Their existing exact-string unit
tests (`replace.rs:2750/2786/2815`, `remove_missing.rs:3071/3131`) remain valid.

### 3. Fix `check_pool_missing_devices` -- `cli/src/doctor.rs:792-848`

- OK gate (`:807`): `Ok(pool) if pool.missing_count == 0`.
- Warn branch (`:810-842`): rebuild the message as:
  - **Lead:** count = `pool.missing_count`, devid list = `pool.alert_missing_devids()`
    (union, sorted). Singular/plural keyed off `missing_count`. When
    `null_underlying` is empty this is identical to today's lead.
  - **btrfs-MISSING segment** (when `!pool.missing_devids.is_empty()`): the
    existing replace recommendation. Its `--missing-id` cross-check sources devids
    **only** from `pool.missing_devids`, never the union. The cross-check *target*
    text must name those devids explicitly whenever they are a strict subset of
    the lead (i.e. `null_underlying` is non-empty -- a mixed pool), so a
    null_underlying devid that appears in the lead is never implied replaceable.
    Concretely:
    - `null_underlying` empty: keep today's wording exactly -- `Use the listed ID.`
      (single) / `Use one of the listed IDs.` (plural). Existing tests stay green.
    - mixed: enumerate the btrfs-MISSING devids, e.g. `Use devid 3.` (single
      btrfs-MISSING) or `Use one of the btrfs-MISSING devids: 3, 4.` (plural).
      `replace --missing-id` is therefore only ever offered for a btrfs-MISSING
      devid, matching what `replace` accepts.
  - **null_underlying segment** (for `null_underlying` devids not in
    `missing_devids`): the `not in missing_devids` subtraction is defensive-only
    and needs no doctor test -- `probe_pool` surfaces a devid in `btrfs filesystem
    show` either as a `/dev/mapper/` path (-> `null_underlying`) or as a MISSING
    sentinel (-> `missing_devids`), never both, so the two sets are disjoint in
    practice (the overlap that `replace`/`remove_missing` test via manually-built
    `PoolState` cannot reach doctor's probed input). This segment renders the
    recover branch followed by the shared helper. The recover-branch
    unlock flag is **conditional on whether other members remain missing**: plain
    `braid unlock` only makes the pool whole when no btrfs-MISSING member is left,
    so key the flag off `pool.missing_devids`:
    - `pool.missing_devids` empty (pure hot-unplug): re-plug the disk(s), then
      `braid lock` and `braid unlock`.
    - `pool.missing_devids` non-empty (mixed): re-plug, then `braid lock` and
      `braid unlock --allow-degraded` (a genuinely-gone member remains, so a plain
      unlock is refused -- `mount.rs#format_degraded_refused`, `unlock.md`).

    e.g. (pure hot-unplug form):

    > If the disk is healthy, re-plug it then run `braid lock` and `braid unlock`
    > to close and reopen the mapper (it does not self-heal on re-plug alone). +
    > `repair_hint::hot_unplug_not_yet_missing(devid, "braid replace")`

  - **Trailer:** keep `Use `braid status` to see the missing disk's name`.

No other touch points need changes: the summary-table label (`doctor.rs:1799`)
and JSON schema (`:2104`, `:3325`) reference only the check *name*; the message
flows into `CheckResult.message` untruncated.

### 4. Tests -- `cli/src/doctor.rs` test module

Use direct `ctx.pool_state = Some(Ok(PoolState{..}))` injection (the
`pool_state_runner` fixture cannot set `null_underlying`; pattern at
`doctor.rs:~5158`). Add:

- `pool_missing_devices_warns_hot_unplug_when_null_underlying` (pure hot-unplug):
  `missing_count: 1, missing_devids: [], null_underlying: [devid 2]`. Assert
  `Warn`; lead `pool has 1 missing device (devid: 2)`; contains `hot-unplugged`,
  `braid unlock --allow-degraded` (promote path) and `re-plug` + a plain
  `braid unlock` recover step (assert the recover step is **not**
  `--allow-degraded` -- e.g. the recover sentence carries `braid lock` then
  `braid unlock` without the flag); does **not** contain `replace with: ` + a bare
  `braid replace --old <missing-name>` recommendation.
- `pool_missing_devices_warns_mixed_plural_btrfs_missing_and_null_underlying`
  (load-bearing union test, mirrors `status.rs:5193`'s rationale; covers the
  **(mixed, plural)** cross-check and the mixed recover flag): `missing_count: 3,
  missing_devids: [3, 4], null_underlying: [devid 2]`. Assert `Warn`; lead
  `pool has 3 missing devices (devids: 2, 3, 4)`; the btrfs-MISSING cross-check
  names **only** `3, 4` (assert it enumerates the btrfs-MISSING devids and never
  invites `--missing-id 2`); devid 2 gets the hot-unplug guidance; and because a
  btrfs-MISSING member remains, the recover step uses
  `braid unlock --allow-degraded` (assert this form). Pins that each sub-state
  routes to its own remediation and that mixed pools never present a
  null_underlying devid as replaceable.
- `pool_missing_devices_warns_mixed_single_btrfs_missing_names_only_its_devid`
  (covers the **(mixed, single)** cross-check -- the most regression-prone branch,
  one keystroke from the non-mixed `Use the listed ID.` wording): `missing_count:
  2, missing_devids: [3], null_underlying: [devid 2]`. Assert `Warn`; lead
  `pool has 2 missing devices (devids: 2, 3)`; the cross-check renders `Use devid
  3.` and **never** `Use the listed ID.`, and never invites `--missing-id 2`;
  devid 2 gets the hot-unplug guidance with the `braid unlock --allow-degraded`
  recover step. Guards against a fallback to the non-mixed wording re-implying the
  null_underlying devid is replaceable -- the exact contradictory-guidance bug
  this plan exists to kill.

This gives all four cross-check cells coverage: `(null empty, single)` and
`(null empty, plural)` by the two existing tests, `(mixed, plural)` and
`(mixed, single)` by the two above.

Keep all five existing `pool_missing_devices_*` tests; they exercise only
`missing_devids` (with `null_underlying` empty) and stay green -- the single/plural
`Use ... the listed ID(s).` wording is unchanged in the non-mixed path.

### 5. Docs -- `docs/commands/doctor.md`

The `pool_missing_devices` row in the **What it checks** table (`doctor.md:89`)
currently reads `No btrfs missing devices in the live pool`, which no longer
matches the expanded behavior. Update it to describe both missing sub-states and
their split remediation, e.g.:

> No missing devices in the live pool -- both btrfs-authoritative MISSING devices
> and hot-unplugged members whose LUKS mapper is still open (`null_underlying`).
> **Warn** lists each missing devid and routes remediation by sub-state: a
> btrfs-MISSING devid gets a `braid replace` recommendation (the optional
> `--missing-id` cross-check names only btrfs-MISSING devids); a hot-unplugged
> member is guided to re-plug + `braid lock`/`braid unlock` to recover (or
> `braid unlock --allow-degraded` when another member is still missing), or to
> `braid lock`/`braid unlock --allow-degraded` to promote it to MISSING then
> `braid replace` if the disk is gone.

The healthy-case example output (`doctor.md:29`, `[ok] missing devs  no missing
devices`) and the "under the hood" line (`:120`) stay valid -- OK still prints
`no missing devices` and the probe step is described generically. No README
change: its doctor entry (`README.md:354`) is a one-line summary with no
per-check granularity.

## Verification

- `just test-rust` is the main lane. For a focused run use a valid filter against
  the `braid-cli` crate, e.g. `cargo test --manifest-path cli/Cargo.toml --lib`
  (the package is `braid-cli`, not `braid`; `cargo test -p braid ...` is invalid).
  New doctor tests pass; existing replace/remove-missing exact-string tests pass
  **unchanged** (proves the helper extraction is byte-equal).
- `python3 scripts/docs/check-output-ascii.py` -- new strings are ASCII-clean
  (backticks and `--` are allowed; no Unicode dashes/quotes/ellipsis).
- `just docs-build` -- mdbook builds and linkcheck passes after the `doctor.md`
  table edit.
- `cargo clippy --all-targets` and `cargo fmt --check`.
- Manual sanity: the no-missing-devices and existing single/plural btrfs-MISSING
  doctor outputs are unchanged; only the null_underlying and mixed cases gain the
  new guidance.

## Out of scope (deliberate)

- `remove.rs:323`/`:400` hot-unplug messages -- different journal-state
  remediation; not merged into the shared helper.
- `status`/`monitor`/`ack` -- already correct (they key on the union /
  `missing_count`).
