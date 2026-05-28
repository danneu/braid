# Strengthen the wrong-`--missing-id` VM subtest to verify pre-mutation rejection

## Context

`tests/cli/replace-dead-disk.py:181-193` ("Wrong --missing-id is rejected
early (no pool mutation)") asserts only `status != 0`. Its own comment claims
the wrong `--missing-id` is "caught at validation time (before any LUKS
formatting or pool changes)", but the assertion never verifies that intent.

The rejection genuinely is pre-mutation today: `resolve_replace_source` runs
inside `plan_replace` (`cli/src/replace.rs:1288`) and returns
`OldDevidMismatch` (`replace.rs:123-130`, message at `:124`); `cmd_replace`
returns that plan error at `replace.rs:1530` *before* `plan.execute()`
(`:1537`) -- and `execute()` is the only thing that writes the journal,
LUKS-formats the new disk, or runs `btrfs replace start`. The wording is
pinned only by the unit test `missing_id_disagrees_with_persisted_devid`
(`replace.rs:2766`), which calls the pure `resolve_replace_source` directly
and therefore cannot prove the *live* `cmd_replace` path rejects before
mutation.

Gap: a regression that moved the devid cross-check from planning into a late
failure (e.g. failing inside `btrfs replace start` after a journal write +
LUKS format) would still exit non-zero, so the current subtest would still
pass while silently stranding a journal and a formatted disk. This subtest is
the only end-to-end coverage of that ordering.

Sibling tests already verify "rejected before mutation" properly; this one is
the outlier. The fix copies the established in-tree pattern.

## Change

Edit the single subtest at `tests/cli/replace-dead-disk.py:181-193`. Keep the
existing `machine.execute(... + " 2>&1")` / `status != 0` core; add a snapshot
before the command and four no-mutation/identity assertions after it, mirroring
`replace-cloned-luks-header-rejected.py:88-108`.

Target shape:

```python
with subtest("Wrong --missing-id is rejected early (no pool mutation)"):
    # Wrong --missing-id is caught at validation time, before any LUKS
    # formatting or pool changes. Snapshot pool.json and the btrfs array
    # first so we can prove nothing mutated.
    machine.succeed("cp /var/lib/braid/pool.json /tmp/pool-before-wrong-id.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-before-wrong-id.txt")

    wrong_devid = 9999
    (status, output) = machine.execute(
        replace_cmd("disk3", "disk5", extra=f"--missing-id {wrong_devid}") + " 2>&1"
    )
    assert status != 0, (
        f"Expected failure with wrong --missing-id {wrong_devid}, got exit 0: {output}"
    )
    print(f"Wrong --missing-id error (expected):\n{output}")

    # Must be the early devid cross-check (OldDevidMismatch), not a late
    # failure after journal/LUKS/btrfs mutation.
    assert "--old and --missing-id disagree" in output, (
        f"Expected the devid-disagreement typo guard, got: {output}"
    )

    # No journal stranded, pool membership untouched, btrfs array untouched,
    # and the new disk was never LUKS-formatted -- proves the rejection
    # landed before execute() ran any mutation.
    machine.fail("test -e /var/lib/braid/pending-op.json")
    machine.succeed("cmp /tmp/pool-before-wrong-id.json /var/lib/braid/pool.json")
    machine.succeed("btrfs fi show /mnt/storage > /tmp/fi-show-after-wrong-id.txt")
    machine.succeed("cmp /tmp/fi-show-before-wrong-id.txt /tmp/fi-show-after-wrong-id.txt")
    machine.fail("cryptsetup isLuks /dev/disk/by-id/virtio-disk5")
```

### Why each assertion (and why this is the right scope)

All five additions are behavioral and structure-insensitive (observable error
text + on-disk/array state), so they meet the test-quality bar; none reference
internal symbol names.

- **`"--old and --missing-id disagree"` substring** -- confirms the rejection
  is specifically the `OldDevidMismatch` typo guard (`replace.rs:124`), not
  some other non-zero exit. `disagree` is unique to this user-facing message
  (verified: no other CLI error string contains it), and the chosen substring
  omits the trailing "...about which member is being replaced" clause to stay
  robust to minor rewording.
- **`test -e pending-op.json` absent** -- directly catches the finding's named
  regression: a late failure after the journal write would leave
  `/var/lib/braid/pending-op.json` present. `clear_journal`
  (`cli/src/journal.rs:290`) removes it on success, so it is absent here after
  Phase 1's successful replace.
- **`cmp` pool.json before/after** -- catches any pool-membership mutation
  (byte-identical, robust to whatever the pre-state is; no hardcoded expected
  state).
- **`cmp` `btrfs fi show` before/after** -- catches any mutation of the btrfs
  array itself (e.g. a `btrfs replace start` that ran with the bad devid).
- **`cryptsetup isLuks virtio-disk5` fails** -- nails the "before any LUKS
  formatting" half of the comment. disk5 is pristine until the correct replace
  at line 195, so it is non-LUKS here; a format-before-validate regression
  would flip this.

## Files

- `tests/cli/replace-dead-disk.py` -- modify only the subtest at lines
  181-193. No other test, no Rust, no module changes. `disk3_devid`
  (line 102) is unrelated to this subtest and stays as-is for the
  correct-`--missing-id` subtest at line 195.

## Idioms reused (existing house patterns, do not invent)

- `cp` snapshot + `cmp` for byte-identical state, and `machine.fail("test -e
  /var/lib/braid/pending-op.json")` for journal absence, and `btrfs fi show`
  before/after `cmp`: `tests/cli/replace-cloned-luks-header-rejected.py:88-108`
  (a replace test asserting the same "refused before mutation" property).
- Error-substring-after-non-zero-exit:
  `tests/cli/add-passphrase-mismatch.py:78-93` (`error_marker` substring) and
  the `for needle in [...]` form at
  `replace-cloned-luks-header-rejected.py:97-103`.
- New disk not LUKS-formatted after a refused mutation:
  `add-passphrase-mismatch.py:95-97` (`cryptsetup isLuks` expected to fail).

## Verification

1. `just test-vm replace-dead-disk` -- confirm the strengthened subtest passes
   end-to-end (the wrong-id command still exits non-zero, the new substring is
   present, and all four no-mutation/identity assertions hold).
2. Sanity-check the assertions are real (not vacuously passing): the substring
   `--old and --missing-id disagree` is emitted by `replace.rs:124`; the
   `cmp`/`test -e`/`isLuks` checks exercise live VM state.
3. No fixture or parser surface is touched, so no `capture-*-fixtures` /
   `test-parsers` / `test-rust` run is required.
