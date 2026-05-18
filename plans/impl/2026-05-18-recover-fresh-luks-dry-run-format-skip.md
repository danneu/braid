# Plan: honest dry-run conditional for FreshLuks add-recovery format step

## Context

`braid recover --dry-run` for an interrupted `add` (PoolMutation phase)
with `FreshLuks` targets always renders `$ cryptsetup luksFormat ...` for
each not-yet-live target, tagged `[destructive]`, with a single
conditional suffix:

> `(skipped at runtime if open/scan reconciliation makes target live before replay)`

But the executor's FreshLuks replay arm (`cli/src/recover.rs:2585-2620`)
also skips the format unconditionally when its per-target probe sees
`ConfigDiskState::PresentLuks` with the journaled UUID and the expected
`braid-<name>` label -- i.e. the original `cryptsetup luksFormat`
already completed pre-crash but the disk never made it into the live
btrfs pool. That structural branch is invisible in the dry-run preview;
the existing conditional suffix only covers the whole-step skip
("target became live before replay"), not the per-command format skip.

The previous fix `c9da2c9 fix(recover): mirror live add replay skips in
dry-run` closed the analogous gap for "target is already a live pool
member" by plumbing a probed `live_uuids` set into the preview renderer.
The current gap is a sibling case that fix did not cover.

Severity is Low: this is an information-quality bug, not a safety bug.
The executor's identity probe (the `match probed.state` at
`cli/src/recover.rs:2585`) prevents accidental reformatting; the
divergence only mis-advertises destructive work to the operator reading
`--dry-run` output. Fixing it preserves the recover dry-run honesty
contract documented at `docs/decisions/022-dry-run-preview-model.md`
and is consistent with the precedent the prior fix set.

## Scope

In-scope:

- `cli/src/recover.rs:841-881` -- the `FreshLuks` arm of
  `render_add_pool_mutation_recovery_steps`. Extend the per-target
  conditional language so the dry-run row truthfully describes both
  skip conditions.
- `cli/src/recover.rs:16095-16138` -- the existing test
  `plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_with_format_row`.
  Extend its assertions to pin the new conditional wording.

Out of scope:

- `RecoverableBraidLabeled` arm. It has no `CryptsetupLuksFormat` in
  preview or executor, so no analogous divergence exists there.
- `ensure_keyfile_enrolled` idempotency
  (`cli/src/recover.rs:2308-2322`). The executor's
  `verify_key_file` short-circuit can also skip the `CryptsetupLuksAddKeyFile`
  the preview emits, but addKey is `[safe]`-risk and operator-non-alarming.
  Track separately if we later want full byte-alignment with replay.
- Plan-time per-target `probe_config_disk` in dry-run. Doable
  (`probe::probe_config_disk` is read-only -- runs only
  `cryptsetup luksUUID` + `luksDump`), but would only resolve the
  divergence when the disk's by-id link exists at plan time; the
  "pool offline, disk warming up" recover dry-run would still need the
  conditional language as a fallback. Larger surface area for marginal
  benefit over the suffix-extension fix.

## Approach

Replace the bare shared `conditional_suffix` reference in the
`FreshLuks` arm with a mode-specific suffix that names both skip
conditions. Keep the existing shared suffix in the
`RecoverableBraidLabeled` arm unchanged (it has no format step to
qualify). Keep the `[destructive]` risk tag: when the dry-run can't
prove the disk is already PresentLuks, the worst-case command is still
destructive, and the conditional wording makes the "may not run"
caveat operator-visible.

### Code change

File: `cli/src/recover.rs`

In `render_add_pool_mutation_recovery_steps`, the FreshLuks arm
currently (line 873-880):

```rust
steps.push(Step {
    risk: "destructive",
    description: format!(
        "replay fresh add target {}{conditional_suffix}",
        target.by_id
    ),
    commands,
});
```

Change to use a FreshLuks-specific suffix that appends the
format-skip clause to the existing whole-step clause, e.g.:

