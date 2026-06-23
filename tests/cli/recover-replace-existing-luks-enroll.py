# Test: recover replays enrollment for ExistingLuks replace target
#
# Intent: a crash mid-`braid replace --enroll DIR` against a
# `PresentLuks` new disk leaves the journal recording
# `ReplaceJournalMode::ExistingLuks { enroll_key_file: Some(kf) }`.
# `braid recover` must (1) probe the new disk and confirm the live
# LUKS UUID matches the journaled value, (2) verify the operator's
# passphrase, (3) replay `cryptsetup luksAddKey` + header backup,
# (4) save pre-replace pool.json, and (5) clear the journal.
#
# Why it exists: the executor-side correctness for the silent-drop
# bug fix in recovery. Pre-refactor, recovery just rolled back the
# membership without ever installing slot 1, so the disk shipped
# without the keyfile and auto-unlock could not open it.
#
# Scenario: operator runs `braid replace --old disk2 --new disk4
# --enroll /tmp` against a pre-formatted disk4 (slot 0 only). The
# braid process is killed between journal write and the live btrfs
# replace_start. Recovery resumes from the journal.

import json
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


def members_except(pool, *names):
    skip = set(names)
    return {
        uuid: member
        for uuid, member in pool["disks"].items()
        if member["name"] not in skip
    }


# --- Phase 0: build pool ---

with subtest("Setup: build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    machine.succeed("echo 'recover-enroll' > /mnt/storage/data.txt")
    machine.succeed("sync")
    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 1: pre-format disk4 ---

with subtest("Pre-format disk4 + generate keyfile"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )
    luks_uuid_disk4 = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Phase 2: inject crashed-replace journal ---

with subtest("Lock pool and inject ExistingLuks + enroll journal"):
    machine.succeed("braid lock")
    machine.fail("mountpoint -q /mnt/storage")

    old_uuid, old_member = member_entry(pool_json, "disk2")
    target_disks = members_except(pool_json, "disk2")
    target_disks[luks_uuid_disk4] = {
        "name": "disk4",
        "by_id": "/dev/disk/by-id/virtio-disk4",
    }
    target_json = {"disks": target_disks}

    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Replace",
            "phase": "PoolMutation",
            "old_uuid": old_uuid,
            "old_name": "disk2",
            "new_uuid": luks_uuid_disk4,
            "new_name": "disk4",
            "new_target": {
                "by_id": "/dev/disk/by-id/virtio-disk4",
                "mode": {
                    "ExistingLuks": {
                        "enroll_key_file": "/tmp/braid.key",
                    }
                },
            },
            "source": {
                "Live": {
                    "old_devid": old_member["devid"],
                    "old_mapper": "braid-disk2",
                }
            },
            "restore_raid1_after_commit": False,
        },
        "pre_membership": pool_json,
        "target_membership": target_json,
    }
    journal_str = json.dumps(journal)
    machine.succeed(
        f"cat > /var/lib/braid/pending-op.json << 'JOURNAL_EOF'\n"
        f"{journal_str}\n"
        f"JOURNAL_EOF"
    )

# --- Phase 3: braid recover replays enrollment + rolls back membership ---

with subtest("braid recover replays addKey + backup, then clears journal"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin --allow-degraded "
        f">/tmp/recover.out 2>/tmp/recover.err"
    )

    # Disk4's slot 1 must be populated by the replay.
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk4"
    )
    assert '"1"' in dump, (
        f"recovery did not enroll slot 1 on disk4:\n{dump}"
    )

    # Header backup must capture the post-enroll header.
    backup_dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata "
        "/var/lib/braid/luks-headers/braid-disk4.luksheader"
    )
    assert '"1"' in backup_dump, (
        f"recovery did not back up the post-enroll header:\n{backup_dump}"
    )

    # LUKS UUID must be unchanged -- recovery must NOT re-format.
    luks_uuid_after = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert luks_uuid_after == luks_uuid_disk4, (
        f"LUKS UUID changed -- recovery re-formatted!"
        f" before={luks_uuid_disk4} after={luks_uuid_after}"
    )

with subtest("Pool rolls back to pre-replace topology"):
    recovered = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))
    assert {m["name"] for m in recovered["disks"].values()} == {
        "disk1",
        "disk2",
        "disk3",
    }, recovered
    machine.fail("test -f /var/lib/braid/pending-op.json")

with subtest("keyfile unlocks the rolled-back pool (after enroll on rest)"):
    # Enroll the rest of the pool to validate end-to-end auto-unlock
    # would work after the operator re-runs the replace.
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"
    )
    machine.succeed(
        f"printf '%s\\n' {pq} | braid enroll /tmp --passphrase-stdin"
    )
    machine.succeed("braid lock")

    # disk4 is not in the pool yet (replace was rolled back), but its
    # slot 1 still authenticates the keyfile.
    machine.succeed(
        "cryptsetup open --type luks --test-passphrase "
        "--key-file /tmp/braid.key /dev/disk/by-id/virtio-disk4"
    )

machine.shutdown()
