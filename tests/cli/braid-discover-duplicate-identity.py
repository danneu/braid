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
