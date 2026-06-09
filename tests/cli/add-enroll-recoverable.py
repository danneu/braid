# Test: add --enroll DIR against a recoverable returning braid disk
#
# Intent: `braid add disk3=... --enroll /tmp` against a returning
# braid disk resolves to exactly one of three slot-1 outcomes.
# (1) Slot 1 empty (`NeedsEnroll`): enroll the keyfile in slot 1 +
# back up the header, then force-add the disk back to the pool.
# (2) Slot 1 already authenticating the same keyfile
# (`AlreadyEnrolled`): take the idempotent skip -- no addKey work,
# no new header backup. (3) Slot 1 occupied by an unknown key the
# keyfile does not authenticate (`SlotConflict`): a pre-journal
# refusal with the canonical `cryptsetup luksKillSlot` remediation,
# exit non-zero, no journal, no pool mutation, and the unknown
# slot-1 key left untouched.
#
# Why it exists: the silent-drop bug fix on the add path. Pre-
# refactor, `Some(kf) + recoverable braid disk` was a no-op -- the
# disk was re-added but slot 1 stayed empty, the auto-unlock service
# could not open it, and the operator was forced to run `braid
# enroll DIR` afterwards. Routing through `plan_single_disk_
# enrollment` makes the decision explicit and journaled. The
# SlotConflict phase additionally locks the documented refusal on
# the add path: that guarantee is otherwise exercised only on the
# replace path and at the shared helper, so an add-path regression
# that swallowed the helper's `Err` could break it while those
# tests stayed green.
#
# Scenario: a disk that was originally added without a keyfile is
# disconnected (`remove-missing`), then later replugged. Operator
# wants to re-add it with the same keyfile the rest of the pool
# uses; uses `--enroll DIR` to install slot 1 in the same shot. In
# the refusal case the returning disk's slot 1 already holds an
# unknown key (a previous owner's, say), so braid declines and tells
# the operator to clear it with `cryptsetup luksKillSlot` first.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(name, extra=""):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {name}=/dev/disk/by-id/virtio-{name} --passphrase-stdin --yes {extra}"
    )


def missing_devid():
    raw = machine.succeed("braid status --json")
    report = json.loads(raw)
    devids = report.get("missing_devids", [])
    assert len(devids) == 1, f"expected one missing devid, got {devids}: {raw}"
    return str(devids[0])


def make_disk3_missing_then_remove():
    """Lock the pool, mount degraded without disk3, then remove-missing.
    Leaves the on-disk LUKS header intact -- only the pool membership
    changes."""
    machine.succeed("umount /mnt/storage")
    machine.succeed("cryptsetup close braid-disk3")
    machine.succeed("mount -o degraded /dev/mapper/braid-disk1 /mnt/storage")
    devid = missing_devid()
    machine.succeed(f"braid remove-missing --missing-id {devid} --yes")


# --- Phase 0: build pool, generate keyfile ---

with subtest("Setup: build 3-disk pool, generate keyfile"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'recoverable enroll' > /mnt/storage/data.txt")
    machine.succeed("sync")

    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Phase 1: enroll keyfile on disk1+disk2 so the pool carries slot 1 ---

with subtest("Setup: enroll keyfile on disk1+disk2 only (asymmetry baseline)"):
    # `braid enroll` enrolls every present pool member, which at this
    # point includes disk3. Phase 2 needs disk3 to come back with slot
    # 1 empty, so reach for cryptsetup directly here and skip disk3.
    pq = shlex.quote(passphrase)
    for name in ["disk1", "disk2"]:
        machine.succeed(
            f"printf '%s' {pq} | "
            f"cryptsetup luksAddKey --batch-mode --key-file=- --key-slot 1 "
            f"{luks_opts} /dev/disk/by-id/virtio-{name} /tmp/braid.key"
        )
    # Sanity: disk3 still has slot 0 only.
    dump3 = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"1"' not in dump3, (
        f"disk3 setup leak: slot 1 should still be empty here:\n{dump3}"
    )

# --- Phase 2: NeedsEnroll path ---

with subtest("Make disk3 missing, then re-add with --enroll (NeedsEnroll)"):
    make_disk3_missing_then_remove()

    # Confirm disk3 still has slot 0 only -- we did not enroll
    # the keyfile on it before it left the pool.
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"0"' in dump, f"slot 0 missing from disk3 luksDump:\n{dump}"
    assert '"1"' not in dump, f"slot 1 should be empty on disk3:\n{dump}"

    machine.succeed(
        f"{add_cmd('disk3', '--enroll /tmp')} >/tmp/add1.out 2>/tmp/add1.err"
    )
    err = machine.succeed("cat /tmp/add1.err")

    enroll_wait = "[wait] disk disk3: enrolling keyfile in slot 1..."
    enroll_ok = "[ok]   disk disk3: keyfile enrolled in slot 1"
    assert enroll_wait in err, (
        f"NeedsEnroll path missing enroll [wait] row; got:\n{err}"
    )
    assert enroll_ok in err, (
        f"NeedsEnroll path missing enroll [ok] row; got:\n{err}"
    )

