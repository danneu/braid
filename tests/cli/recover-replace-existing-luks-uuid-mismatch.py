# Test: recover refuses on LUKS UUID mismatch + preserves journal
#
# Intent: when the journaled `ReplaceJournalMode::ExistingLuks`
# carries a `luks_uuid` that no longer matches the live disk's UUID
# (i.e. the operator swapped the disk between crash and recover),
# `braid recover` must refuse, preserve the journal, and emit the
# canonical "preserving pending-op.json" wording. No LUKS slot
# mutation may run.
#
# Why it exists: the defensive identity probe in the ExistingLuks
# recovery arm. Pre-refactor, recovery had no probe and would happily
# roll back even if the user replugged a different disk; that would
# silently rewrite pool.json against the wrong target. Routing the
# probe through `probe_config_disk` and matching the journaled UUID
# blocks that.
#
# Scenario: operator crashes mid-`replace --enroll DIR`, then
# accidentally swaps the new disk for a different (re-formatted)
# disk before running `braid recover`.

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


# --- Phase 0: build pool ---

with subtest("Setup: build 3-disk pool"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed(add_cmd("disk3"))
    pool_json = json.loads(machine.succeed("cat /var/lib/braid/pool.json"))

# --- Phase 1: pre-format disk4 with a UUID that we'll record in journal ---

with subtest("Pre-format disk4 to capture an initial LUKS UUID"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )
    journaled_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert journaled_uuid != "", "expected non-empty UUID"

    machine.succeed("dd if=/dev/urandom of=/tmp/braid.key bs=4096 count=1 iflag=fullblock")
    machine.succeed("chmod 400 /tmp/braid.key")

# --- Phase 2: simulate "operator swapped disk4 for a different LUKS disk" ---
#
# Re-format disk4 in place: same physical slot, different LUKS UUID.
# This is what an operator-swapped-the-wrong-disk situation looks like
# from braid's perspective (the journal records the old UUID, the live
# disk has a new one).

with subtest("Re-format disk4 -> new LUKS UUID (different from journal)"):
    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | "
        f"cryptsetup luksFormat --batch-mode --key-file=- {luks_opts} /dev/disk/by-id/virtio-disk4"
    )
    new_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk4"
    ).strip()
    assert new_uuid != journaled_uuid, (
        f"re-format should produce a different UUID; old={journaled_uuid} new={new_uuid}"
    )

# --- Phase 3: lock pool, inject crashed-replace journal with the OLD UUID ---

with subtest("Lock pool and inject ExistingLuks + enroll journal (mismatched UUID)"):
    machine.succeed("braid lock")

    target_disks = {}
    for name, member in pool_json["disks"].items():
        if name != "disk2":
            target_disks[name] = member
    target_disks["disk4"] = {"by_id": "/dev/disk/by-id/virtio-disk4"}
    target_json = {"disks": target_disks}

    journal = {
        "started_at": "2026-01-01T00:00:00Z",
        "op": {
            "op": "Replace",
            "phase": "PoolMutation",
            "old_name": "disk2",
            "new_name": "disk4",
            "new_target": {
                "by_id": "/dev/disk/by-id/virtio-disk4",
                "mapper_name": "braid-disk4",
                "mode": {
                    "ExistingLuks": {
                        "luks_uuid": journaled_uuid,
                        "enroll_key_file": "/tmp/braid.key",
                    }
                },
            },
            "source": {
                "Live": {
                    "old_devid": pool_json["disks"]["disk2"]["devid"],
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

# --- Phase 4: braid recover refuses ---

with subtest("braid recover refuses with UUID-mismatch + preserves journal"):
    pq = shlex.quote(passphrase)
    exit_code, output = machine.execute(
        f"printf '%s\\n' {pq} | braid recover --passphrase-stdin --allow-degraded 2>&1"
    )
    assert exit_code != 0, (
        f"recover must refuse on UUID mismatch; got exit {exit_code}, output:\n{output}"
    )
    assert "LUKS UUID mismatch" in output, (
        f"missing canonical UUID-mismatch wording; got:\n{output}"
    )
    assert "preserving pending-op.json" in output, (
        f"missing journal-preservation remediation; got:\n{output}"
    )

with subtest("Journal preserved, no slot mutation, no membership write"):
    machine.succeed("test -f /var/lib/braid/pending-op.json")

    # Disk4's slot 1 must still be empty -- recovery refused before
    # any LUKS mutation.
    dump = machine.succeed(
        "cryptsetup luksDump --dump-json-metadata /dev/disk/by-id/virtio-disk4"
    )
    assert '"1"' not in dump, (
        f"slot 1 should not have been enrolled on UUID-mismatch refusal:\n{dump}"
    )

machine.shutdown()
