# Test: replace --enroll DIR against an already-LUKS new disk
#
# Intent: when `braid replace --enroll /tmp` targets a new disk that
# is already LUKS-formatted (mapper closed, slot 1 empty), the
# command must enroll the keyfile into slot 1 + back up the header,
# then complete the live replace. After the replace, the new disk's
# slot 1 must authenticate the keyfile (auto-unlock works).
#
# Why it exists: pre-refactor, this combination silently dropped the
# keyfile -- `replace --enroll` with `Some(kf) + PresentLuks` was a
# no-op, the new disk shipped without slot 1 enrolled, and the auto-
# unlock service couldn't open it. The refactor routes the decision
# through `plan_single_disk_enrollment`. This test pins the
# end-to-end shape: keyfile lands in slot 1, header backup contains
# slot 1, and unlock --key-file opens the pool after the replace.
#
# Scenario: returning braid disk that was LUKS-formatted but never
# had the operator's keyfile enrolled. Operator runs `braid replace`
# with `--enroll DIR` to install both a fresh slot 1 and the
# replacement at once.

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


def replace_cmd(old, new, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid replace --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 --old {old} --new {new}=/dev/disk/by-id/virtio-{new} --passphrase-stdin --yes {extra}"
    )


# --- Phase 0: build 3-disk pool ---

with subtest("Setup: build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))

    machine.succeed("echo 'enroll-existing-luks' > /mnt/storage/data.txt")
    machine.succeed("sync")

# --- Phase 1: pre-format disk4 as LUKS (no slot 1 yet) ---

with subtest("Pre-format disk4 as LUKS (slot 0 only)"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )
    machine.succeed("cryptsetup isLuks /dev/disk/by-id/virtio-disk4")
    machine.fail("test -e /dev/mapper/braid-disk4")

    luks_uuid_before = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()

    # Generate the keyfile braid will enroll.
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Phase 2: replace disk2 with disk4 + --enroll ---

with subtest("Replace disk2 with pre-LUKS disk4 + --enroll"):
    machine.succeed(
        f"{replace_cmd('disk2', 'disk4', '--enroll /tmp')} "
        f">/tmp/repl.out 2>/tmp/repl.err"
    )
    err = machine.succeed("cat /tmp/repl.err")

    # Status rows for the in-replace enrollment must surface.
    enroll_wait = "[wait] disk disk4: enrolling keyfile in slot 1..."
    enroll_ok = "[ok]   disk disk4: keyfile enrolled in slot 1"
    assert enroll_wait in err, f"missing enroll [wait] row; got: {err!r}"
    assert enroll_ok in err, f"missing enroll [ok] row; got: {err!r}"
    assert err.find(enroll_wait) < err.find(enroll_ok), (
        f"enroll wait must precede ok; got: {err!r}"
    )

    # Identity invariant: the disk was NOT re-formatted.
    luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert luks_uuid_after == luks_uuid_before, (
        f"LUKS UUID changed -- disk was reformatted!"
        f" before={luks_uuid_before} after={luks_uuid_after}"
    )

with subtest("Slot 1 occupied on disk4 (live header)"):
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk4"
    )
    assert '"1"' in dump, f"slot 1 missing from disk4 luksDump:\n{dump}"

with subtest("Header backup contains slot 1"):
    # The post-enroll backup is the artifact recovery / off-system
    # restore depend on; if the backup were taken before luksAddKey,
    # restoring it would silently destroy keyfile-based unlock.
    backup_dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata "
        "/var/lib/braid/luks-headers/braid-disk4.luksheader"
    )
    assert '"1"' in backup_dump, (
        f"slot 1 missing from header backup; got:\n{backup_dump}"
    )

# --- Phase 3: keyfile unlocks the new pool ---

with subtest("Unlock --key-file opens disk4 alongside disk1+disk3"):
    # Enroll the keyfile on the rest of the pool so unlock --key-file
    # can drive the whole set.
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )
    machine.succeed("braid lock")

    machine.succeed("braid unlock --key-file /tmp/braid.key")
    machine.succeed("mountpoint -q /mnt/storage")
    content = machine.succeed("cat /mnt/storage/data.txt").strip()
    assert content == "enroll-existing-luks", (
        f"data lost after replace+enroll: got {content!r}"
    )

machine.shutdown()