with subtest("Slot 1 occupied after first add --enroll"):
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"1"' in dump, f"slot 1 missing after add --enroll:\n{dump}"

    backup_dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata "
        "/var/lib/braid/luks-headers/braid-disk3.luksheader"
    )
    assert '"1"' in backup_dump, (
        f"slot 1 missing from header backup:\n{backup_dump}"
    )

# --- Phase 3: AlreadyEnrolled path (idempotent skip) ---

with subtest("Make disk3 missing again, then re-add (AlreadyEnrolled)"):
    make_disk3_missing_then_remove()

    # Disk3 still has slot 1 from Phase 2 -- this re-add should NOT
    # rerun luksAddKey.
    dump_before = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"1"' in dump_before, (
        f"slot 1 should still be set from Phase 2:\n{dump_before}"
    )

    machine.succeed(
        f"{add_cmd('disk3', '--enroll /tmp')} >/tmp/add2.out 2>/tmp/add2.err"
    )
    err = machine.succeed("cat /tmp/add2.err")

    # The probe happens via `probe_keyfile_enrollment`, which emits
    # `[wait] keyfile: checking against disk3` and on Authenticated
    # `[ok]   keyfile: already enrolled on disk3`. The enrollment
    # wait/ok rows must NOT appear -- that would mean the planner
    # re-enrolled instead of taking the idempotent skip.
    enroll_wait = "[wait] disk disk3: enrolling keyfile in slot 1..."
    assert enroll_wait not in err, (
        "AlreadyEnrolled path must NOT emit enroll [wait] row "
        f"(no luksAddKey should run); got:\n{err}"
    )

    # Slot 1 contents must not have been re-mutated. cryptsetup
    # luksDump prints a per-slot digest; if luksAddKey ran, the digest
    # changes. Comparing the full dump catches a regression that
    # silently re-enrolled.
    dump_after = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert dump_after == dump_before, (
        "AlreadyEnrolled idempotent skip must not mutate the LUKS header; "
        f"before:\n{dump_before}\nafter:\n{dump_after}"
    )

# --- Phase 4: SlotConflict path (unknown key in slot 1 -> refusal) ---

with subtest("Make disk3 missing again, poison slot 1 with an unknown key"):
    make_disk3_missing_then_remove()

    # Disk3 returns with slot 1 still authenticating /tmp/braid.key from
    # Phase 2 -- the header survives remove-missing. `cryptsetup
    # luksAddKey --key-slot 1` refuses a full slot, so clear the
    # inherited slot 1 before planting the foreign key.
    machine.succeed(
        "cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/virtio-disk3 1"
    )
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"1"' not in dump, (
        f"slot 1 must be cleared before planting the foreign key:\n{dump}"
    )

    # Plant an unknown key in slot 1 (distinct bytes from /tmp/braid.key),
    # authenticated with the pool passphrase -- "a previous owner left
    # something there". The --enroll keyfile /tmp/braid.key does not
    # authenticate this slot.
    machine.succeed(
        "dd if=/dev/urandom of=/tmp/foreign.key bs=4096 count=1 iflag=fullblock"
    )
    machine.succeed("chmod 400 /tmp/foreign.key")
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksAddKey --batch-mode --key-slot 1 --key-file=- {luks_opts} "
        f"/dev/disk/by-id/virtio-disk3 /tmp/foreign.key"
    )
    dump_before = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert '"1"' in dump_before, (
        f"slot 1 should be occupied by the foreign key:\n{dump_before}"
    )

with subtest("add --enroll refuses with luksKillSlot remediation (SlotConflict)"):
    exit_code, output = machine.execute(f"{add_cmd('disk3', '--enroll /tmp')} 2>&1")
    assert exit_code != 0, (
        f"add must refuse on slot-1 conflict; got exit {exit_code}, output:\n{output}"
    )
    assert "slot 1 on disk3" in output, (
        f"missing per-disk slot-1 wording; got:\n{output}"
    )
    assert "occupied by an unknown key" in output, (
        f"missing canonical occupancy wording; got:\n{output}"
    )
    assert "luksKillSlot" in output, (
        f"missing luksKillSlot remediation; got:\n{output}"
    )

with subtest("Refusal wrote no journal and did not mutate the pool"):
    machine.fail("test -f /var/lib/braid/pending-op.json")
    fi_show = machine.succeed("btrfs fi show /mnt/storage")
    assert "braid-disk3" not in fi_show, (
        f"disk3 must not be re-added on a refused enroll:\n{fi_show}"
    )

with subtest("Refusal left the unknown slot-1 key untouched"):
    # The contract is a refusal that preserves operator state: braid
    # must not wipe or replace the unknown slot-1 key, only decline and
    # point at `cryptsetup luksKillSlot`. Reuse the AlreadyEnrolled
    # phase's dump-equality idiom.
    dump_after = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk3"
    )
    assert dump_after == dump_before, (
        "SlotConflict refusal must not mutate the LUKS header; "
        f"before:\n{dump_before}\nafter:\n{dump_after}"
    )

machine.shutdown()
