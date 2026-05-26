# Fail-closed the remove-missing ENOSPC relocation-space preflight

## Context

`braid remove-missing`'s relocation-space preflight (`check_relocation_space`
in `cli/src/remove_missing.rs`) is a safety net against a catastrophic btrfs
failure: on a degraded, near-full pool, `btrfs device remove missing` starts
relocating block groups, hits ENOSPC mid-transaction, and forces the filesystem
read-only. This is proven by `tests/repro/btrfs-remove-enospc-crash.py` ("the
catastrophic failure mode... the filesystem is destroyed"). The execute path
writes `pending-op.json` (the recovery journal) *before* the btrfs remove, so a
mid-relocation crash also strands a journal.

Today, when the preflight itself *cannot run* -- `btrfs device usage --raw`
fails to spawn, returns a nonzero exit, or emits unparseable output -- the
helper returns `RelocationCheck::ProceedWithWarning` and `braid` runs the
remove anyway. `parse_btrfs_device_usage` maps any nonzero exit to
`ParseError::CommandFailed` (`cli/src/parse/btrfs_device_usage.rs:43-49`), so
even a genuine "not a btrfs filesystem" exit is swallowed into the soft-warn.
On a near-full pool that is the exact path into the read-only crash.

This contradicts the project's own rule (AGENTS.md, Mutation Safety
Heuristics): "Set fail-closed policy from the downstream failure mode. If a
branch can corrupt state or strand a journal when a preflight is wrong, every
uncertainty in that branch is a hard error even if a sibling branch can warn
and proceed."

**Outcome:** the preflight fails closed. When it cannot positively confirm
survivors have room, `braid remove-missing` refuses (in both dry-run and
real-run) with a `Validation` error and a remediation hint, mutating nothing
and stranding no journal. The genuine "ran and survivors lack space" rejection
already works (the `braid-remove-missing-enospc*` VM tests pass); this closes
only the "could not run" gap.

This deliberately reverses a currently-tested behavior (the soft-warn). The
sibling `cli/src/remove.rs::check_single_survivor` (the 2->1 eviction branch)
is already fail-closed on every input uncertainty and is the template to
mirror.

## The change

### 1. Helper: fail closed, collapse the enum

In `cli/src/remove_missing.rs`, change `check_relocation_space` to return
`Result<(), RemoveMissingError>`. Replace the two `Ok(ProceedWithWarning(...))`
arms with hard `RemoveMissingError::Validation` errors, using a three-way split
that mirrors `remove.rs::check_single_survivor` / `check_eviction_space`
message styles. Reuse the existing `RemoveMissingError::Validation` variant
(`remove_missing.rs:20-21`); do **not** resurrect the deliberately-removed
`Cmd` variant -- `Validation(format!("{e}"))` via the error's `Display` is
correct and sufficient (see `plans/impl/2026-05-18-remove-dead-cmd-error-variants.md`).

Messages (ASCII, `--` not em-dash). `{mount_point}` is the in-scope
`mount_point: &MountPoint`, which impls `Display` (`types.rs:410`), matching the
existing `format!("btrfs filesystem show {mount_point}")` convention in this
file (`remove_missing.rs:732`). These are the exact final format strings:

- **spawn error** (`runner.run` Err `e`):
  `ENOSPC pre-flight: btrfs device usage spawn failed: {e}. Refusing to remove the missing device without a validated relocation-space check. Inspect \`btrfs device usage --raw {mount_point}\` manually, then re-run.`
