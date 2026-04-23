# Plan: collapse duplicated Live/Missing dispatch in replace.rs

## Context

`cli/src/replace.rs` carries two near-identical Live/Missing arms in both the
execution path (`cmd_replace` body, lines 293-364) and step compilation
(`compile_replace_steps`, lines 585-655). In each arm pair, the shared
sequence is `pool_replace_device` + `pool_resize_device` (execution) and
`BtrfsReplaceStart` + `BtrfsFilesystemResize` (compile). The only real
differences are Live-only surface: an I/O-stats pre-warning, a `cryptsetup
close` of the old mapper, and wording in the kickoff `eprintln!`. The
Missing-only surface is the soft-balance follow-up (already hoisted out of
the execution match into the `maybe_restore_raid1` call at 370-378, but still
inlined inside the Missing arm of `compile_replace_steps` at 643-653).

Two arms that share ~80% of their body make the real differences hard to
see and invite drift (a future tweak to the shared calls can easily be
forgotten in the second arm). This change collapses the shared spine so the
remaining per-variant code is exactly the variant-specific behavior.

No behavior change -- identical commands in identical order, identical
user-facing wording. The soft-balance follow-up on the missing path is part
of the `replace` contract per
[docs/principles.md](../../docs/principles.md:17), not an accident to
optimize away.

## Approach

### 1. Refactor execution path (cli/src/replace.rs:293-364)

Target shape -- bind `devid` locally via a single match (no accessor method
added to `ReplaceSource`):

```rust
// Live-only: source-device I/O stats warning (informational).
if let ReplaceSource::Live { mapper, devid } = &replace_source {
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: config.mount_point().clone(),
    });
    if let Ok(ref raw) = stats_raw
        && let Ok(stats) = parse_btrfs_device_stats(raw) {
            let expected_path = format!("/dev/mapper/{}", mapper.0);
            let has_errs = stats.devices.iter().any(|d| {
                d.target.as_path() == Some(expected_path.as_str())
                    && (d.read_io_errs > 0
                        || d.write_io_errs > 0
                        || d.flush_io_errs > 0
                        || d.corruption_errs > 0
                        || d.generation_errs > 0)
            });
            if has_errs {
                eprintln!(
                    "Warning: source device (devid {devid}) has I/O errors. \
                     btrfs replace will read from mirrors where possible, \
                     but may fail if any data lacks a healthy mirror copy."
                );
            }
        }
}

// Kickoff wording differs between Live and Missing -- keep it per-variant
// but inline the match so the shared spine below is unconditional. Bind
// devid here too.
let devid = match &replace_source {
    ReplaceSource::Live { devid, .. } => {
        eprintln!("Replacing device (devid {devid}) with {}...", new_mn);
        *devid
    }
    ReplaceSource::Missing { devid } => {
        eprintln!("Rebuilding missing device (devid {devid}) onto {}...", new_mn);
        *devid
    }
};

// Shared spine.
pool_replace_device(
    runner,
    devid,
    &new_mapper_path,
    config.mount_point(),
    params.progress,
)?;
eprintln!("Replace complete.");
pool_resize_device(runner, devid, config.mount_point())?;

// Live-only tail: best-effort close of old mapper.
if let ReplaceSource::Live { mapper, .. } = &replace_source {
    let close_result = runner.run(&CmdRequest::CryptsetupClose {
        mapper: mapper.0.clone(),
    });
    match close_result {
        Ok(r) if r.exit_status != 0 => {
            eprintln!(
                "Warning: failed to close LUKS mapper {} (exit {})",
                mapper, r.exit_status
            );
        }
        Err(e) => eprintln!("Warning: failed to close LUKS mapper {}: {}", mapper, e),
        _ => {}
    }
    eprintln!("Old device closed. If repurposing the physical disk, wipe it separately.");
}
```

Everything after line 364 (`maybe_restore_raid1` gated on `Missing`, post-
commit membership persist, journal clear) is unchanged.

Result: the `pool_replace_device` and `pool_resize_device` calls -- the
irreversible operations -- appear exactly once instead of twice, making it
impossible to let the arms drift.

### 2. Refactor `compile_replace_steps` (cli/src/replace.rs:585-655)

Target shape -- again, bind `devid` locally:

```rust
let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);
let devid = match input.replace_source {
    ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
};

// Shared spine: btrfs replace start + filesystem resize.
steps.push(Step {
    risk: "long",
    description: format!(
        "btrfs replace start {} /dev/mapper/{} {}",
        devid, new_mn, input.mount_point
    ),
    commands: vec![CmdRequest::BtrfsReplaceStart {
        devid,
        target_device: new_mapper_path,
        mount_point: input.mount_point.clone(),
    }],
});
steps.push(Step {
    risk: "safe",
    description: format!(
        "btrfs filesystem resize {}:max {}",
        devid, input.mount_point
    ),
    commands: vec![CmdRequest::BtrfsFilesystemResize {
        devid,
        mount_point: input.mount_point.clone(),
    }],
});

// Variant-specific tails.
match input.replace_source {
    ReplaceSource::Live { mapper, .. } => {
        steps.push(Step {
            risk: "safe",
            description: format!("cryptsetup close {}", mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: mapper.0.clone(),
            }],
        });
    }
    ReplaceSource::Missing { .. } => {
        if input.will_clear_last_missing && input.total_devices >= 2 {
            steps.push(Step {
                risk: "long",
                description:
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                        .into(),
                commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: input.mount_point.clone(),
                }],
            });
        }
    }
}
```

`BtrfsReplaceStart` + `BtrfsFilesystemResize` appear exactly once. The
remaining match carries only what actually differs: Live closes the old
mapper, Missing optionally appends a soft balance.

## New tests required

Unit tests and VM tests that pass today do **not** pin the two behaviors
this refactor actually risks: ordering in the missing-path dry-run render,
and the post-replace soft balance on the missing path. Both gaps must close
before the refactor ships.

### 2a. New unit test: missing-path dry-run render ordering

Existing test at `cli/src/replace.rs:1268`
(`dry_run_missing_path_shows_btrfs_replace`) only asserts presence/absence
of descriptions, not their order -- a regression that rendered the soft
balance before `btrfs replace start` would still pass. Mirror the live-path
render-order test at `cli/src/replace.rs:1998`
(`dry_run_render_fresh_disk_live_replace_with_keyfile`):

Add `dry_run_render_missing_path_ordering` in the same test module:

- `ReplaceSource::Missing { devid: 2 }`, `will_clear_last_missing: true`,
  `total_devices: 2`, `new_probed` as `PresentNotLuks`, no keyfile.
- Call `Step::render_dry_run(&steps)` and assert, in this order:
  1. LUKS format, header backup, LUKS open (lines for the new disk init)
  2. `btrfs replace start` (risk `[long       ]`)
  3. `btrfs filesystem resize`
  4. `-dconvert=raid1,soft` (the soft balance)
- Assert no line contains `cryptsetup close` (missing path has no old
  mapper).
- Find each expected substring with `.iter().position(...)` and assert the
  returned indices are strictly increasing, so the test fails if order
  breaks even if all substrings are still present.

### 2b. New cmd_replace Rust test: missing-arm soft-balance wiring

**Update during implementation**: an end-to-end VM test of the soft-balance
invariant turned out to be infeasible. The only way to create the
single-profile chunks the soft balance is meant to clean up is to write
while degraded, and that same state prevents `btrfs replace start` from
running (kernel returns ENOSPC from `inc_block_group_ro` during replace
staging; see
[reference/linux/fs/btrfs/block-group.c:1366](../../reference/linux/fs/btrfs/block-group.c:1366)).
This is why
[tests/repro/degraded-soft-balance.py](../../tests/repro/degraded-soft-balance.py)
uses `btrfs device add` + `device remove missing` + balance -- not
`btrfs replace start`. braid's missing arm takes the replace-start path,
so the repro's scenario cannot be recreated on top of braid replace.

Instead, add a wiring test at the `cmd_replace` layer that drives the full
missing path with a recorded-call runner and asserts the command sequence:

`cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize` in
the `#[cfg(test)] mod tests` block:

- Uses a stateful `MissingPathSuccessRunner` that reports degraded btrfs
  state (disk1 live, devid 2 missing) until `BtrfsReplaceStart` is issued,
  then flips to a healthy 2-device layout (disk1 + disk3) so the second
  `probe_pool` inside `maybe_restore_raid1` sees `missing_count == 0` with
  `devices.len() >= 2` -- the minimal condition set for the soft balance
  to fire.
