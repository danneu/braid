# Plan: end-to-end VM test for discover's structural-scan refusals

## Context

`braid discover` reconstructs `pool.json` from attached braid-labeled LUKS2
disks. Before it trusts a scan it refuses two structural hazards:

- `DuplicateUuid` -- two physically distinct disks share one LUKS UUID (the
  dd-cloned-disk case; the headline hazard discover exists to catch).
- `LabelCollision` -- two distinct disks both carry the same `braid-<name>`
  label.

Both are produced by the scanner and *rendered to the operator* through one
shared path in `main.rs`:

```
discover_pool_members -> drain_warnings (Err arm) -> print_cli_error -> exit(1)
```
(`cli/src/main.rs:970-978`; preview rows are computed only *after* this check at
`cli/src/main.rs:979-983`, and both bare `braid discover` and
`braid discover --write` flow through the same arm.)

**The gap:** unit tests prove the scanner *produces* both errors
(`discover.rs::discover_duplicate_uuid_surfaces_friendly_error`,
`discover_fails_on_label_collision_across_disks`,
`drain_warnings_writes_warnings_before_returning_error`), but **no VM test
drives the `Err` arm through the real binary**. The only discover refusal with
end-to-end coverage is the structurally *separate* empty-scan path
(`members.is_empty() -> NoMembersDiscovered`, `main.rs:979-982`), exercised by
`braid-discover-empty-scan.py` -- where `drain_warnings` returns `Ok`, so the
`Err` arm is never hit. `braid-discover.py` only ever boots two clean, distinct
disks (the success path).

A regression that printed the preview before the error check, routed a
structural error to stdout, or exited 0 would pass every existing test. Notably,
`add` and `replace` already have end-to-end cloned-LUKS-header rejection tests
(`braid-add-cloned-luks-header-rejected`, `replace-cloned-luks-header-rejected`);
discover is the third command that refuses this hazard and the only one missing
the test. This plan completes that family.

## Recommended approach

Add **one** new NixOS VM test node that formats LUKS at runtime (the proven
pattern from the cloned-header family) and drives **both** refusals through the
real binary in a single boot. Two subtests; each checks **bare** and **`--write`**
and asserts the operator contract: exit 1, empty stdout (no preview rows), the
remediation wording on stderr, and (for `--write`) no `pool.json` written.

Runtime formatting -- rather than the `initrd-fixture.nix` `diskUuidMap` route
that `braid-discover-name-order.nix` uses -- is chosen because it lets one VM
boot cover both the shared-UUID and shared-label scenarios (the fixture bakes
UUID/label at format time and ties the label to the disk serial 1:1, so it
cannot host both scenarios), and it matches how `add`/`replace` already test the
identical hazard.

### Files to change

**1. New `tests/cli/braid-discover-duplicate-identity.nix`** -- node config
mirrors `braid-discover-empty-scan.nix` (imports the braid module, the way the
other discover tests do) but adds two blank disks and `cryptsetup` for runtime
formatting:

```nix
# Test: braid-discover-duplicate-identity
#
# What: Boots two blank disks, then formats them at runtime into the two
# structural hazards discover must refuse -- (1) distinct braid labels sharing
# one LUKS UUID (dd-cloned disk), and (2) two distinct disks sharing one braid
# label. For each, asserts `braid discover` and `braid discover --write` exit 1,
# print no preview rows to stdout, emit the remediation wording on stderr, and
# write no pool.json.
#
# Why: discover's DuplicateUuid / LabelCollision refusals (the cloned-disk
# hazard discover exists to catch) are proven only at the scanner unit-test
# level. The Err arm of drain_warnings in main.rs -- print_cli_error -> exit 1 --
# has no end-to-end coverage; braid-discover-empty-scan.py exercises the
# structurally separate Ok(empty) -> NoMembersDiscovered path instead. This is
# the discover sibling of braid-add/replace-cloned-luks-header-rejected.
{ braid }:
{
  name = "braid-discover-duplicate-identity";

  nodes.machine =
    { pkgs, ... }:
    {
      imports = [ ../../modules/braid ];

      braid = {
        enable = true;
        package = braid;
      };

      virtualisation.emptyDiskImages = [
        { size = 256; driveConfig.deviceExtraOpts.serial = "disk1"; }
        { size = 256; driveConfig.deviceExtraOpts.serial = "disk2"; }
      ];

      environment.systemPackages = [ pkgs.cryptsetup ];
    };

  testScript = builtins.readFile ./braid-discover-duplicate-identity.py;
}
```

