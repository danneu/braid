# Test: replace --enroll DIR refuses on slot-1 conflict
#
# Intent: when the new disk is already LUKS-formatted AND slot 1 is
# already occupied by an unknown key, `braid replace --enroll DIR`
# must fail with the canonical "slot 1 ... occupied by an unknown
# key" remediation text, exit non-zero, and NOT write a journal.
#
# Why it exists: pre-refactor, the silent-drop path proceeded with
# the replace anyway, the user lost no slot data but also got no
# `--enroll` work done. Routing through `plan_single_disk_
# enrollment` makes the slot-1 conflict an explicit, journaled-free
# refusal -- the operator must clear the conflict manually before
# braid will touch the disk.
#
# Scenario: someone enrolled an unknown key into slot 1 of the
# replacement disk (perhaps a previous owner). Operator passes
# `--enroll DIR`; braid refuses with the cryptsetup luksKillSlot
# remediation hint and preserves all state.

import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes"
    )


# --- Phase 0: build pool ---

with subtest("Setup: build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

# --- Phase 1: pre-format disk4 with an UNKNOWN key in slot 1 ---

with subtest("Pre-format disk4 with slot 0 + unknown slot 1"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )
    # Drop a random key into slot 1, simulating "previous owner left
    # something there".
    machine.succeed(
        "dd if=/dev/urandom of=/tmp/foreign.key bs=4096 count=1 iflag=fullblock"
    )
    machine.succeed("chmod 400 /tmp/foreign.key")
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksAddKey --batch-mode --key-slot 1 --key-file=- {luks_opts} "
        f"/dev/disk/by-id/virtio-disk4 /tmp/foreign.key"
    )
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk4"
    )
    assert '"1"' in dump, f"slot 1 should be occupied; got:\n{dump}"

    # Operator's intended keyfile (different bytes from foreign.key)
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Phase 2: replace must refuse cleanly ---

with subtest("replace --enroll refuses with luksKillSlot remediation"):
    pq = shlex.quote(passphrase)
    cmd = (
        f"printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 "
        f"--luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 "
        f"--old disk2 --new disk4=/dev/disk/by-id/virtio-disk4 "
        f"--passphrase-stdin --yes --enroll /tmp"
    )
    exit_code, output = machine.execute(f"{cmd} 2>&1")
    assert exit_code != 0, (
        f"replace must refuse on slot-1 conflict; got exit {exit_code}, output:\n{output}"
    )
    assert "slot 1 on disk4" in output, (
        f"missing per-disk slot-1 wording; got:\n{output}"
    )
    assert "occupied by an unknown key" in output, (
        f"missing canonical occupancy wording; got:\n{output}"
    )
    assert "luksKillSlot" in output, (
        f"missing luksKillSlot remediation; got:\n{output}"
    )

with subtest("No journal was written"):
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("Pool state is unchanged"):
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    for name in ["braid-disk1", "braid-disk2", "braid-disk3"]:
        assert f"/dev/mapper/{name}" in fi_show, f"{name} missing:\n{fi_show}"
    assert "braid-disk4" not in fi_show, f"disk4 must not be in pool:\n{fi_show}"

machine.shutdown()
