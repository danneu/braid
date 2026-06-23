# Plan: doctor `pool_missing_devices` must not whitewash a hot-unplug

## Context

`braid doctor`'s `check_pool_missing_devices` (`cli/src/doctor.rs:792`) gates its
OK result on `pool.missing_devids.is_empty()` and reports **"no missing devices"**.
But `missing_devids` is deliberately btrfs-MISSING-only (`cli/src/types.rs:919-935`,
`cli/src/probe.rs:524`); it excludes `null_underlying` devices -- the LUKS-mapper-open,
backing-device-gone state that, per `docs/internals/real-world/sata-hot-unplug.md`,
is the **empirical first state after a SATA hot-unplug** and persists for minutes.

So on a freshly hot-unplugged pool (`missing_count > 0`, `missing_devids` empty),
doctor's `pool_missing_devices` says "no missing devices" while:

- `braid status` reports it missing -- `StatusReport.missing_devids` is the *union*
  (`alert_missing_devids`, pinned by `cli/src/status.rs:5194`);
- `monitor`/`ack` fire a `MissingDevice` alert for it;
- `remove-missing` deliberately keys on `missing_count` (not `missing_devids.is_empty()`)
  precisely "so null-underlying hot-unplug pools ... are not mislabeled healthy"
  (`cli/src/remove_missing.rs:379-383`).

`pool_missing_devices` is the lone path that mislabels this pool as healthy. Doctor as
a whole is not *silent* (`check_declared_disks` warns "disk not found" because the
by-id backing handle is gone), but the contradictory "no missing devices" line next to
that warning is falsely reassuring, and -- critically -- the existing branch's replace
command (`braid replace --old <missing-name> --new ...`, with `--missing-id <devid>`
offered only as an optional cross-check, never as a bare command) targets btrfs-MISSING
devids, which both `replace` (`cli/src/replace.rs:1873-1918`) and `remove-missing`
(`cli/src/remove_missing.rs:307-317`) **refuse** for null_underlying devids -- they instead
instruct: relock + `unlock --allow-degraded` to let btrfs promote the device to MISSING first.

**Outcome:** make `pool_missing_devices` null_underlying-aware with *correct*
hot-unplug remediation (promote-then-replace), keep the existing btrfs-MISSING branch
byte-identical, and add the regression coverage the original finding asked for (with the
right assertions). Fold in the safe dedup of the duplicated refusal text as a separable
unification step.

This supersedes the original finding's proposal ("add a test asserting `Warn`"), which
was misframed: the behavior is unfixed (the test would be red, not a regression guard),
and its "silent regression" impact is already covered by `declared_disks`.

## Approach

### 1. Restructure the OK arm of `check_pool_missing_devices` (`cli/src/doctor.rs:792`)

Replace the `Ok(pool) if pool.missing_devids.is_empty()` guard + single warn arm with a
single `Ok(pool)` arm that classifies into two sets and composes the message:

```rust
let missing = &pool.missing_devids;            // btrfs-authoritative MISSING (replace targets)
// null_underlying devids btrfs has not promoted yet. The `!missing.contains`
// filter is belt-and-suspenders: through the real probe these two sets are
// DISJOINT by construction -- `parse_btrfs_filesystem_show` routes each devid
// line into EITHER `devices` (-> null_underlying) OR `missing_devids`, never
// both. (This differs from `PoolState::alert_missing_devids`, whose
// monitor/device-stats inputs genuinely can overlap; do NOT claim parity.)
let hot_unplugged: Vec<Devid> = pool.null_underlying.iter()
    .map(|d| d.devid)
    .filter(|d| !missing.contains(d))
    .collect();

if missing.is_empty() && hot_unplugged.is_empty() {
    return CheckResult::ok("pool_missing_devices", "no missing devices");
}

let mut segments: Vec<String> = Vec::new();
if !missing.is_empty() {
    segments.push(/* EXISTING replace-guidance message, verbatim */);
}
if !hot_unplugged.is_empty() {
    segments.push(repair_hint::null_underlying_hot_unplug_hint(&hot_unplugged));
}
CheckResult::warn("pool_missing_devices", segments.join(" "))
```

- The `Err(e)` arm is unchanged.
- The btrfs-MISSING segment must be the current format string **verbatim** (singular/plural
  suffix, `optional_missing_id_cross_check_phrase`, the `Use braid status...` tail) so the
  five existing tests (`doctor.rs:5561,5580,5617,5683,5737`) stay green for the
  `missing`-non-empty / `hot_unplugged`-empty case. Only *append* a second segment when
  `hot_unplugged` is non-empty (mixed case); MISSING segment goes first.
- Net behavior change is confined to the two new cases: pure hot-unplug (was OK -> now Warn)
  and mixed (was Warn-MISSING-only -> now Warn with an added hot-unplug segment).