**2. New `tests/cli/braid-discover-duplicate-identity.py`** -- the test logic.
Reuse `braid-discover-empty-scan.py`'s refusal-assertion shape (exit 1 exactly /
empty stdout / message on stderr) and the canonical LUKS-format invocation from
`initrd-fixture.nix:124-127` (`--batch-mode ... --key-file=- --pbkdf pbkdf2
--pbkdf-force-iterations 1000`). `--batch-mode` is required because subtest 2
reformats already-LUKS disks (it auto-accepts the overwrite prompt).

```python
# Intent: `braid discover` and `braid discover --write` refuse the two
#   structural-scan hazards -- a duplicate LUKS UUID across distinct labels
#   (dd-cloned disk) and a braid-label collision across distinct disks -- with
#   exit 1, no stdout preview, the remediation wording on stderr, and no
#   pool.json written.
# Why it exists: these refusals flow through main.rs's drain_warnings Err arm
#   (print_cli_error -> exit 1), which has no end-to-end coverage; the unit
#   tests only prove the scanner produces the errors, and the empty-scan VM test
#   exercises the structurally separate Ok(empty) -> NoMembersDiscovered path.
#   A regression printing the preview before the error check, routing the error
#   to stdout, or exiting 0 would pass every existing test. Sibling of
#   braid-add/replace-cloned-luks-header-rejected.
# Scenario: an operator rebuilding a lost pool.json accidentally leaves a
#   dd-cloned spare (shared UUID) or a mislabeled disk (shared label) attached;
#   discover must refuse cleanly rather than write incomplete/ambiguous state.

import shlex

PASSPHRASE = "testpassphrase"
POOL_JSON = "/var/lib/braid/pool.json"


def luks_format(name, uuid, label):
    """LUKS2-format /dev/disk/by-id/virtio-<name> with an explicit UUID and
    braid label. --batch-mode auto-accepts the overwrite prompt so a disk can be
    reformatted between subtests; fast PBKDF keeps the test quick."""
    machine.succeed(
        "echo -n " + shlex.quote(PASSPHRASE) + " | cryptsetup luksFormat "
        "--batch-mode --uuid " + uuid + " --label " + label + " --key-file=- "
        "--pbkdf pbkdf2 --pbkdf-force-iterations 1000 "
        "/dev/disk/by-id/virtio-" + name
    )


def assert_discover_refuses(label, command, needles):
    """One discover invocation must refuse: exit 1 exactly (not just non-zero --
    a panic is 101, a misroute is 2), empty stdout, and on stderr every needle
    present AND no preview-shaped row. The negative stderr check is what makes
    "preview withheld" true rather than merely "preview not on stdout": a
    regression that rendered render_preview_lines before the error check and
    emitted the rows through the stderr writer would pass an empty-stdout check
    but leak the preview. A preview row is '  <name> = <by-id>'
    (discover.rs#render_preview_lines), whereas both refusal messages quote
    by-id paths in parens '(path)' -- so ' = /dev/disk/by-id/' matches a leaked
    preview row but never the error text. Asserts internally."""
    rc, _ = machine.execute(command + " >/tmp/out 2>/tmp/err")
    out = machine.succeed("cat /tmp/out")
    err = machine.succeed("cat /tmp/err")
    assert rc == 1, label + ": expected exit 1; rc=" + str(rc) + "\n" + err
    assert out.strip() == "", label + ": printed preview rows on stdout:\n" + out
    assert " = /dev/disk/by-id/" not in err, (
        label + ": leaked preview rows onto stderr:\n" + err
    )
    for needle in needles:
        assert needle in err, label + ": missing " + repr(needle) + ":\n" + err


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("duplicate LUKS UUID across distinct labels refuses, no preview, no write"):
    # Distinct labels (braid-disk1 / braid-disk2) so LabelCollision does NOT
    # fire first; one shared UUID so the scan reaches DuplicateUuid.
    shared = "11111111-1111-1111-1111-111111111111"
    luks_format("disk1", shared, "braid-disk1")
    luks_format("disk2", shared, "braid-disk2")
    machine.succeed("test ! -e " + POOL_JSON)  # bare discover must reach the scan
    needles = [
        "duplicate LUKS UUID: braid-",
        "share UUID " + shared,
        "detach the cloned or unintended disk before retrying",
    ]
    assert_discover_refuses("bare discover (dup uuid)", "braid discover", needles)
    assert_discover_refuses("discover --write (dup uuid)", "braid discover --write", needles)
    machine.succeed("test ! -e " + POOL_JSON)

with subtest("same braid label on two distinct disks refuses, no preview, no write"):
    # Same label, distinct UUIDs: an unambiguous label collision across two
    # genuinely different disks. LabelCollision fires before the UUID pass.
    luks_format("disk1", "22222222-2222-2222-2222-222222222222", "braid-foo")
    luks_format("disk2", "33333333-3333-3333-3333-333333333333", "braid-foo")
    machine.succeed("test ! -e " + POOL_JSON)
    needles = [
        "label collision: braid-foo",
        "found on two distinct devices",
        "relabel or detach one before retrying",
    ]
    assert_discover_refuses("bare discover (label collision)", "braid discover", needles)
    assert_discover_refuses("discover --write (label collision)", "braid discover --write", needles)
    machine.succeed("test ! -e " + POOL_JSON)

machine.shutdown()
```

