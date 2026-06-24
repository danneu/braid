# Plan: end-to-end VM coverage for the interactive 2->1 redundancy-loss prompt

## Context

`braid remove <disk>` on a 2-disk pool drops the array to a single disk with no
RAID1 redundancy. This is irreversible, so when `--yes` is absent the command
prints a go/no-go prompt whose last line is:

```
WARNING: Pool will have 1 disk -- no RAID1 redundancy.
```

Today that gate is exercised **only** in-process, against the `RecordingConfirm`
seam (`cli/src/remove.rs#cmd_remove_accepted_confirm_records_prompt_with_warning`
and `#cmd_remove_declined_confirm_aborts_before_side_effects`). The VM suite's
only redundancy-reducing remove (`tests/cli/braid-remove-disk.py` Phase 3) runs
`--yes`, which bypasses the prompt entirely. The unit test's own comment even
claims it is the sole coverage.

That leaves the real process boundary untested for this command: nothing pins
that `main.rs` honors `yes=false` (no `--yes` leak), that `RealConfirm` actually
reads the answer from stdin, or that the warning reaches **stderr** rather than
the wrong stream. These are exactly the wiring regressions that would silently
disarm the last human checkpoint before a redundancy-losing operation.

The finding that surfaced this hedged that "tty prompts are awkward in the VM
harness" and might be undocumentable-only. That premise is false: the harness
drives piped-stdin confirmations routinely. `tests/cli/replace-preview-warnings.py`
Phase 2c (`printf 'no\n' | braid replace ... 2>err`, then assert non-zero exit +
warning on stderr + `btrfs fi show` unchanged) is a line-for-line template, and
`tests/cli/multi-add.py` / `confirm-then-passphrase-on-stdin.py` drive the same
`RealConfirm`+stdin path for other commands. There is currently **no** VM test
that declines *any* `braid remove`. So the gap is simply unfilled, not a designed
limitation -- the fix is to fill it.

Outcome: focused VM coverage that drives the real interactive prompt for a 2->1
remove on BOTH answers -- "no" aborts cleanly, "yes" proceeds -- plus a one-line
correction to the now-stale "ONLY coverage" comment. Pairing the two answers is
what pins the stdin contract: a decline-only test cannot tell "read 'no', aborted"
apart from "ignored stdin, always aborts", so the accepted-"yes" path (which must
proceed) is the half that proves the piped answer is actually consumed.

## Change 1 -- drive the real prompt on both answers (primary)

File: `tests/cli/braid-remove-disk.py`

At Phase 3 the pool is `disk1 + disk2` (Phase 2 already removed disk3). Two edits
here:

1. **Insert** a new decline check (and a no-mutation check) at the top of Phase 3,
   before any mutating remove. The decline makes no mutation, so the mutating
   remove that follows still finds the 2-disk state.
2. **Convert** the existing mutating subtest from the `--yes` bypass
   (`remove_cmd('disk2')`) to an interactive accept (`printf 'yes\n' | braid
   remove disk2`, no `--yes`), keeping all its success/progress/topology
   assertions. This is the half of the contract that proves stdin is consumed.

No coverage is lost by converting Phase 3. The `--yes` bypass *at a 2->1 remove*
-- i.e. `--yes` suppressing this very redundancy warning at `remaining == 1` -- is
guarded in-process by the `two_to_one_remove_invokes_survivor_capacity_preflight`
unit test (`cli/src/remove.rs`): it runs a 2->1 remove with the builder's default
`yes: true` (`cli/src/test_fixtures/remove.rs`) against an *unarmed*
`RecordingConfirm`, which panics if the gate prompts, so a regression where
`--yes` still prompts at `remaining == 1` fails there. Phase 2's 3->2
`remove_cmd('disk3')` (a `--yes` remove at `remaining == 2`, which carries no
redundancy warning) only exercises the weaker general
`--yes`-skips-the-normal-prompt path. Converting Phase 3 therefore upgrades the
more critical 2->1 path to the real prompt without dropping any guard.

