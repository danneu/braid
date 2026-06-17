# Pivot: classify `MapperBackingResolveError` honestly in the TUI unpooled-disk gate

## Context

A review finding (Low / Simplicity) flagged that in the TUI's unpooled-disk
classifier, `ProbeError::MapperBackingResolveError` is grouped into the generic
`continue` catch-all arm, while its sibling mapper errors
(`MapperBackingMismatch` / `MapperConflict`) get a dedicated
`UnpooledDiskRender::MapperHijacked` cell. The finding proposed *moving*
`MapperBackingResolveError` into the `MapperHijacked` arm "since the operator
recovery is identical."

Verification established:

- **The finding's reachability point is correct.** `MapperBackingResolveError`
  *is* genuinely reachable from `probe_config_disk` (via `probe_mapper_open` ->
  `classify_mapper_ownership`, `cli/src/luks.rs#classify_mapper_ownership`),
  unlike `PoolDevice` / `NotBtrfs` / `MountInfo`, which the catch-all comment
  correctly names as unreachable. So the comment's framing -- "future variants
  must be classified here rather than silently swallowed" -- is partly violated:
  a *live* variant sits silently among named-dead ones.
- **The finding's proposed fix is wrong.** The recovery is *not* identical.
  `MapperConflict`/`MapperBackingMismatch` Display say "close the conflicting
  mapper ... and re-run"; `MapperBackingResolveError` Display says "Check that
  the configured disk is plugged in and that udev has populated
  /dev/disk/by-id/" (`cli/src/probe.rs`, the `#[error(...)]` attributes on
  `ProbeError`). Folding it into the red `"mapper conflict"` cell
  (`cli/src/tui/view/mod.rs`, `UnpooledDiskRender::MapperHijacked`) would assert
  an ownership conflict that was never established -- canonicalization failed
  *before* any expected-vs-found comparison -- and would mislead the operator
  into closing a mapper when the real issue is disk presence / a udev race.
- **`status.rs` is the positive precedent.** `config_probe_advisory`
  (`cli/src/status.rs`) deliberately passes all of `MapperConflict` /
  `MapperBackingMismatch` / `MapperBackingResolveError` through `e.to_string()`
  *verbatim* precisely so each keeps its own distinct remediation. Collapsing
  them in the TUI would diverge from that surface, not align with it.
- **The conservative "missing" fallback is the honest result** for a genuinely
  indeterminate state. Mechanism (important -- the catch-all does NOT render
  `Missing`): the residual `continue` inserts no `unpooled_disks` entry, so
  `cli/src/tui/view/mod.rs#unpooled_disk_status_cell` returns `None` and the
  caller's `unwrap_or_else` renders the default yellow `"missing"` cell -- its
  documented fallback "for disks the unpooled probe couldn't classify (e.g.
  probe errors)." This is the same defensive *spirit* as the explicit
  "treat as Missing rather than lying about state" and "conservative
  LuksHeaderUnreadable" renders in `probe_pool_for_tui`, reached by a different
  path. The finding's own Impact paragraph concedes the fallback is honest.

**Outcome:** keep the behavior (route `MapperBackingResolveError` to the
generic-missing fallback), but make the *intent* structural and unmistakable,
and pin it with a regression test. No behavior change; this dissolves the
finding's valid core (a misleading comment masking a live variant) without
the incorrect render change it proposed.

Scope is confined to `cli/src/tui/probe.rs`. `monitor.rs` (fails closed on all
indeterminate variants; mapper variants are unreachable from
`probe_pool_alerts`) and `status.rs` (already correct, see above) need no change.

## Change 1 -- split the residual arm into "reachable-indeterminate" vs "unreachable-listed"

File: `cli/src/tui/probe.rs`, inside the
`match probe_config_disk(runner, fs, &parsed_name, &by_id, backing_path_resolver)`
block (the unpooled-disk loop, currently ~lines 397-409).

Replace the single catch-all arm:

```rust
                // Exhaustive residual arm: future ProbeError variants must be
                // classified here rather than silently swallowed. PoolDevice,
                // NotBtrfs, and MountInfo are unreachable from
                // probe_config_disk today, but listing them keeps this gate in
                // lockstep with the other diagnostic surfaces.
                Err(
                    ProbeError::Cmd(_)
                    | ProbeError::Parse(_)
                    | ProbeError::PoolDevice { .. }
                    | ProbeError::NotBtrfs { .. }
                    | ProbeError::MapperBackingResolveError { .. }
                    | ProbeError::MountInfo(_),
                ) => continue,
```

