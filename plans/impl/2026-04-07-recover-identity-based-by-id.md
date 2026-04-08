# Recover: resolve by_id from each live pool device's identity

## Context

`braid recover` rebuilds `pool.json` from the live mounted btrfs pool but copies each disk's `by_id` path verbatim from the union of `journal.pre_membership` + `journal.target_membership`. Those snapshots are frozen at mutation start — they are never re-validated against the live `/dev/disk/by-id/` symlinks at recovery time.

The bug bites in this narrow window:

1. The pool was already mounted when `braid recover` runs (`mount::plan_open_pool` returns `None` at `cli/src/mount.rs:70-73`, so `probe_config_disk` is never called and the journal `by_id` is never sanity-checked).
2. A `by_id` path has changed since the journal was written (USB re-enumeration, enclosure swap, port change for USB drives — rare for direct SATA, but possible).

In that case the recovered `pool.json` is silently saved with a stale `by_id` and the next `braid unlock` fails to find the device.

The unmounted recovery path is unaffected: `mount::plan_open_pool` calls `probe_config_disk` (`cli/src/probe.rs:72`) which uses `Path::exists()` (which follows symlinks), so a stale `by_id` symlink fails fast with a clear "no unlockable disks found" error. This plan does not change that path.

The intended outcome: `braid recover` always produces a `pool.json` whose `by_id` paths reflect the live system, derived from the actual identity of each live pool device — not from any name- or label-based lookup that could match the wrong physical device. If a live pool member has no resolvable `/dev/disk/by-id/` symlink, recovery hard-fails with an explicit, actionable remediation message.

## Approach

For each device returned by `probe::probe_pool`, resolve its `by_id` by walking `/dev/disk/by-id/` and finding the symlink whose canonical target matches that device's live `underlying` kernel path. The journal snapshots are no longer consulted for `by_id` values during recovery; they remain in use only by the unmounted-recovery LUKS-open / mount step (`open_and_mount_pool`) and by the existing "live pool member is unknown to journal" sanity check.

### Why identity-based resolution instead of label-based discovery

`discover::discover_pool_members` matches `name` → `by_id` by scanning LUKS labels. That collapses duplicate `braid-<name>` labels by picking a deterministic winner via `by_id_priority` + filename, but the winner has no relationship to which device is *actually* in the live pool. A stale clone, test disk, or externally-relabelled device with the same `braid-disk1` label could resolve recovery's `disk1` to the wrong physical device — recreating the exact class of stale-`by_id` corruption this fix exists to remove.

`PoolDevice` (`cli/src/types.rs:96-101`) gives recovery the live device's authoritative identity: `underlying` (the kernel block-device path the LUKS mapper is currently backed by, sourced live from `cryptsetup status` in `cli/src/probe.rs:213`). Walking `/dev/disk/by-id/` and matching by canonical target is a direct identity check — no name collision is possible because we are asking the kernel "which by-id symlink points to this exact block device right now?"

`luks_uuid` is also available on `PoolDevice` and would be a valid alternative key (run `cryptsetup luksUUID` against each by-id entry, match against `pool_device.luks_uuid`). It is not chosen here because it costs N cryptsetup invocations per recovery for no additional safety: if two by-id symlinks have the same canonical kernel path, they are by definition the same physical device, and per-symlink LUKS UUID lookups would all return the same value.

### Why hard-fail on missed resolution

The only way the new resolver returns zero matches is if `/dev/disk/by-id/` is genuinely incomplete or absent for a device that is currently mounted in the pool — a broken/unusual state that operator intervention should investigate, not a state recovery should silently paper over with a guess. The error includes the concrete `underlying` kernel path so the operator can act on it.

### Why a recovery-local `ByIdResolver` instead of widening `Filesystem`

`probe::Filesystem` has 14 mock implementations across the tree (`cli/src/{luks,preflight,recover,enroll_key_file,mount,status,probe,lock,remove_missing,remove,add,unlock}.rs`) plus one production impl. Adding `canonicalize` to `Filesystem` would require updating every one of those just so a recovery-only code path can read symlinks. That is a lot of unrelated test churn for a method only `cmd_recover` will ever call.

Instead, define a narrow trait local to `cli/src/recover.rs` with exactly the operations the resolver needs:

