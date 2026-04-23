# Fix: "Old device closed" prints unconditionally after best-effort LUKS close

## Context

In the `Live` branch of `cmd_replace` (cli/src/replace.rs:329-345), the old
LUKS mapper is closed best-effort after `btrfs replace` completes. Three
outcomes are possible: close succeeded (exit 0), close returned non-zero,
or the runner errored. All three are handled inside `match close_result`.

But the follow-up line -- `eprintln!("Old device closed. If repurposing the
physical disk, wipe it separately.")` -- sits *outside* the match, so it
fires on every path. On a real failure the operator sees:

```
Warning: failed to close LUKS mapper braid-disk1 (exit 5)
Old device closed. If repurposing the physical disk, wipe it separately.
```

The two lines contradict each other, and the second is actively dangerous:
the mapper may still be open on live data, so "wipe" guidance could
destroy the user's data on the source side of a completed replace.

No test covers the exact text of these messages, so this regression can
reappear silently.

## Fix

The code change is a one-liner: move the `eprintln!` into the success arm
of the existing match. The regression gate is a new CLI VM test modeled on
the existing `tests/cli/braid-remove-disk-busy.py` -- same pattern, but for
the replace command instead of remove.

## Files to modify

- `cli/src/replace.rs` -- move the `eprintln!` at line 345 into the `_ =>`
  arm of the match at line 343.
- `tests/cli/replace-live-disk-busy.py` (new) -- VM regression test.
- `tests/cli/replace-live-disk-busy.nix` (new) -- VM config (model on
  `tests/cli/replace-live-disk.nix`).
- `flake.nix` -- register the new test next to `replace-live-disk` around
  line 236.

## Code change

Replace lines 335-345 of `cli/src/replace.rs` with:

```rust
match close_result {
    Ok(r) if r.exit_status != 0 => {
        eprintln!(
            "Warning: failed to close LUKS mapper {} (exit {})",
            mapper, r.exit_status
        );
    }
    Err(e) => eprintln!("Warning: failed to close LUKS mapper {}: {}", mapper, e),
    _ => {
        eprintln!(
            "Old device closed. If repurposing the physical disk, wipe it separately."
        );
    }
}
```

No new helper, no new abstraction. The control-flow is now correct on
inspection.

## VM regression test

`tests/cli/replace-live-disk-busy.py` follows the pattern of
`tests/cli/braid-remove-disk-busy.py`:

1. Build a 3-disk RAID1 pool (`disk1`, `disk2`, `disk3`), using the same
   LUKS-pbkdf fast-iteration options and `add_cmd` helper already used in
   `tests/cli/replace-live-disk.py`.
2. Attach a loop device to `/dev/mapper/braid-disk2` with
   `losetup --find --show /dev/mapper/braid-disk2`. This holds an fd on
   the mapper so that `cryptsetup close braid-disk2` returns EBUSY after
   the btrfs replace completes. (btrfs replace itself proceeds fine -- it
   operates via the mount point, not by requiring exclusive access to the
   source mapper.)
3. Run `braid replace --old disk2 --new disk4=...` and capture stderr.
4. Assert:
   - Command exit status is 0 (best-effort close never fails the command).
   - Output contains `"Warning"` and `"braid-disk2"` (the close-failure
     warning was emitted).
   - Output does NOT contain `"Old device closed"` (the contradictory line
     is suppressed). **This is the regression gate for the bug.**
   - `/dev/mapper/braid-disk2` still exists (mapper remained open).
   - `btrfs fi show /mnt/storage` shows `braid-disk4` present and
     `braid-disk2` absent (the replace itself succeeded).
   - Data on `/mnt/storage/precious.txt` is intact.
5. Detach the loop device and confirm `cryptsetup close braid-disk2`
   succeeds, as a cleanup sanity check.

The block-comment header follows the project Intent / Why / Scenario
convention; the Scenario cites this specific bug (unconditional "Old
device closed" printing after a close failure).

`tests/cli/replace-live-disk-busy.nix` mirrors
`tests/cli/replace-live-disk.nix` (4 virtio disks, `braid` + `cryptsetup`
+ `btrfs-progs` in `systemPackages`, config at `/etc/braid/config.json`).

`flake.nix` gets one new entry modeled on the surrounding `replace-*`
entries:

```nix
replace-live-disk-busy = pkgs.testers.nixosTest (
  import ./tests/cli/replace-live-disk-busy.nix {
    braid = linuxCrane.braid;
  }
);
```

## Why a CLI VM test, not a unit-level formatter helper

An earlier draft of this plan proposed extracting a pure
`format_old_mapper_close_messages` helper and unit-testing its string
output. Rejected: those tests bind to an internal helper's signature, not
to the `braid replace` contract the bug violated. They're
structure-sensitive and create a single-use abstraction that exists only
to satisfy the tests. The repo already treats best-effort `cryptsetup
close` messaging as VM-test territory
(`tests/cli/braid-remove-disk-busy.py`) -- reusing that pattern for the
replace command produces a structure-insensitive test bound to
user-visible behavior.

## Verification

- `just test-vm replace-live-disk-busy` -- runs the new regression test.
  Should pass with the fix applied; should fail before the fix (the
  `"Old device closed"` absence assertion catches the bug). This is the
  primary gate.
- `just test-vm replace-live-disk` -- happy-path smoke check that live
  replace still succeeds end-to-end (pool state, mapper absence, data
  intact, membership). It does not assert on the success-arm output text,
  so it is not a gate for that line; it only confirms the fix did not
  break the happy path.
- `just test-rust` -- no Rust tests change, but run to confirm nothing
  regressed.

## Known coverage gap (accepted)

The new VM test forces the non-zero close-exit path (via `losetup` holding
an fd; `cryptsetup close` returns non-zero with EBUSY). It does not
exercise the `CmdError::Failed(_)` arm of the match, which corresponds to
the runner itself failing to invoke `cryptsetup` at all. The bug affects
both arms symmetrically, and covering the non-zero-exit arm is sufficient
regression coverage here -- the `CmdError` arm is practically unreachable
in a functioning VM. Left as an accepted gap.
