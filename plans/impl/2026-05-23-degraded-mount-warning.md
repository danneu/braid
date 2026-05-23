# Plan: post-degraded-mount warn line

## Context

After a successful `braid unlock --allow-degraded` (or any other path
that mounts the pool with `-o degraded`), braid prints only
`[ok] pool: mounted /mnt/storage` -- no inline indication that the mount
was degraded or that follow-up is required. The user has to read the
preceding `[skip] disk diskN: ...` probe events or rerun `braid status`
to learn that the pool is running without redundancy. The
B10-gotcha-sweep finding flagged this as a UX gap and proposed a `[warn]`
line plus a "one-shot mount" docs section in `troubleshooting.md`.

**Pivot from the original recommendation.** The "one-shot RW mount"
framing comes from an outdated btrfs wiki gotcha. Since kernel 4.14, the
chunk-level degraded check (`btrfs_check_rw_degradable` in
`reference/linux/fs/btrfs/volumes.c:7328-7383`) allows a 2-device RAID1
to be RW-mounted repeatedly with one missing device -- new writes
land as `single` profile on the present device, surviving RAID1 chunks
still tolerate one missing member, so a second remount typically still
succeeds. Reproducing the "one-shot" wording in user docs would mislead
operators. The *underlying* hazard -- accumulating single-profile chunks
during degraded operation -- is what's real, and it is already covered
by `braid doctor` (commit `1c94425` routes mixed-profile warnings to
`braid replace` first), `braid status` (`DEGRADED (N missing device)`),
and `docs/guides/recovery-scenarios.md:296-321` ("Degraded mount").

What's missing is the inline acknowledgement at mount time. This plan
adds that warn line, and folds in a narrow wording sweep so existing
operator-facing surfaces don't contradict it.

**Sweep scope.** Other operator-facing strings today claim "ZERO
redundancy" / "no redundancy for new writes" -- the refusal message,
both clap help blocks, and two user-facing docs. That phrasing is
accurate for a 2-disk RAID1 lost-one case but false for 3-disk RAID1
with two survivors (`tests/repro/degraded-writes-3disk.py:88-90` asserts
new chunks stay RAID1). If we change only the new warn line, operators
still meet the false framing on the way in (clap help, refusal message)
and in the recovery docs, and the test at `cli/src/mount.rs:1382` keeps
the old phrase pinned. Sweeping these together prevents the CLI
contract from contradicting itself.

## Change

### 1. Add the post-mount warn line

Emit one `[warn]` line from `scan_and_mount` in `cli/src/mount.rs`
immediately after the existing `[ok] pool: mounted {mount_point}` line,
gated on `plan.any_missing_member`. This is the single funnel for all
degraded mounts (interactive `unlock`, `recover`'s remount cycle,
auto-unlock service), per the seam audit.

### Wording

```
[warn] pool: mounted degraded with missing device(s) -- redundancy is reduced; next: braid replace
```

Rationale:

- Uses the `pool:` namespace + present-tense action established at
  `cli/src/recover.rs:3290-3294` ("pool: kernel dev_replace status check
  failed -- proceeding").
- "Redundancy is reduced" is topology-neutral. The earlier draft said
  "no redundancy for new writes", which is only accurate for the
  2-disk-RAID1-minus-one case; in a 3-disk RAID1 with two survivors,
  `tests/repro/degraded-writes-3disk.py:88-90` asserts new block groups
  stay RAID1 (no single-profile chunks), so "no redundancy for new
  writes" would be false there. "Reduced" holds for both topologies:
  the pool is short a mirror copy for the missing device's existing
  data, and any further disk loss is one step closer to data loss.
- Points at `braid replace` as the primary next step, matching
  `doctor`'s mixed-profile message
  (`cli/src/doctor.rs:684-686`: "replace missing device(s) first, then
  rebalance") and the `pool_missing_devices` doctor check
  (`cli/src/doctor.rs:714-723`).
- ASCII `--` per project CLI output style. No reference to "one-shot"
  or RO fallback.
- Singular/plural handling: keep `device(s)` rather than templating on
  count -- the operator already saw per-disk `[skip]` events in the
  probe phase, so the count is visible upstream.

### Implementation site

`cli/src/mount.rs` around line 851. The existing block is:

```rust
eprint!(
    "{}",
    status_line(
        StatusTag::Ok,
        color_enabled,
        &format!("pool: mounted {mount_point}")
    )
);

Ok(true)
```

Add directly after the `Ok` emission, before `Ok(true)`:

```rust
if plan.any_missing_member {
    eprint!(
        "{}",
        status_line(
            StatusTag::Warn,
            color_enabled,
            DEGRADED_MOUNT_WARNING,
        )
    );
}
```

Extract the literal as a private module-level `const
DEGRADED_MOUNT_WARNING: &str` at the top of `cli/src/mount.rs`. Keep it
private: no current cross-module caller needs it, and the unit test
below lives in `cli/src/mount.rs`'s test module so it can read the
private const directly. Widening to `pub(crate)` would also trip the
project's doc-comment requirement for `pub`/`pub(crate)` items
(AGENTS.md "Doc Comments") for no benefit.

### 2. Wording sweep across the same UX

Replace the pre-existing "ZERO redundancy" / "no redundancy for new
writes" phrasing with topology-neutral wording in the call sites below.
The new phrasing follows the same shape as the warn line: "redundancy
is reduced" + replace-first hint. The refusal message also drops the
`(single-profile chunks)` parenthetical, which is the specific
2-disk-RAID1 failure mode and is misleading on a 3-disk pool.

| Site | Old | New |
| --- | --- | --- |
| `cli/src/mount.rs:87` (refusal message) | `new writes would have ZERO redundancy (single-profile chunks)` | `new writes would land on a degraded pool with reduced redundancy` |
| `cli/src/mount.rs:1382` (test pin) | `msg.contains("new writes would have ZERO redundancy")` | `msg.contains("reduced redundancy")` |
| `cli/src/main.rs:221` (recover `--allow-degraded` help) | `Allow mounting with missing devices (degraded mode -- new writes have no redundancy)` | `Allow mounting with missing devices (degraded mode -- redundancy is reduced until you replace the missing device)` |
| `cli/src/main.rs:370` (unlock `--allow-degraded` help) | same as above | same as above |
| `docs/commands/recover.md:63` (recover flag row) | `Allow mounting with missing devices (new writes have no redundancy)` | `Allow mounting with missing devices (redundancy is reduced until you replace the missing device)` |
| `docs/guides/recovery-scenarios.md:189` (recover --allow-degraded prose) | `New writes will have no redundancy until the missing device is replaced.` | `Redundancy is reduced until the missing device is replaced.` |
| `docs/guides/recovery-scenarios.md:232` (Unlock with a missing disk) | `but new writes have no redundancy until you replace the dead drive` | `but the pool is running with reduced redundancy until you replace the dead drive` |
| `docs/guides/recovery-scenarios.md:298` (Degraded mount intro) | `but new writes have no redundancy on the missing device's share of data` | `but the pool is running with reduced redundancy on the missing device's share of data` |
| `docs/guides/recovery-scenarios.md:308` (Risks bullet) | `**No redundancy for new writes** -- data written while degraded exists on fewer disks. A second drive failure could lose data.` | `**Reduced redundancy** -- the pool is short the missing device's mirror copy of existing data, and on 2-disk pools new writes are allocated as single-profile chunks. A further drive failure could lose data.` |
| `docs/guides/auto-unlock.md:111` (Degraded mode prose) | `New writes in degraded mode have no redundancy until the drive is replaced and data rebalances.` | `Redundancy is reduced until the drive is replaced and data rebalances.` |

Notes:

- `docs/commands/status.md:48` ("DEGRADED" status definition) uses the
  same "no redundancy for new writes" phrase. Include it in the sweep
  with the same topology-neutral wording.
- The `docs/guides/recovery-scenarios.md:308` bullet keeps the single-
  profile detail behind "on 2-disk pools" so the precise hazard remains
  documented for the topology where it applies, without overgeneralising.
- The unit-test pin loosens to `reduced redundancy` so the assertion
  matches whichever exact sentence the refusal-message function emits;
  this preserves the test's intent (the refusal must still tell the
  user redundancy is in play) without coupling to one exact wording.

### Files touched

- `cli/src/mount.rs` -- one const + one conditional `eprint!` block in
  `scan_and_mount`; refusal-message string in `format_degraded_refused`;
  test pin in the existing refusal-message test.
- `cli/src/main.rs` -- two clap doc-comment lines (`UnlockArgs`,
  `RecoverArgs`).
- `docs/commands/recover.md` -- one flag-row cell.
- `docs/commands/status.md` -- one table cell.
- `docs/guides/recovery-scenarios.md` -- three prose sentences plus the
  Risks bullet (lines 189, 232, 298, 308).
- `docs/guides/auto-unlock.md` -- one "Degraded mode" prose sentence
  (line 111).

### Files NOT touched (and why)

- `cli/src/recover.rs` -- the `RemountCycle` execute path goes through
  `mount::execute_unlock_and_mount` -> `scan_and_mount`
  (`cli/src/recover.rs:3504-3527`), so the single edit covers it.
- `modules/braid/storage.nix` -- auto-unlock invokes `braid unlock`
  itself; the warn flows to the systemd journal automatically via
  stderr.
- `cli/src/mount.rs:413-421` (dry-run preview) -- already labels the
  step `mount -> ... (degraded)`. Dry-run output stays clean.
- `docs/guides/troubleshooting.md` -- the "Missing device after drive
  failure" section already documents the recovery options (replace vs
  remove-missing). The new warn line answers "I just mounted degraded
  -- now what?" inline; no doc edit needed.