```rust
pub trait ByIdResolver {
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error>;
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error>;
}

pub struct RealByIdResolver;

impl ByIdResolver for RealByIdResolver {
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error> {
        match std::fs::read_dir("/dev/disk/by-id") {
            Ok(entries) => entries
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())
    }
}
```

This is consistent with how `runner` and `fs` are already injected into `cmd_recover` — the new dependency joins them as a third constructor-time parameter. No existing trait or mock is touched.

### Algorithm in `cmd_recover`

After `probe::probe_pool` succeeds (`cli/src/recover.rs:179`) and inside the recovered-membership loop (`cli/src/recover.rs:182-206`):

1. Extract `name` from the mapper as today (`config::name_from_mapper(&dev.mapper.0)`).
2. **Sanity check (preserved):** if `name` is not in `union.disks`, return the existing `RecoverError::Failed("device {} is in the live pool but has no by-id path in either snapshot ...")`. Recovery refuses to handle pool members the journal never recorded.
3. **`by_id` resolution (new):** call `resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)`. On success, use the returned `ByIdPath`. On failure, propagate the error — it carries an actionable message including the underlying kernel path.
4. Insert into `recovered.disks` exactly as today, using the resolved `by_id`.

### `resolve_by_id_for_underlying` (new private helper in `recover.rs`)

```rust
fn resolve_by_id_for_underlying(
    resolver: &dyn ByIdResolver,
    underlying: &str,
) -> Result<ByIdPath, RecoverError> {
    let by_id_dir = "/dev/disk/by-id";

    // Canonical kernel path of the live pool device, used as the join key.
    let target = resolver.canonicalize(underlying).map_err(|e| {
        RecoverError::Failed(format!(
            "cannot canonicalize live pool device {underlying}: {e}"
        ))
    })?;

    let entries = resolver.list_by_id_entries().map_err(|e| {
        RecoverError::Failed(format!("cannot read {by_id_dir}: {e}"))
    })?;

    // (priority, filename, full_path) for every by-id entry that resolves to `target`.
    let mut matches: Vec<(u8, String, String)> = Vec::new();
    for name in entries {
        if discover::is_partition_entry(&name) {
            continue;
        }
        let full = format!("{by_id_dir}/{name}");
        // Skip dangling/broken symlinks silently — they cannot match anything.
        let Ok(resolved) = resolver.canonicalize(&full) else { continue };
        if resolved == target {
            matches.push((discover::by_id_priority(&name), name, full));
        }
    }

    if matches.is_empty() {
        return Err(RecoverError::Failed(format!(
            "live pool device '{underlying}' has no /dev/disk/by-id/ symlink \
             resolving to it. Recovery cannot persist a stable identifier for \
             this device.\n\
             To inspect the udev-created symlinks for this device, run:\n  \
             udevadm info --query=symlink --name {underlying}\n\
             If the output contains no `disk/by-id/...` entries, ensure udev \
             is running and the device's hardware identifiers are exposed by \
             the kernel, then re-run `braid recover`. If by-id entries exist \
             but none match this device's canonical path, file a braid bug \
             with the udevadm output."
        )));
    }

    // Stable highest-priority pick: lowest by_id_priority wins, ties broken by filename.
    matches.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Ok(ByIdPath(matches.into_iter().next().unwrap().2))
}
```

Multiple matches are not an error: every SATA drive normally has several by-id symlinks (`wwn-`, `ata-`, `scsi-`) all pointing to the same kernel device. We pick the most stable identifier using `discover::by_id_priority` (`cli/src/discover.rs:96-123`), exactly as `discover --write` does.

The remediation text concretely names the live device path that failed, hands the operator a single command (`udevadm info --query=symlink --name <underlying>`) that does not assume basename matching against by-id filenames, and tells them what a useful vs unhelpful output looks like.

### Reusing `is_partition_entry` and `by_id_priority`

These are private functions in `cli/src/discover.rs:96-133`. Promote both to `pub(crate)` so `recover::resolve_by_id_for_underlying` can call them. No semantic change, no new module surface — they stay in `discover.rs` because that file already documents the priority rationale and the partition-filter rationale.

### `cmd_recover` signature change

```rust
pub fn cmd_recover<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError>
```