### 2. Add `repair_hint::null_underlying_hot_unplug_hint` (`cli/src/repair_hint.rs`)

Doctor-tailored, ASCII-only (CLI output ASCII rule), singular/plural aware. Reuses
`repair_hint::missing_replace_command(None)` for the post-promotion replace command.

This step **introduces** the shared promotion-phrase `const` -- the load-bearing fragment
`relock and re-unlock the pool degraded (`braid lock` then `braid unlock --allow-degraded`)`
-- because the hint needs it *unconditionally* (the truly-gone branch below emits it on every
hot-unplug warn). Define it here, not in step 3, so the "steps 1, 2, 4 stand alone"
separability guarantee stays honest: the hint must not reference a symbol that only the
deferrable dedup step creates. Step 3 then *reuses* this same const for the refusal dedup, so
the invariant remediation cannot drift across the three callers regardless of whether step 3
ships.

It must give **two** recovery
paths, because per `docs/internals/real-world/sata-hot-unplug.md` a zombie mapper stays
`device: (null)` even after the drive is replugged -- the doc's key finding is the mapper is
"broken until closed and reopened", so re-seating alone is *not* sufficient. The
close-then-reopen primitive is `braid lock` then `braid unlock`; phrase the re-seat branch
as "relock and re-unlock" (consistent with the reconciled doc per the Files section), not as
"replug fixes it". Singular form (`devids == [2]`):

```
pool has 1 hot-unplugged device (devid: 2): LUKS mapper open but backing device gone;
btrfs has not yet promoted it to MISSING. If the disk is back, relock and re-unlock the
pool (`braid lock` then `braid unlock`) -- the mapper stays a zombie until closed and
reopened, even after replug. If it is truly gone, relock and re-unlock the pool degraded
(`braid lock` then `braid unlock --allow-degraded`) so btrfs promotes it, then `braid
replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>`. Use `braid status`
to see the disk's name.
```

Plural mirrors it (`devices`, `devids: 2, 4`, `the disk names`) but keeps replacement
**per-disk**: render a single base `braid replace --old <missing-name> --new ...` and, like
the existing plural MISSING message's "Use one of the listed IDs", instruct the operator to
recover one disk at a time -- not "replace them", which would read as one command covering
all hot-unplugged disks. Crucially it does **not** emit `braid replace --missing-id <devid>`
-- that command is refused for these devids. Unit-test it in `repair_hint.rs`'s existing
`assert_eq!` style (mirror the `missing_replace_command_*` tests), covering both the singular
and plural exact strings.

### 3. Dedup the duplicated refusal text (separable unification)

The refusal strings in `cli/src/replace.rs:1875-1881` and
`cli/src/remove_missing.rs:309-315` are **byte-identical except the command name**
(`braid replace` vs `braid remove-missing`). Extract:

```rust
// repair_hint.rs
pub(crate) fn null_underlying_refusal(devid: Devid, command: &str) -> String { /* exact text */ }
```

