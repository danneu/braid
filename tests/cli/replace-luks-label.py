# Test: replace-luks-label
#
# Intent: After `braid replace --old disk2 --new disk3`, the new LUKS2 volume
# on disk3 must carry the label "braid-disk3" so that luksDump / blkid can
# identify it by braid name.
#
# Why it exists: `braid add` sets --label braid-<name> when formatting, but
# `braid replace` was missing that flag. This test ensures replace matches
# add's labeling behavior.
#
# Scenario: Operator replaces a live disk, then runs
# `cryptsetup luksDump /dev/sdc` on the new drive and expects to see
# "Label: braid-disk3".

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


def replace_cmd(old, new):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes"
    )


with subtest("Build 2-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

with subtest("Replace disk2 with disk3"):
    machine.succeed(replace_cmd("disk2", "disk3"))

with subtest("LUKS label on new disk is braid-disk3"):
    dump = machine.succeed("cryptsetup luksDump /dev/disk/by-id/virtio-disk3")
    found = False
    for line in dump.splitlines():
        if line.strip().startswith("Label:"):
            label = line.split(":", 1)[1].strip()
            assert label == "braid-disk3", (
                f"expected LUKS label 'braid-disk3', got '{label}'"
            )
            found = True
            break
    assert found, f"no Label: line found in luksDump output:\n{dump}"

machine.shutdown()