- `docs/book/html/**` -- generated mdbook output, regenerated from the
  source `.md` files. Intentionally not touched.
- `research/**` and `plans/impl/**` -- historical scratch / planning
  artefacts. They preserve the original "ZERO redundancy" wording as a
  record of past intent; they are not operator-facing surfaces.
- `tests/repro/*.py`, `tests/module/no-silent-degraded.nix`,
  `research/*.md` -- test comments and historical research notes that
  use "zero redundancy" / "no redundancy" describing the underlying
  btrfs behaviour for 2-disk RAID1. These are accurate in their own
  context (the 2-disk repro) and are not user-facing CLI text.

## Verification

### Unit test (new)

In `cli/src/mount.rs` test module, add a content test:

```rust
#[test]
fn degraded_mount_warning_mentions_reduced_redundancy_and_replace() {
    assert!(DEGRADED_MOUNT_WARNING.contains("redundancy is reduced"));
    assert!(DEGRADED_MOUNT_WARNING.contains("braid replace"));
}
```

Intent: pin the user-facing wording's load-bearing tokens so a stray
edit can't silently drop "redundancy is reduced" or the action hint.
The existing `mount_degraded_with_flag` test already exercises the warn
emission path via `open_and_mount_for_test`; it does not need to assert
on stderr (matches the project's existing post-mount-message coverage
style -- the `[ok]` line is also not stderr-asserted).

### Refusal-message test update

Update the existing assertion at `cli/src/mount.rs:1382` to anchor on
`reduced redundancy` instead of `new writes would have ZERO redundancy`
(per the sweep table above). The test's intent -- "the refusal message
must still tell the user redundancy is in play" -- is preserved; only
the exact phrase loosens.

### VM test extension

In `tests/cli/braid-unlock.py`, extend Test 4b around line 445-462 --
the existing `--allow-degraded` happy-path subtest. Today it calls

```python
machine.succeed(unlock_cmd(passphrase, extra="--allow-degraded"))
```

Switch to the stderr-capture pattern already used by Test 1's probe
assertion at `tests/cli/braid-unlock.py:99-108`:

```python
machine.succeed(
    f"{unlock_cmd(passphrase, extra='--allow-degraded')}"
    " >/tmp/unlock-degraded-stdout 2>/tmp/unlock-degraded-stderr"
)
err = machine.succeed("cat /tmp/unlock-degraded-stderr")
assert "redundancy is reduced" in err, (
    f"expected post-mount degraded warning on stderr; got: {err!r}"
)
```

This is the right test because:

- It actually exercises `braid unlock --allow-degraded` end-to-end
  through the CLI -- the path the warn lives on.
- `tests/repro/degraded-soft-balance.py:51-54` bypasses braid entirely
  (`cryptsetup luksClose` + `mount -o degraded` direct), so it cannot
  observe braid stderr.
- `tests/repro/` is excluded from the default `just test-vm` run (see
  AGENTS.md: `just test-vm` "Run NixOS VM tests (excludes repro
  tests)"). `tests/cli/braid-unlock.py` is in the regular VM lane.

Per AGENTS.md test-scope guidance, run only the touched test plus the
Rust suite for this change:

```
just test-rust
just test-vm braid-unlock
```

### Manual smoke (optional)

In the VM dev loop:

```
sudo braid unlock --allow-degraded
```

Expected stderr tail:

```
[ok]   pool: mounted /mnt/storage
[warn] pool: mounted degraded with missing device(s) -- redundancy is reduced; next: braid replace
```

## Out of scope

- "One-shot mount" docs section -- inaccurate for modern kernels;
  intentionally not added.
- Doctor / status / TUI changes -- already cover the underlying
  single-profile-chunk risk.
- Reformatting the existing `[ok]` line to include `(degraded)` -- the
  separate `[warn]` is clearer than a parenthetical and matches the
  project's bracketed status-tag convention.