**3. Register in `flake.nix`** -- add next to the other discover checks
(~line 258, after `braid-discover-empty-scan`; the list is grouped by family,
not alphabetical). Required or the test never runs (`docs/dev/testing.md`):

```nix
braid-discover-duplicate-identity = pkgs.testers.nixosTest (
  import ./tests/cli/braid-discover-duplicate-identity.nix {
    braid = linuxCrane.braid;
  }
);
```

## Design decisions

- **One node, runtime format (not `initrd-fixture` + `diskUuidMap`).** A single
  boot covers both scenarios; the fixture cannot, since it bakes UUID/label at
  format time and ties label to serial. Matches the `add`/`replace` cloned-header
  tests that exercise the same hazard.
- **Assert `rc == 1` exactly**, mirroring `braid-discover-empty-scan.py` and the
  `docs/commands/discover.md` "exits 1" contract -- stricter than the finding's
  "non-zero" and catches a panic (101) or misroute (exit 2).
- **Split stdout/stderr, with a negative stderr check.** The empty-stdout
  assertion guards against "error routed to stdout"; the additional
  `" = /dev/disk/by-id/"`-not-on-stderr assertion guards against a "preview
  rendered before the error check and emitted through the stderr writer"
  regression that an empty-stdout check alone would miss. Together they make the
  test prove the preview is *withheld*, not merely moved off stdout. The pattern
  discriminates a preview row (`  <name> = <by-id>`) from the refusal messages,
  which quote by-id paths in parens (`(path)`), so it never matches the error
  text or a skip warning.
- **Assertion substrings are the operator-facing remediation wording**, not
  internal structure -- behavioral and structure-insensitive, the same style as
  `braid-discover.py`'s pinned refusal clauses. Exact wording confirmed against
  `discover.rs:26-27` (LabelCollision) and `discover.rs:40-42` (DuplicateUuid).
- **String concatenation, not bare f-strings** -- `tests/**/*.py` reject an
  f-string with no `{placeholder}` at build-lint time (`docs/dev/testing.md`).

## Verification

1. Run the focused VM test (checks build on `aarch64-darwin` via the
   linux-builder per `AGENTS.md`):
   ```
   just test-vm braid-discover-duplicate-identity
   ```
   It should **pass on current HEAD** -- this is a characterization/regression
   test for already-correct wiring, not red-green TDD.

2. **Confirm it actually guards** (the project's "fail for the right reason"
   step): temporarily break `cli/src/main.rs`'s Discover arm -- e.g. move
   `render_preview_lines` + the `println!` loop ahead of the `drain_warnings`
   match, or change the `Err(e)` arm to `println!`/`exit(0)` -- and re-run; the
   test must fail on the empty-stdout and/or exit-code assertion. Revert.

3. No Rust changes, so `just test-rust` is unaffected; run it only as a sanity
   check that nothing else moved.

## Out of scope (deliberately not included)

- **End-to-end warnings-before-error ordering** (e.g. adding a third LUKS1 disk
  so a warning prints to stderr before the structural error). Already unit-tested
  by `drain_warnings_writes_warnings_before_returning_error` and
  `discover_surfaces_warnings_alongside_structural_error`; adding it here is
  scope creep for a Low-severity gap.
- **Generalizing `initrd-fixture.nix`** to decouple label from serial. The
  runtime-format approach makes it unnecessary.
- **Cross-file dedup** of the refusal-assertion helper shared with
  `braid-discover-empty-scan.py`. VM `.py` files are each `readFile`'d into
  separate derivations; the project does not share Python helper modules across
  them, so this would be over-engineering.