Reused harness facts (all confirmed):

- `member_names(pm)` / `member(pm, name)` and `read_pool()` are already in scope
  (`tests/cli/member_helpers.py`, concatenated via
  `tests/cli/braid-remove-disk.nix` `testScript`).
- Decline idiom: `machine.execute("printf 'no\\n' | ... 2>/tmp/x.err")` returns
  `(status, _)`; `status != 0` is the authoritative abort signal. Read stderr
  back with `machine.succeed("cat /tmp/x.err")`.
- Both the prompt (`RealConfirm` `eprint!`) and the CLI error
  (`print_cli_error` -> `eprintln!`, `cli/src/main.rs#print_cli_error`) go to
  **stderr**, so one `2>` capture gets the warning *and* `aborted by user`.
- Stranded-recovery check: `machine.fail("test -e /var/lib/braid/pending-op.json")`
  (path from `cli/src/state_paths.rs`, `pending_op_json`). This is the VM analog
  of the unit test's "journal is None" assertion -- the decline aborts above
  `journal::write_journal`.
- Update the Phase 3 header comment (currently lines ~131-133): it presently
  reads "With --yes, the interactive redundancy confirmation is bypassed."
  Rewrite it to describe what Phase 3 now covers -- interactive decline
  (no -> abort) then interactive accept (yes -> proceed) -- and attribute the
  `--yes`-at-2->1 bypass (suppressing this warning at `remaining == 1`) to the
  `two_to_one_remove_invokes_survivor_capacity_preflight` unit test
  (unarmed-confirm-would-panic), NOT to Phase 2 (whose 3->2 `--yes` remove only
  covers the general no-prompt path at `remaining == 2`).
- `cp` + `cmp` of `/var/lib/braid/pool.json` is the established repo idiom for
  "byte-identical after a rejected mutation" (e.g. `tests/cli/braid-add-disk.py`,
  `tests/cli/replace-dead-disk.py`); use it for the no-mutation check.

Subtest to add (carry the in-file Intent / Why it exists / Scenario preamble
style used by the `replace-preview-warnings.py` interactive subtests):

```python
with subtest("Interactive 2->1 remove: declining at the real prompt aborts with the no-RAID1 warning"):
    # Intent: the real stdin/stderr confirm path (RealConfirm, not the
    #   in-process RecordingConfirm seam) shows the single-survivor warning on
    #   stderr, and a piped "no" aborts before any mutation.
    # Why it exists: this warning is the last human checkpoint before an
    #   irreversible, redundancy-losing remove. The Rust unit tests only cover
    #   the in-process Confirm seam. This half pins that yes=false is honored
    #   (a piped "no" aborts -- no --yes leak) and that the warning lands on
    #   stderr, not the wrong stream. The paired interactive-accept subtest below
    #   pins the other half: that the piped answer is actually consumed.
    # Scenario: operator runs `braid remove disk2` on a healthy 2-disk pool,
    #   sees the no-RAID1 warning, types "no", and the pool is untouched.
    # Snapshot pool.json immediately before the rejected op so the no-mutation
    # check below can assert it is byte-identical (repo idiom).
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-rm2-decline.json")
    (status, _) = machine.execute(
        "printf 'no\\n' | braid remove disk2 >/tmp/rm2-decline.out 2>/tmp/rm2-decline.err"
    )
    assert status != 0, (
        "declining the prompt must abort with non-zero exit; "
        "exit 0 would mean --yes leaked into the interactive path"
    )
    decline_err = machine.succeed("cat /tmp/rm2-decline.err")
    warning = "WARNING: Pool will have 1 disk -- no RAID1 redundancy."
    assert warning in decline_err, (
        f"single-survivor warning must appear on stderr at the real prompt; got: {decline_err!r}"
    )
    assert "aborted by user" in decline_err, (
        f"decline must report the abort reason; got: {decline_err!r}"
    )

with subtest("Declined 2->1 remove leaves pool.json, the RAID1 topology, and recovery state untouched"):
    # Membership names present -- readable first-failure signal...
    pm = read_pool()
    assert "disk2" in member_names(pm), f"declined remove must keep disk2 in pool.json: {pm}"
    assert "disk1" in member_names(pm), f"disk1 must remain in pool.json: {pm}"
    # ...and pool.json byte-identical, so a declined remove cannot rewrite or
    # corrupt any membership field (devid, by_id, mapper, luks_uuid, added_at)
    # while leaving the names in place. `cmp` exits non-zero on any difference.
    machine.succeed("cmp /tmp/pool-before-rm2-decline.json /var/lib/braid/pool.json")

    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    devid_count = fi_show.count("devid")
    assert devid_count == 2, f"declined remove must leave 2 devices, got {devid_count}:\n{fi_show}"
    for name in ["braid-disk1", "braid-disk2"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing after declined remove:\n{fi_show}"

    df_output = machine.succeed("btrfs fi df /mnt/storage")
    assert "RAID1" in df_output, f"declined remove must keep the RAID1 profile:\n{df_output}"

    # No recovery state stranded: the decline aborts above journal::write_journal.
    machine.fail("test -e /var/lib/braid/pending-op.json")
    # The LUKS mapper must NOT have been torn down on a declined remove.
    machine.succeed("test -e /dev/mapper/braid-disk2")

    content = machine.succeed("cat /mnt/storage/precious.txt").strip()
    assert content == "important data", f"data must survive a declined remove, got '{content}'"
```