- Seeds pool.json with disk1 + disk2 (disk2 has `devid: Some(2)` so
  `build_replacement_membership` matches `--missing-id 2`).
- Uses `PresentLuks { mapper_open: true }` for disk3 to skip LUKS
  format/open/enroll and focus the test on the shared replace spine plus
  missing-path tail.
- Asserts `cmd_replace` returns `Ok(())`, `pending-op.json` is cleared,
  and the sleep inhibitor is acquired exactly once.
- Asserts the recorded command log has
  `BtrfsReplaceStart -> BtrfsFilesystemResize -> BtrfsBalanceRaid1Soft`
  in strict order.
- Asserts `CryptsetupClose` is never issued on the missing path (no old
  mapper to close).
- Verified by mutation test: setting the `if matches!(..., Missing { .. })`
  guard to `if false` causes the new test to fail at the
  `BtrfsBalanceRaid1Soft` `.expect(...)` assertion, confirming it catches
  the regression class the plan review flagged.

No VM test files added; no flake.nix change.

## Critical files

- `cli/src/replace.rs` -- only source file modified. Changes:
  - execute body rewritten per step 1 (local `devid` bind, no
    `ReplaceSource::devid()` accessor)
  - `compile_replace_steps` body rewritten per step 2 (same pattern)
  - new unit test `dry_run_render_missing_path_ordering` pinning the
    missing-path render order
  - new integration test
    `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`
    with a `MissingPathSuccessRunner` pinning the
    `BtrfsReplaceStart -> BtrfsFilesystemResize -> BtrfsBalanceRaid1Soft`
    sequence and the absence of `CryptsetupClose` on the missing path

## Non-goals / scope bounds

- Do not add an accessor method on `ReplaceSource`. Bind `devid` locally in
  each function. The value is collapsing the shared spine, not growing
  `ReplaceSource`'s API surface.
- Do not touch `resolve_replace_source` (426-503), `format_replace_confirm`
  (lines 680+), `build_replacement_membership`, or `maybe_restore_raid1`.
- Do not change user-facing wording. Preserve the "Replacing device ..." vs
  "Rebuilding missing device ..." distinction verbatim -- operators key off
  these strings and the Missing path is semantically a rebuild, not a
  replace-in-place.
- Do not reorder the post-match `maybe_restore_raid1` call or any journal/
  membership code.
- Do not extend `replace-dead-disk.py` with the degraded-write assertions.
  Keep the soft-balance invariant in its own focused test so regressions
  read off one failing test, not a mixed-concern one.

## Verification

1. `just test-rust` -- validates `compile_replace_steps` for both paths,
   including:
   - the **new** `dry_run_render_missing_path_ordering`, which pins
     `btrfs replace start -> filesystem resize -> optional soft balance`
     and the absence of `cryptsetup close` on the missing path
   - the **new**
     `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`,
     which drives the full `cmd_replace` flow with a recorded-call runner
     and asserts the
     `BtrfsReplaceStart -> BtrfsFilesystemResize -> BtrfsBalanceRaid1Soft`
     sequence
   - the existing `dry_run_render_fresh_disk_live_replace_with_keyfile`
     and `close_runs_before_resize_on_live_replace` for the Live path

2. `just test-vm replace-2disk-pool replace-dead-disk
   replace-new-already-luks replace-larger-disk replace-sequential
   replace-preserves-devid` -- unchanged; covers the Live and missing
   execution paths end-to-end.

3. `cargo clippy -p braid-cli --all-targets -- -D warnings` -- catches
   stray imports and any callsite the rewrite missed.

## Risks

- Low for the refactor itself. The shared spine runs the same calls in the
  same order; per-variant tails are unchanged.
- The two new tests guard the two axes that existing coverage does not
  pin: dry-run step ordering on the missing path, and the post-replace
  soft-balance invariant. Without them, "no behavior change" is a claim
  the suite cannot validate.
- Watch for: in the compile path, `new_mapper_path: String` is consumed
  into `CmdRequest::BtrfsReplaceStart { target_device }`. That is fine
  because it is used once. In the execute path, `&new_mapper_path` is
  passed by reference, so a single binding works. Both ownership shapes
  compile as sketched.