- **`ParseError::CommandFailed { exit_code, stderr, .. }`** (btrfs itself
  refused -- surface its words verbatim, **no** "ENOSPC pre-flight" prefix, so
  the operator does not think braid's check is at fault):
  `btrfs device usage failed (exit {exit_code}): {stderr}`
- **other `ParseError` `e`**:
  `ENOSPC pre-flight: btrfs device usage output unparseable: {e}. Refusing to remove the missing device without a validated relocation-space check. Inspect \`btrfs device usage --raw {mount_point}\` manually, then re-run.`

Do **not** append the existing "Free up space by deleting files..." tail
(`preflight.rs:566`) to these three -- that hint is only correct on the genuine
capacity-failure path, which stays unchanged.

With both error arms now returning `Err`, `RelocationCheck::ProceedWithWarning`
has zero producers (the helper is only reached when `pool.devices.len() >= 2`,
so the single-survivor "skip" never enters it). Delete the `RelocationCheck`
enum and its doc comment (`remove_missing.rs:503-514`).

### 2. Edge-case hardening: target devid absent from usage output

The target filter is `d.device_size == 0 && d.devid == missing_id`. If the
missing devid is **absent** from the parsed output, the target vec is empty,
`bytes_on_target == 0` for every type, and `check_raid1_relocation_space`
returns `Ok(())` -> unconditional Proceed regardless of survivor space. This is
the same "expected device missing from usage" uncertainty that
`remove.rs::check_single_survivor` already treats as fail-closed (pinned by
`check_eviction_space_2to1_fails_closed_when_survivor_missing`).

Crucially this is distinguishable from the benign case: a *present-but-zero*
missing device (genuinely nothing to relocate) still matches the filter, so its
target vec is **non-empty** with zero allocations -> correctly Proceeds. Only a
genuinely absent devid yields an empty target. Add, after building `target` and
before calling `check_raid1_relocation_space`:

```rust
if target.is_empty() {
    return Err(RemoveMissingError::Validation(format!(
        "ENOSPC pre-flight: missing devid {missing_id} is not listed in \
         `btrfs device usage --raw {mount_point}`, so its allocations cannot \
         be measured. Refusing to remove the missing device without a \
         validated relocation-space check. Inspect the command output \
         manually, then re-run."
    )));
}
```

Risk of rejecting a benign no-op removal is near-zero: `pool.missing_devids` /
`missing_count` (which gate reaching this code) and `btrfs device usage` both
read the same btrfs `fs_devices` list, so disagreement between them is exactly
the untrustworthy-input state that must fail closed.

### 3. Caller

Replace the 3-arm match (`remove_missing.rs:454-462`) with:

```rust
if pool.devices.len() >= 2 {
    if let Err(e) = check_relocation_space(runner, config.mount_point(), params.missing_id) {
        return Err(PlanFailure::with_notes(notes, e));
    }
}
```

The `return` inside the block keeps `notes` available for the
`RemoveMissingPlan { notes, work_plan }` move on the success path.

### 4. Doc comments

Rewrite `check_relocation_space`'s doc (`remove_missing.rs:516-528`): drop the
"the caller receives `ProceedWithWarning`" paragraph; state the fail-closed
policy and cite the degraded-pool read-only-crash failure mode
(`tests/repro/btrfs-remove-enospc-crash.py`). Explicitly note the divergence
from `remove.rs`'s `>= 2` soft-warn: this command always runs on a **degraded**
pool (the repro'd crash context), whereas `remove.rs` runs on a healthy pool --
so future readers do not "unify" the two policies. Rewrite the test rationale
at `remove_missing.rs:1065-1070` ("proceeds gracefully" -> "fails closed").

## Tests

### Rust (`cli/src/remove_missing.rs` `#[cfg(test)]`)

Mirror the `remove.rs` 2->1 fail-closed suite (`check_eviction_space_2to1_fails_closed_on_*`).

- Rename + invert `check_relocation_space_proceeds_on_command_error` (uses
  `CmdError::MissingMock` -> the spawn arm) to
  `check_relocation_space_fails_closed_on_command_error`: expect
  `Err(Validation)` whose message contains "spawn failed".
- Add `check_relocation_space_fails_closed_on_command_failed_exit`: inline
  runner returning `exit_status: 1` + stderr (model on the inline handler in
  `journal_survives_device_remove_failure`); assert message contains "exit 1"
  and the stderr text, and does **not** contain "ENOSPC pre-flight" (pins the
  deliberate verbatim-btrfs arm).
- Add `check_relocation_space_fails_closed_on_parse_error`: `exit_status: 0`
  with malformed-but-nonempty stdout (a device header with no `Device size`,
  per `device_usage_missing_required_field`); assert "unparseable".
- Add `check_relocation_space_fails_closed_on_target_absent_from_usage`: usage
  lists only survivors (`device_size > 0`), target devid absent; assert message
  contains "is not listed".
- Add `check_relocation_space_passes_present_zero_allocation_missing_target`
  (regression for the benign case the `target.is_empty()` guard must NOT
  reject): the missing target IS present in the usage output (`device_size == 0`,
  devid matches) but has zero Data/Metadata/System allocations, and survivors
  have no useful free space. Assert `Ok(())` -- a zero-allocation missing device
  is a safe no-op regardless of survivor capacity (target vec is non-empty, so
  the guard does not fire; `bytes_on_target == 0` makes `check_raid1_relocation_space`
  `continue` past every type). This pins the present-vs-absent distinction and
  guards against a weaker `bytes_on_target == 0` fail-closed implementation that
  would wrongly reject no-op removals.
- Rename + convert `plan_remove_missing_surfaces_soft_warn_on_command_error`
  and `..._on_parse_error` (note the parse one feeds `exit 1` = the
  `CommandFailed` arm) to `plan_remove_missing_fails_closed_on_*`: assert
  `plan_remove_missing(...)` returns `Err(PlanFailure { error: Validation, .. })`.
  Keep their `.dry_run(true)` -- it now proves dry-run also fails closed.
- Retarget (do **not** delete) the two warn-render tests
  `plan_preview_renders_warn_above_steps` and
  `remove_missing_warn_notes_render_canonical_bracketed_form`: change their
  fixture warn body from the (now-removed) ENOSPC soft-warn string to the
  read-only-probe warn body. `remove_missing` keeps a live `PreviewNote::Warn`
  producer independent of the ENOSPC path -- `plan_remove_missing` calls
  `require_mutation_preflight` and folds its notes in via `notes.extend(...)`
  (`remove_missing.rs:368-369`), and that helper emits
  `PreviewNote::Warn("read-only pre-flight failed: ...; proceeding anyway")`
  when the read-only probe degrades (`preflight.rs:613-617`). So the warn
  render/order contract (`[warn] <body>` above steps, no legacy `warning:`
  prefix) is still reachable for this command and must stay tested; only the
  ENOSPC soft-warn *body* disappears. Use the read-only-probe body as the new
  fixture so the tests exercise a genuinely-producible note.
- Keep `check_relocation_space_rejects_insufficient_space`,
  `_passes_sufficient_space`, `_with_missing_id_filters` unchanged.

### VM test: repurpose the soft-warn test into a fail-closed test

`tests/cli/braid-remove-missing-softwarn.py` currently asserts braid *proceeds*
when a PATH wrapper fails `btrfs device usage --raw`. Rename to
`braid-remove-missing-preflight-fails-closed` and invert the assertions; keep
the wrapper (it only intercepts `device usage --raw` and execs real btrfs
otherwise, so `braid status` discovery via `filesystem show` still works):

- **dry-run:** `braid remove-missing --missing-id N --dry-run` exits nonzero;
  the `Validation` message appears on stderr; pool still shows the missing
  device.
- **real-run:** `braid remove-missing --missing-id N --yes` exits nonzero; pool
  STILL shows the missing device (`"missing" in fi_show.lower()`); and
  `machine.fail("test -f /var/lib/braid/pending-op.json")` -- the canonical
  "no journal stranded" idiom (per `recover-remove-missing-completed.py:151`).

Preamble note (load-bearing): the refusal fires inside `plan_remove_missing`,
*before* `journal::write_journal` in `execute`, so no journal is ever written --
the absence is the proof of fail-closed, not incidental.

Files: rename `braid-remove-missing-softwarn.py` + `.nix`
(update the `name =` and `readFile` path inside the `.nix`, line ~24/~55) to
`braid-remove-missing-preflight-fails-closed.{py,nix}`; update the matching
attribute + `import ./tests/cli/...nix` path in `flake.nix` (lines ~605-609).

The genuinely-full-pool VM tests `braid-remove-missing-enospc.py` and
`braid-remove-missing-enospc-crash.py` already assert braid refuses on the
happy path and need **no** change.

## Docs

Extend the ENOSPC pre-flight bullet in `docs/commands/remove-missing.md`
(line ~74): "... and refuses if that pre-flight cannot run (the
`btrfs device usage` probe failed to spawn, returned a nonzero exit, produced
unparseable output, or did not list the targeted missing devid)."

No ADR enshrines the soft-warn, and AGENTS.md already mandates fail-closed, so
no architecture-doc change is needed.

## Non-goals

- **Do not change `remove.rs`'s `>= 2` soft-warn branch.** Its docstring
  documents the asymmetry deliberately: live (non-degraded) `btrfs device
  remove` with `>= 2` survivors "ENOSPCs cleanly without corrupting the
  filesystem," and "Do not unify the two error policies -- the asymmetry is the
  point." The repros prove only the *degraded* `remove missing` crash. The two
  commands have different downstream failure modes; fail-closing only
  `remove_missing` is the correct scope. (Note: `remove.rs`'s `>= 2` branch
  already hardens the `CommandFailed` sub-case that `remove_missing` swallows --
  this fix brings `remove_missing` ahead of, not just level with, its sibling,
  which is correct given its catastrophic failure mode.)
- No change to `parse_btrfs_device_usage` or `preflight.rs`.

## Files

- `cli/src/remove_missing.rs` -- helper, enum deletion, caller, doc comments,
  tests (primary change).
- `tests/cli/braid-remove-missing-softwarn.{py,nix}` -- rename + repurpose.
- `flake.nix` -- rename the check attribute + import path (~605-609).
- `docs/commands/remove-missing.md` -- extend the pre-flight bullet (~74).
- `cli/src/remove.rs`, `cli/src/preflight.rs` -- read-only references for
  message shape and the empty-target behavior; not modified.

## Verification

This change is localized to the `remove_missing` preflight (not systemd
lifecycle / pool lock / mount), so focused runs are appropriate.

1. `just test-rust` -- the inverted/new unit tests are the primary signal; the
   enum deletion will fail compilation if any `ProceedWithWarning` reference
   remains.
2. `just test-vm braid-remove-missing-preflight-fails-closed braid-remove-missing-enospc braid-remove-missing-enospc-crash`
   -- the repurposed fail-closed test plus the two happy-path-refusal
   regressions. (Add `braid-remove-softwarn` to confirm the untouched
   `remove.rs` warn-routing still passes.)
3. Hand sanity check of the new error wording over a normal terminal (ASCII,
   `--`, copy-pasteable `btrfs device usage --raw <mount>` hint).

Tell the user it is ready for their full-suite `just test-vm` run before merge;
do not autonomously run the unscoped suite.