Then **convert** the existing mutating subtest (currently
`with subtest("Redundancy-reducing remove with --yes succeeds"):`) in place --
this is the accept half that proves stdin is consumed:

- Retitle to:
  `with subtest("Interactive 2->1 remove: piping 'yes' at the real prompt proceeds (stdin consumed)"):`
- Replace its command line:
    - from: `machine.succeed(f"{remove_cmd('disk2')} >/tmp/rm2.out 2>/tmp/rm2.err")`
    - to:   `machine.succeed("printf 'yes\\n' | braid remove disk2 >/tmp/rm2.out 2>/tmp/rm2.err")`
- After the existing `rm2_err = machine.succeed("cat /tmp/rm2.err")`, add one
  assertion that the warning was shown before the answer was consumed (proves the
  gate ran on the real mutating path, not a bypass):

  ```python
  assert "WARNING: Pool will have 1 disk -- no RAID1 redundancy." in rm2_err, (
      f"interactive accept must still show the no-RAID1 warning; got: {rm2_err!r}"
  )
  ```
- Keep every existing `[wait]`/`[ok]` progress + ordering assertion (balance,
  device remove, trailing LUKS close) unchanged -- they already read `rm2_err`.

A regression that printed the prompt but ignored stdin and always aborted would
fail HERE (the remove would abort instead of proceeding), which is exactly the
hole the decline-only version could not catch.

Notes on assertion choices (keep behavioral, avoid duplicating the unit test):

- The warning **string on stderr** is the core content assertion -- it is the
  redundancy gate itself, and capturing only stderr means a regression that
  routed the prompt to stdout fails this check (the "wrong stream" guard).
- `status != 0` is the `--yes`-leak guard; an accidental default-yes would
  proceed and exit 0.
- Pool.json + `btrfs fi show`/`df` + absent `pending-op.json` + still-open mapper
  together pin "no mutation, no stranded recovery state" at the real boundary.
- Deliberately **not** re-asserting the `devid`/`2 disks -> 1 disk` prompt
  formatting -- the unit test owns prompt assembly; this test owns wiring. Adding
  format assertions here would be brittle and redundant.
- The decline and accept subtests pin the stdin contract **only as a pair**:
  decline kills the "`--yes` leaked / always proceeds" hypothesis, accept kills
  the "ignores stdin / always aborts" hypothesis. Neither answer alone suffices.