with two arms that separate the two genuinely different categories the original
conflated (final wording to be refined during implementation; intent fixed):

```rust
                // Reachable from probe_config_disk, but the disk's true state is
                // genuinely indeterminate. Leave it unclassified -- do NOT
                // insert an unpooled_disks entry. unpooled_disk_status_cell then
                // returns None for this disk and the caller's unwrap_or_else
                // renders the default yellow "missing" cell (its documented
                // fallback "for disks the unpooled probe couldn't classify").
                // Cmd/Parse are environmental (spawn failure, output drift).
                // MapperBackingResolveError fires when braid-<name> is open but
                // a backing path won't canonicalize, so NO ownership conflict
                // was ever established: it must not be classified as
                // MapperHijacked (the red "mapper conflict" cell asserts a
                // conflict we never confirmed), and its recovery differs --
                // replug the disk / let udev settle, not close the mapper
                // (status's config_probe_advisory keeps the same distinction).
                Err(
                    ProbeError::Cmd(_)
                    | ProbeError::Parse(_)
                    | ProbeError::MapperBackingResolveError { .. },
                ) => continue,
                // Unreachable from probe_config_disk today -- these arise only
                // on the pool-probing paths -- but enumerated so a newly-wired
                // variant cannot reach this gate without a compile error forcing
                // a classification decision here.
                Err(
                    ProbeError::PoolDevice { .. }
                    | ProbeError::NotBtrfs { .. }
                    | ProbeError::MountInfo(_),
                ) => continue,
```

Both arms `continue` -> identical behavior to today; the split + comments encode
the reachable/indeterminate vs unreachable/listed distinction in the code
structure, and the full enumeration preserves the exhaustiveness guard (adding a
new `ProbeError` variant still fails to compile until classified here).

Comments are exempt from the enforced CLI-output ASCII check, but keep them ASCII
anyway (`--`, plain quotes) per house style.

## Change 2 -- regression test pinning the resolve-error missing fallback

File: `cli/src/tui/probe.rs`, in `#[cfg(test)] mod tests`, immediately after the
two existing `MapperHijacked` tests (`unpooled_disk_mapper_backing_mismatch_classified_correctly`
~line 2634 and `unpooled_disk_mapper_conflict_null_backing_classified_correctly`
~line 2711). There is currently a TUI-layer coverage gap: no test pins what the
TUI does with `MapperBackingResolveError` (only the `probe_config_disk`-layer
`probe_config_disk_mapper_backing_resolve_error_is_distinct` in
`cli/src/probe.rs` exists).

Mirror the mismatch test exactly, changing only the resolver (make it error on
the configured by-id path) and the assertion. `MockBackingPathResolver` is
re-exported at `crate::test_fixtures::MockBackingPathResolver`
(`cli/src/test_fixtures.rs`), so no new `use` is needed -- match the file's
existing fully-qualified `crate::test_fixtures::...` style. The `default()`
resolver returns identity for every unseeded path, exactly as the virtio
resolver does for the non-virtio `braid-*` / `/dev/vd*` paths these tests use,
so the live-pool (toshiba) probe behaves identically.

