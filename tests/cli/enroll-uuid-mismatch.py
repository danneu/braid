# Intent: braid enroll refuses with non-zero exit when any membership
#   disk's live LUKS UUID at its by-id path no longer matches the UUID
#   captured in pool.json, before slot 1 is mutated on any disk.
# Why it exists: decision-024 mandates UUID re-checks at every mutation
#   boundary. mount/replace/recover already enforce this; enroll did
#   not, so a swapped or reformatted disk would silently take the
#   operator's keyfile into slot 1 of a foreign LUKS container while
#   the real member's slot 1 stays empty -- breaking auto-unlock.
# Scenario: operator sets up a 2-disk braid pool, locks it, then
#   reformats one member out-of-band (or hot-swaps a foreign LUKS disk
#   onto the same by-id slot). The next `braid enroll --generate` must
#   abort before any slot 1 mutation and surface the same wording shape
#   as `braid unlock` does for the same scenario.

import json
import shlex


def member_uuid(pool, name):
    for uuid, entry in pool["disks"].items():
        if entry["name"] == name:
            return uuid
    raise AssertionError(f"{name} missing from pool.json: {pool}")


start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"


def add_cmd(key):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )


def unlock_cmd():
    pq = shlex.quote(passphrase)
    return f"printf '%s\\n' {pq} | braid unlock --passphrase-stdin"


def enroll_cmd(extra_args):
    pq = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {pq} | "
        f"braid enroll /tmp/usb --generate {extra_args} --passphrase-stdin"
    )


def close_all():
    machine.execute("umount /mnt/storage 2>/dev/null || true")
    for key in ["disk1", "disk2"]:
        machine.execute(f"cryptsetup close braid-{key} 2>/dev/null || true")


def assert_slot1_empty(device):
    dump = machine.succeed(f"cryptsetup luksDump --dump-json-metadata {device}")
    assert '"1"' not in dump, f"slot 1 should be empty on {device}:\n{dump}"


def assert_mismatch_output(output, old_uuid, new_uuid):
    assert "LUKS UUID mismatch" in output, (
        f"expected UUID mismatch in output, got: {output}"
    )
    assert "detach the foreign disk" in output, (
        f"expected remediation hint in output, got: {output}"
    )
    assert "braid replace" in output, (
        f"expected replacement command in output, got: {output}"
    )
    assert "disk2" in output, f"expected disk2 in output, got: {output}"
    assert old_uuid in output, (
        f"expected original UUID {old_uuid} in output, got: {output}"
    )
    assert new_uuid in output, f"expected new UUID {new_uuid} in output, got: {output}"


with subtest("Setup: create pool and reformat disk2 behind pool.json"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))

    close_all()
    machine.succeed(unlock_cmd())
    machine.succeed("mountpoint -q /mnt/storage")

    pool_raw = machine.succeed("cat /var/lib/braid/pool.json")
    pool = json.loads(pool_raw)
    old_uuid = member_uuid(pool, "disk2")

    close_all()

    pq = shlex.quote(passphrase)
    machine.succeed(
        f"printf '%s' {pq} | cryptsetup luksFormat --batch-mode --key-file=- "
        f"{luks_opts} /dev/disk/by-id/virtio-disk2"
    )
    new_uuid = machine.succeed(
        "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2"
    ).strip()
    assert new_uuid != old_uuid, (
        f"new disk2 UUID should differ from pool.json UUID: {new_uuid}"
    )

    machine.succeed("mkdir -p /tmp/usb")
    machine.succeed("mount -t tmpfs -o size=1m,mode=700 tmpfs /tmp/usb")
    machine.succeed("mountpoint -q /tmp/usb")


with subtest("Dry-run rejects mismatch before previewing enrollment"):
    status, output = machine.execute(enroll_cmd("--dry-run") + " 2>&1")
    assert status != 0, "expected non-zero exit for dry-run UUID mismatch"
    assert_mismatch_output(output, old_uuid, new_uuid)
    assert "enroll keyfile ->" not in output, (
        f"dry-run must not preview enrollment after mismatch:\n{output}"
    )
    machine.fail("test -f /tmp/usb/braid.key")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk1")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk2")


with subtest("Real run rejects mismatch before creating keyfile or mutating slots"):
    status, output = machine.execute(enroll_cmd("") + " 2>&1")
    assert status != 0, "expected non-zero exit for real-run UUID mismatch"
    assert_mismatch_output(output, old_uuid, new_uuid)
    machine.fail("test -f /tmp/usb/braid.key")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk1")
    assert_slot1_empty("/dev/disk/by-id/virtio-disk2")


machine.shutdown()