Have both call sites delegate (`replace`'s `null_underlying_refusal` closure -> wrap in
`ReplaceError::Validation`; `remove_missing`'s inline branch -> wrap in the `Err(String)`).
Because the helper reproduces the exact bytes, the four pinned `assert_eq!` tests stay
green unchanged: `replace.rs` `missing_id_null_underlying_refused` (:2750),
`auto_resolve_null_underlying_refused` (:2786); `remove_missing.rs`
`plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing` (:3049),
`validate_missing_id_target_null_underlying_only_rejected` (:3123).

`null_underlying_refusal` **reuses** the shared promotion-phrase `const` that step 2
introduces (the `relock and re-unlock the pool degraded (...braid unlock --allow-degraded...)`
fragment), rather than introducing it here -- so the const's owner is the unconditional caller
(the hint), and the no-drift invariant holds whether or not step 3 ships. The refusal must
still reproduce its exact surrounding bytes (only the embedded fragment comes from the const)
so the four pinned `assert_eq!` tests stay green.

> This step is cleanly separable. If the maintainer prefers a surgical doctor-only pivot,
> steps 1, 2, 4 stand alone (step 2 already owns the shared promotion-phrase const, so nothing
> in the 1/2/4 bundle depends on step 3); ship step 3 as a follow-up. Recommendation: include
> it -- it is the unification the verify-issue pass flagged and is low-risk (byte-exact, tests
> green).

### 4. Tests

The check is pure classification over `ctx.pool_state`, so test the segment-composition
cases by **seeding `ctx.pool_state` directly** with a hand-built `PoolState` -- the
established pattern at `doctor.rs#metadata_pressure_with_cached_pool_state` (`:5200`:
`mountpoint_ok()` runner + `ctx.pool_state = Some(pool_state)`) and `doctor.rs:5619`
(`Some(Ok(pool_state))`). `ensure_pool_state` no-ops when the cache is pre-seeded
(`doctor.rs:701`), so the injected state is exactly what the check reads, and
`mountpoint_ok()` satisfies the `ensure_mountpoint_is_mounted` gate. `PoolState` /
`PoolDevice` / `NullUnderlyingDevice` have public fields (`types.rs`), so build them inline
(the check reads only `missing_devids` and `null_underlying`; other fields just need to be
plausible). This avoids re-driving `probe_pool` -- whose `(null)` routing is already pinned
by `probe.rs#probe_pool_device_null_underlying` -- and deletes the elaborate
`pool_state_runner_with_null_underlying` fixture and its justifying prose entirely.

Injection tests (call `check_pool_missing_devices` directly):

- `pool_missing_devices_warns_on_null_underlying_hot_unplug` -- `missing_devids = []`, one
  `null_underlying` devid (2). Assert `Warn`; names `devid: 2`; gives both recovery paths
  (`braid unlock` and `braid unlock --allow-degraded`); does **not** contain `--missing-id 2`.
  (The original finding's salvaged test, corrected.)
- `pool_missing_devices_mixed_missing_and_hot_unplug` -- `missing_devids = [Devid::new(3)]`,
  one `null_underlying` devid (2). Assert `Warn`; message contains the replace command (for
  3) **and** the hot-unplug guidance (for 2); does not recommend `--missing-id 2`.
- (Existing `pool_missing_devices_ok_when_healthy` at `:5561` stays; it already asserts OK /
  "no missing".)

One end-to-end test -- pins the motivating contradiction and the probe -> report wiring:

- `pool_missing_devices_warn_through_full_probe` -- build a runner **inline** (no reusable
  helper) emitting a `(null)` cryptsetup status for one mapper (reuse the polymorphic
  `doctor_cryptsetup_status_active(mapper, "(null)")`), one healthy member with status +
  `luksUUID`, a `doctor_btrfs_show(present, &[])` with no MISSING line, and `mountpoint_ok()`;
  drive it through `run_doctor` (the style of `pool_missing_devices_ok_when_healthy` at
  `:5561`). **To actually reproduce the motivating two-check contradiction**, persist pool.json
  membership via `save_doctor_membership` (`doctor.rs:2117`, as
  `check_foreign_luks_uuid_fails_when_pool_has_unknown_uuid` does) for the hot-unplugged member
  with a by-id path **absent from the runner's filesystem** -- mirroring the hot-unplug reality
  where the by-id symlink vanishes ~11s post-unplug, so `check_declared_disks` probes the
  persisted path, gets `DiskState::Missing` (`doctor.rs:355,440`), and warns "disk not found".
  Without this, the test (modeled on `pool_missing_devices_ok_when_healthy`, which saves no
  membership) would have `declared_disks` *skip* (`doctor.rs#declared_disks_skips_when_no_membership`),
  leaving the contradiction unreproduced. Assert in the **one** assembled report that
  `pool_missing_devices` is `Warn` **and** `declared_disks` is `Warn` -- this is the only test
  that exercises probe -> report and pins the actual fix to the motivating bug: doctor no longer
  prints "no missing devices" next to the `declared_disks` warning on a hot-unplugged pool. This
  also turns the Context's load-bearing "doctor is not silent because `declared_disks` warns"
  claim into a genuine guard rather than narrative.

**repair_hint** unit tests for `null_underlying_hot_unplug_hint` (singular + plural exact
strings) and, if step 3 is included, the byte-exact `null_underlying_refusal` output for both
command names.

## Files

- `cli/src/doctor.rs` -- restructure `check_pool_missing_devices`; add three tests (two
  `pool_state`-injection composition tests + one end-to-end `run_doctor` test). No new
  test-fixture helper -- the e2e runner is built inline from existing fixtures
  (`doctor_cryptsetup_status_active`, `doctor_btrfs_show`, `mountpoint_ok`,
  `save_doctor_membership` for the absent-by-id-path member so `declared_disks` warns).
- `cli/src/repair_hint.rs` -- add `null_underlying_hot_unplug_hint` and the shared
  promotion-phrase `const` it consumes (step 2, unconditional); add `null_underlying_refusal`
  reusing that const (step 3 only); unit tests.
- `cli/src/replace.rs`, `cli/src/remove_missing.rs` -- delegate to `null_underlying_refusal`
  (step 3 only).
- `docs/commands/doctor.md` -- the check's documented contract changes, so update it:
  the `What it checks` row (`doctor.md:89`) currently reads "No btrfs missing devices in the
  live pool"; widen it to cover null-underlying hot-unplug detection (e.g. "No missing or
  hot-unplugged devices in the live pool; warns on btrfs-MISSING and on null-underlying
  (mapper open, backing device gone) members"). Also update the under-the-hood summary
  (`doctor.md:120`, "probes for missing devices") to mention the hot-unplug case.
- `docs/internals/real-world/sata-hot-unplug.md` -- reconcile the `## Recovery path` section
  (currently "Reboot -> `braid unlock`") with the doc's own key finding ("broken until closed
  and reopened") and with the refusal/hint wording: document the `braid lock` -> `braid unlock`
  (`--allow-degraded` when the disk is truly gone) close-and-reopen primitive. Note it performs
  the same stable by-id LUKS reopen a post-reboot `braid unlock` does -- so the plain-`unlock`
  re-seat branch rests on that mechanistic equivalence, not on a separately observed
  replug-then-`unlock` test -- and keep the reboot path as the hardware-validated instance.
  AGENTS.md routes recovery-messaging changes through `luks-unlock.md`; the new hint adds no
  local `/var/lib/braid/luks-headers/`-path references, so it respects that file's messaging
  invariant.
- Optional: note the doctor behavior in `docs/internals/tool-behavior/device-disappearance.md`
  (state table currently covers probe/monitor/alert; doctor now warns too).

## Verification

- `just test-rust` (or `cargo test -p braid-cli`). Confirm: the four pinned refusal
  `assert_eq!` tests pass unchanged; the five existing `pool_missing_devices` tests pass
  unchanged; the new doctor + repair_hint tests pass.
- TDD order per AGENTS.md: write the injection composition tests and the end-to-end test
  first against current code, confirm the pure-hot-unplug cases fail with "no missing devices"
  returned as `Ok` (the exact bug), then implement steps 1-2 to green them.
- `cargo clippy -p braid-cli` clean.
- `scripts/docs/check-output-ascii.py` passes (new message is ASCII-only).
- `just docs-build` passes (mdbook-linkcheck2 validates the edited `docs/commands/doctor.md`
  and `docs/internals/real-world/sata-hot-unplug.md`, including any anchors they reference);
  confirm the `What it checks` row + under-the-hood summary describe the null-underlying
  contract and the reconciled Recovery-path section names the lock/unlock primitive.
- No new doctor check is added (an existing check's message is extended), so the doctor
  check roster / table parity is unaffected -- spot-check that no check-count doc needs updating.

## Implementation notes

- **Net-new-deltas-only scope.** The core fix this plan targets (steps 1-3: the
  doctor OK-gate / warn-routing restructure, the shared promotion-phrase helper, and the
  replace/remove-missing refusal dedup) was already implemented and committed in
  `01ab69b5` ("fix(doctor): warn on hot-unplugged pool members"), promoted as
  `plans/impl/2026-06-23-doctor-hot-unplug-missing-devices.md`. That commit used a
  different but equivalent design: the OK gate keys on `missing_count == 0`, and a single
  shared `repair_hint::hot_unplug_not_yet_missing(devid, actor)` is reused verbatim across
  doctor, `replace`, and `remove-missing` (in place of this plan's proposed
  `null_underlying_hot_unplug_hint` + `null_underlying_refusal` + shared-`const` split).
  Per the operator's explicit decision, this session implemented only the genuinely-missing
  net-new deltas and left the committed code/function-naming untouched (re-applying steps
  1-3 would re-litigate merged code). Delivered: step 4's end-to-end `run_doctor` test, and
  the Files-section doc reconciliation (`sata-hot-unplug.md` Recovery path +
  `device-disappearance.md` Null-underlying note). `docs/commands/doctor.md` was already
  updated by `01ab69b5`.
- **E2e test reproduces the contradiction faithfully.** `pool_missing_devices_warn_through_full_probe`
  persists *both* pool members (disk1 healthy and resolving via block-device + isLuks +
  luksUUID; disk2's by-id symlink absent) rather than the plan's literal single
  hot-unplugged member. This makes `declared_disks` warn about exactly disk2 and keeps
  `foreign_luks_uuid` green, so the assembled report cleanly pins the motivating
  contradiction (`pool_missing_devices` Warn next to `declared_disks` Warn) without an
  unrelated foreign-UUID failure muddying the story.
- **Assertions track the committed message, not the plan's proposed wording.** The e2e test
  asserts the strings the shipped code emits (`pool has 1 missing device (devid: 2)`,
  `hot-unplugged`, and the absence of `no missing`), matching the `hot_unplug_not_yet_missing`
  design rather than this plan's `null_underlying_hot_unplug_hint` text.

## Follow Up

- `cargo clippy -p braid-cli --all-targets` is not fully clean on the base tree (two
  pre-existing warnings unrelated to this diff): a collapsible `if` at `cli/src/add.rs:5622`
  and an `unnecessary_get_then_check` at `cli/src/parse/upsc.rs:284`. The plan's Verification
  expected clippy clean; neither was introduced here.