`&dyn` (not generic `B`) keeps the generic surface narrow. The resolver is called at most ~N times per recovery (where N is the number of `/dev/disk/by-id/` entries, typically <50), so dynamic dispatch overhead is negligible.

### Out of scope

- Refreshing `union` *before* `open_and_mount_pool` so the unmounted path also picks up fresh `by_id`s. The unmounted path already fails loudly with a clear error and the user has a working escape hatch (`braid discover --write` then re-run `recover`). Expanding scope grows risk for a non-bug.
- Extending `probe::Filesystem` with `canonicalize`. The new resolver is a separate, recovery-local trait so the 14 unrelated `MockFs` impls do not need to be touched.
- Touching `discover.rs` beyond promoting two helpers to `pub(crate)`. The discover module is unchanged for its existing callers.
- Verifying `luks_uuid` against the resolved by-id symlink as a defense-in-depth check. The kernel-path identity match already proves physical identity; a UUID re-check would cost N cryptsetup invocations for zero added safety.
- VM tests. The fix is a parser/data-flow change inside one function and is fully covered by mocked unit tests; the existing VM recover suite already covers end-to-end behaviour.

## Files to modify

- **`cli/src/discover.rs`**
  - Change `is_partition_entry` (line 125) from `fn` to `pub(crate) fn`.
  - Change `by_id_priority` (line 106) from `fn` to `pub(crate) fn`.
  - No other changes — `discover_pool_members` is left alone.

- **`cli/src/recover.rs`**
  - Add `use crate::types::ByIdPath;` to the imports block (`recover.rs:1-9`) if not already present.
  - Define `pub trait ByIdResolver { ... }` and `pub struct RealByIdResolver;` near the top of the file (after `RecoverError`, before `RecoverParams`).
  - Add the private `resolve_by_id_for_underlying` helper described above just below the trait.
  - Add `by_id_resolver: &dyn ByIdResolver` as a third parameter to `cmd_recover` (after `fs`, before `params`).
  - Replace the `let by_id = union.disks.get(name)...` block at `recover.rs:189-201` with: (a) the existing union sanity check unchanged, then (b) `let by_id = resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?;`.
  - In the `tests` module (`recover.rs:321+`), add a `MockByIdResolver` struct with two `BTreeMap`-backed fields: `entries: Vec<String>` (returned by `list_by_id_entries`) and `canonicalize_results: BTreeMap<String, String>`. Provide a `Default` impl returning empty maps and a small builder for tests that need to populate them.
  - Update each of the 8 existing `cmd_recover(...)` test call sites (`recover.rs:563, 731, 842, 938, 1075, 1162, 1215, 1333`) to pass `&MockByIdResolver::default()` as the new third argument. Tests that need a real resolver result populate the mock first.
  - Add the new tests described below.

- **`cli/src/main.rs`**
  - At line 610 (the production `cmd_recover` call), pass `&braid_cli::recover::RealByIdResolver` as the new third argument.

- **`docs/decisions/017-runtime-disk-membership.md`**
  - Append one sentence to the Recovery section (around line 61) explaining the new invariant: `pool.json` `by_id` paths are resolved at recovery time by matching each live pool device's underlying kernel path against `/dev/disk/by-id/` symlinks, never copied from the journal. Note that recovery hard-fails if no symlink resolves.
  - Lines 47 and 61's existing wording about "rebuilds membership from the live btrfs pool topology — not from LUKS label scanning" is **still accurate** under the new design (membership still comes from the live pool; `by_id` resolution is path-based, not label-based) and does not need changes.

- **`docs/principles.md`**
  - No change. The principle at line 18 ("rebuilds membership from the live mounted pool (not LUKS label scanning)") remains accurate: membership is still sourced from the live pool, and `by_id` resolution is path-based, not label-based.

No other files need changes. `DiskMember::enriched(by_id, luks_uuid, devid)` (`cli/src/membership.rs:65`) is unchanged — only the source of `by_id` changes. None of the 14 `MockFs` impls listed earlier needs to change because `Filesystem` is not touched.

## Tests to add

All tests live in `cli/src/recover.rs` and mirror `recover_skips_mount_when_already_mounted` (`recover.rs:881-961`), which already sets up the "pool already mounted" code path that triggers the bug.