```rust
    // Intent: probe_pool_for_tui leaves a declared disk whose open mapper's
    // backing path fails to canonicalize OUT of unpooled_disks (no entry), so
    // the view layer falls back to its default "missing" cell -- it is NOT
    // classified as MapperHijacked.
    //
    // Why it exists: ProbeError::MapperBackingResolveError is reachable from
    // probe_config_disk but is deliberately routed to the catch-all `continue`
    // (no unpooled_disks entry -> unpooled_disk_status_cell None-fallback):
    // canonicalization fails before any ownership conflict can be established,
    // and its recovery (replug / let udev settle) differs from a hijack's (close
    // the mapper). This pins that decision so a refactor cannot silently fold it
    // into the red "mapper conflict" cell, and matches status's
    // config_probe_advisory, which keeps the resolve-error remediation distinct.
    //
    // Scenario: 1-disk live pool. Second declared disk exists and is LUKS2 and
    // braid-ironwolf is active, but the configured by-id path cannot be
    // canonicalized (udev has not populated /dev/disk/by-id, or the disk was
    // unplugged mid-probe).
    #[test]
    fn unpooled_disk_mapper_backing_resolve_error_uses_missing_fallback() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "22222222-2222-2222-2222-222222222222\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksDump",
                    "LUKS header information\nVersion:       \t2\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-ironwolf".into()),
                },
                ok_raw(
                    "cryptsetup status braid-ironwolf",
                    "/dev/mapper/braid-ironwolf is active and is in use.\n\
                     \ttype:    LUKS2\n\
                     \tdevice:  /dev/vdz\n",
                ),
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            (
                "toshiba".to_owned(),
                "/dev/disk/by-id/braid-toshiba".to_owned(),
            ),
            (
                "ironwolf".to_owned(),
                "/dev/disk/by-id/braid-ironwolf".to_owned(),
            ),
        ]);

        // Open mapper, but the configured by-id path will not canonicalize.
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_error(
                "/dev/disk/by-id/braid-ironwolf",
                std::io::ErrorKind::NotFound,
            );

        let pool = expect_pool(
            probe_pool_for_tui(
                &runner,
                &fs,
                &MountPoint::new("/mnt/storage".into()),
                &tui_disks_with_by_id(disk_by_id),
                &test_paths().1,
                &resolver,
            )
            .unwrap(),
        );

        // The catch-all `continue` inserts no entry; the disk is left to the
        // view's None-fallback (yellow "missing"). A Some(MapperHijacked) here
        // would mean the resolve error was wrongly classified as a conflict.
        assert!(
            pool.unpooled_disks.get("ironwolf").is_none(),
            "resolve error must stay unclassified (view-layer missing fallback), \
             not be rendered as a mapper conflict"
        );
    }
```

Why this triggers the variant: `classify_mapper_ownership`
(`cli/src/luks.rs`) canonicalizes the *expected* by-id path first; the seeded
`NotFound` error on `/dev/disk/by-id/braid-ironwolf` raises
`OwnershipError::BackingPathResolveError` -> `ProbeError::MapperBackingResolveError`,
which the (now first) catch-all arm handles with a bare `continue` -- it inserts
no `unpooled_disks` entry, so `get("ironwolf")` is `None` and the caller of
`unpooled_disk_status_cell` (`cli/src/tui/view/mod.rs`) renders its default
yellow `"missing"` cell via `unwrap_or_else`. The active non-null
`device: /dev/vdz` status is required so the classifier proceeds past the status
match to the canonicalize step. Why `is_none()` is the correct contract: an
explicit `Some(UnpooledDiskRender::Missing)` is only produced by
`ConfigDiskState::Absent` (a different path); folding the resolve error into the
`MapperHijacked` arm -- the rejected fix -- would make this `Some(MapperHijacked)`
and fail the assertion. (Precedent for the `is_none()` idiom:
`unpooled_disk_status_cell_renders_each_variant` at `cli/src/tui/view/mod.rs`.)

## Considered and rejected

- **Fold into `MapperHijacked` (the finding's proposal).** Asserts an
  unconfirmed conflict, mismatches the actual recovery, and diverges from
  `status.rs`'s deliberate per-variant remediation. Rejected on correctness.
- **A dedicated `UnpooledDiskRender` variant** (e.g. "backing unresolved").
  Over-engineering for a rare, transient, environmental state; the finding
  itself notes no dedicated render state is warranted and the generic-missing
  fallback is honest. Rejected on simplicity.
- **Minimal single-comment rewrite (no arm split).** Acceptable, but leaves the
  reachable/unreachable distinction in prose only; the two-arm split makes it
  structural and is the "ideal" form requested.

## Verification

- `just test-rust` (or `cargo test -p braid-cli tui::probe`) -- the new test
  passes and the two existing `MapperHijacked` tests still pass (proving the
  split did not alter their routing).
- Targeted: `cargo test -p braid-cli unpooled_disk_mapper` runs all three
  unpooled-mapper render tests together.
- `cargo build` / `cargo clippy` -- confirms the `match` is still exhaustive
  (the two new arms cover exactly the same variant set as the old single arm).
- No fixture refresh, no VM test, no docs change: behavior and CLI output are
  unchanged; this is a comment-structure + test-coverage change.

## Implementation notes

- The plan's literal assertion `pool.unpooled_disks.get("ironwolf").is_none()`
  trips clippy's `unnecessary_get_then_check` (a fresh warning). Switched to the
  semantically identical `!pool.unpooled_disks.contains_key("ironwolf")` to keep
  the lane warning-clean. Same contract (no entry for the disk -> view-layer
  missing fallback); the cited `is_none()` precedent in
  `cli/src/tui/view/mod.rs` is an `Option`-returning call, not a `HashMap::get`,
  so clippy does not flag it there.