```rust
let fresh_conditional_suffix = format!(
    "{conditional_suffix} (the LUKS format command is also skipped \
     at runtime if the disk already shows a LUKS header with the \
     journaled UUID and the 'braid-{}' label)",
    target.name
);
steps.push(Step {
    risk: "destructive",
    description: format!(
        "replay fresh add target {}{fresh_conditional_suffix}",
        target.by_id
    ),
    commands,
});
```

Exact wording is the operator-visible surface; settle in review. The
key requirements:

- The clause names the LUKS format command specifically (not the whole
  step).
- The clause names the two conditions that gate the skip: matching LUKS
  UUID and matching label.
- The clause uses `--` not `—` per CLI output style
  (`AGENTS.md` "CLI Output Style").
- The clause sits inside the description string so the existing
  `Step` rendering at `cli/src/cmd.rs:380-385` includes it on the
  step's header line, between the description and the indented
  `$ <cmd>` rows.

`rendered_step_block` (cli/src/recover.rs:15872) is anchored on a
prefix substring (`"replay fresh add target /dev/disk/by-id/..."`),
so appending the new clause to the end of the description preserves
all existing test call sites.

### Test change

File: `cli/src/recover.rs:16095-16138`

Extend `plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_with_format_row`
to add an assertion alongside the existing
`"(skipped at runtime if open/scan reconciliation makes target live before replay)"`
check:

```rust
assert!(
    disk2_block.contains("LUKS format command is also skipped")
        && disk2_block.contains("journaled UUID")
        && disk2_block.contains("label")
        && disk2_block.contains("braid-disk2"),
    "fresh replay row should advertise per-command format skip: {disk2_block:?}",
);
```

Behavioral, structure-insensitive: asserts the operator-facing clause
exists, not the rendering layout. If we later split the FreshLuks step
into format + setup-and-add steps, the clause can move; the assertion
just needs to follow it.

Note the deliberate pairing: pinning `"journaled UUID"` and `"label"`
independently mirrors the executor's two-gate identity check at
`cli/src/recover.rs:2596-2613`, where `PresentLuks` triggers the
format-skip only when both `uuid` AND `label` match. Pinning
`"braid-disk2"` alone would allow a regression that drops the label
condition from the conditional wording (e.g. "the journaled UUID
(braid-disk2)") to pass silently.

The existing assertion that the `$ cryptsetup luksFormat` argv row is
still rendered (lines 16122-16136) stays unchanged -- the dry-run
still emits the command, it just now honestly states the conditional.

The sibling test
`plan_recover_dry_run_pool_mutation_already_mounted_all_live_mixed_modes_renders_safe_placeholders`
(line 15977) covers the orthogonal "target already live in pool" path
and does not need updating: it goes through the `live_uuids` early-out
branch (`cli/src/recover.rs:778-794`), not the FreshLuks-replay branch.

## Files to modify

- `cli/src/recover.rs` -- one suffix construction in the FreshLuks
  preview arm; one new assertion in the existing not-mounted-fresh
  dry-run test.

## Verification

1. `just test-rust` -- run the Rust unit tests. Pin tests of interest:
   - `plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_with_format_row`
     (must pass with the new assertion).
   - `plan_recover_dry_run_pool_mutation_already_mounted_all_live_mixed_modes_renders_safe_placeholders`
     (must still pass, untouched).
   - All other `plan_recover_*` dry-run tests in
     `cli/src/recover.rs` that match on `"replay fresh add target"`
     prefixes (must still pass; substring matching is robust to the
     appended clause).
2. Eyeball a real dry-run rendering for a FreshLuks crash scenario.
   The cheapest path is the existing test harness -- inspect the
   `rendered` string in
   `plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_with_format_row`
   by adding a `dbg!(&rendered)` locally and confirming the two
   conditional clauses read naturally on one description line.
3. (Optional, but recommended once before commit) Run the VM test
   suite that covers add-recovery: `just test-vm` filtered to
   add-recovery tests, to confirm no incidental string-matching
   regression in VM-side scripts. Add-recovery VM tests live in
   `tests/test-*.py` -- grep `tests/` for `recover` + `add` to pin
   the list.

No new VM test is needed: this is a per-string operator-visible
correctness fix in the dry-run renderer, and the unit-test layer is the
right place for the assertion. A VM test would add cost without
improving coverage of the actual change.