1. **`recover_uses_live_by_id_when_journal_is_stale`** — regression test for the fix. Journal `pre_membership`/`target_membership` contain a stale `by_id` (`/dev/disk/by-id/ata-OLD_PATH`) for `disk1`. Pool already mounted (`mountpoint_ok()`). The probed `PoolDevice.underlying` is `/dev/vdb`. `MockByIdResolver` is configured with `entries = ["ata-NEW_PATH", "wwn-0xDEADBEEF", "ata-OLD_PATH-part1"]` and `canonicalize_results = { "/dev/disk/by-id/ata-NEW_PATH" → "/dev/vdb", "/dev/disk/by-id/wwn-0xDEADBEEF" → "/dev/vdb", "/dev/disk/by-id/ata-OLD_PATH-part1" → "/dev/vdb1", "/dev/vdb" → "/dev/vdb" }`. Assert that the written `pool.json` contains `/dev/disk/by-id/wwn-0xDEADBEEF` (highest priority), not `ata-OLD_PATH` (the stale journal value), `ata-NEW_PATH` (lower priority than `wwn-`), or `ata-OLD_PATH-part1` (filtered as partition entry).

2. **`recover_hard_fails_when_underlying_has_no_by_id`** — regression test for the no-fallback contract. Same scaffold but `MockByIdResolver.entries` is empty (or contains only entries whose `canonicalize_results` resolve to a different kernel path). Assert that `cmd_recover` returns `Err(RecoverError::Failed(_))` whose message contains the substring `"has no /dev/disk/by-id/ symlink resolving to it"`, includes the concrete `underlying` path, mentions `udevadm info --query=symlink --name`, and that `pool.json` was not rewritten.

3. **`resolve_by_id_picks_highest_priority_when_multiple_match`** — direct unit test of `resolve_by_id_for_underlying`. Construct a `MockByIdResolver` with three entries (`wwn-X`, `ata-Y`, `scsi-Z`) all canonicalizing to the same target, plus a fourth (`ata-OTHER`) canonicalizing somewhere else. Call the helper directly and assert the result is the `wwn-X` path. This locks in the priority contract independently of the recovery scaffolding (per the project's "test the helper directly" feedback).

4. **`resolve_by_id_skips_partition_entries`** — direct unit test. Mock returns `["ata-FOO", "ata-FOO-part1", "ata-FOO-part2"]` all canonicalizing to the same target. Assert that the result is `/dev/disk/by-id/ata-FOO`, not a partition entry. This guards against future regressions in `is_partition_entry`'s visibility/behaviour as it crosses the recover/discover boundary.

5. **`recover_skips_mount_when_already_mounted`** (`recover.rs:881-961`) — existing test. Update its setup to construct a `MockByIdResolver` with realistic entries that resolve to each device's `underlying`, and pass it to `cmd_recover`. The test continues to assert that `pool.json` is written, and now also implicitly asserts that the written by_id paths come from the resolver, not the journal.

The mocks for `cmd::CryptsetupStatus` (already required by `probe_pool`) are unchanged because `PoolDevice.underlying` is already populated by the existing probe path.

## Verification

1. `just test-rust` — new and updated unit tests pass; existing recover tests pass after their `cmd_recover` call sites are updated to pass `&MockByIdResolver::default()`. No `Filesystem` mocks are touched, so no other test files require changes.
2. `cargo test -p braid recover::tests::recover_uses_live_by_id_when_journal_is_stale` — confirm the regression test fails on `master` (without the fix) and passes on the branch.
3. `cargo test -p braid recover::tests::recover_hard_fails_when_underlying_has_no_by_id` — confirms the no-fallback contract.
4. `cargo test -p braid recover::tests::resolve_by_id_picks_highest_priority_when_multiple_match recover::tests::resolve_by_id_skips_partition_entries` — confirms helper-level invariants.
5. `just test-vm recover` — full VM recovery test suite. Exercises `RealByIdResolver` against a real `/dev/disk/by-id/` and confirms the new code path runs end-to-end.
6. Manual sanity: `git grep 'union.disks.get' cli/src/recover.rs` should return zero matches inside the recovered-membership loop after the fix lands; only the union sanity-check call site (the "no by-id path in either snapshot" guard) should remain.
7. Manual doc sanity: re-read the updated `docs/decisions/017-runtime-disk-membership.md` recovery section to confirm the new sentence accurately describes the resolver's behaviour.