- The pool.json `cmp` is strictly stronger than the member-name check (it catches
  field rewrites/corruption that leave names intact) and matches the repo's
  rejected-mutation idiom.
- Deliberately **not** `cmp`-ing `btrfs fi show` output (some sibling tests do):
  it carries volatile per-device "used" byte counts. The behavioral topology
  checks (2 devids, both mappers present, RAID1 profile) pin the structural
  invariants without that flake risk.

## Change 2 -- correct the stale "ONLY coverage" comment

File: `cli/src/remove.rs`, the `Why it exists` block of
`cmd_remove_accepted_confirm_records_prompt_with_warning` (currently ~lines
1021-1026). Replace the false "this is the ONLY coverage" claim with a statement
that this test pins prompt *assembly* at the seam while the end-to-end path is
covered by the new VM subtests:

```rust
    // Why it exists: this pins the warning-prompt ASSEMBLY at the in-process
    //   Confirm seam (warning present once, correct target, correct
    //   transition). The end-to-end stdin/stderr path -- RealConfirm reading a
    //   piped answer, the warning landing on stderr, and `--yes` staying off
    //   the interactive path -- is covered by the interactive decline + accept
    //   subtests in tests/cli/braid-remove-disk.py. Asserting behavior instead of
    //   byte-exact assembly keeps the test pinned to the contract, not cosmetic
    //   layout, while the literal still catches wording regressions.
```

(Plain file reference, matching the existing comment style and the
`doc-citations.md` rule against line-number citations.)

## Files to modify

- `tests/cli/braid-remove-disk.py` -- add the decline + no-mutation subtests,
  convert the existing mutating subtest from `--yes` to interactive `yes`, and
  update the Phase 3 header comment.
- `cli/src/remove.rs` -- correct one comment block (no behavior change).

## Out of scope (deliberate)

- `remove_missing` has the same unit-only confirm pattern, but its prompt carries
  no redundancy warning and the operation is not redundancy-losing, so it does
  not warrant the same end-to-end gate. Left untouched.
- No production code changes: the wiring is already correct
  (`cli/src/main.rs` passes `yes: args.common.yes` and `confirm: &RealConfirm`);
  this plan only adds the missing regression net and fixes a stale comment.

## Verification

1. Run the targeted VM test (the only check that needs to pass for this change):

   ```
   just test-vm braid-remove-disk
   ```

   Expect the two new subtests to pass alongside the existing Phase 0-5 flow.

2. Confirm the subtests fail for the right reason (repo TDD norm) -- two
   independent mutations, each reverted after:
   - Force `yes: true` at the `cmd_remove` callsite in `main.rs` (simulates a
     `--yes` leak): the **decline** subtest must go red (the remove would proceed,
     so `status != 0` and the abort-message assertions fail).
   - Make the confirm path ignore stdin and always abort by inserting an early
     `return Err("aborted by user".into())` at the top of
     `cli/src/confirm.rs#confirm_yes_from` (skipping the stdin read loop). This
     leaves `RealConfirm::confirm`'s upstream `eprint!("{prompt}")` intact, so the
     warning still reaches stderr: the **decline** subtest stays **green** (it
     expects an abort and still sees the warning) while only the **accept**
     subtest goes **red** (its mutating remove aborts instead of proceeding).
     Keeping decline green is the point -- it demonstrates that decline genuinely
     cannot tell "read no, aborted" from "ignored stdin, always aborts", so the
     accept path is the unique catcher of a stdin-ignore regression. (Do NOT stub
     at the `RealConfirm::confirm` level: dropping its `eprint!("{prompt}")` would
     also fail decline's `warning in decline_err` assertion, muddying the
     isolation.)
   Together these prove the pair pins the stdin contract rather than passing
   vacuously.

3. Rust suite still green (comment-only change, but cheap to confirm):

   ```
   just test-rust
   ```

4. ASCII-output lint is unaffected (the `--` in the warning is already ASCII and
   the literal is unchanged), but the repo's doc/output checks run in CI anyway.
