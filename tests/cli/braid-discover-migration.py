# Intent: verify the legacy name-keyed pool.json migration runbook.
#
# Why it exists: the LUKS-UUID identity cutover needs a safe preview path,
# exact member-count writes, and a UUID-keyed result that can unlock a real
# multi-disk fixture.
#
# Scenario: an operator boots a three-disk NAS with an old name-keyed
# pool.json, previews discover, moves the file aside, rejects wrong expected
# counts, writes the new membership, and confirms normal unlock.

import json
import shlex


POOL_JSON = "/var/lib/braid/pool.json"
LEGACY_POOL_JSON = "/var/lib/braid/pool.json.legacy"

DISK_UUIDS = {
    "disk1": "11111111-1111-1111-1111-111111111111",
    "disk2": "22222222-2222-2222-2222-222222222222",
    "disk3": "33333333-3333-3333-3333-333333333333",
}


def write_pool_json(contents):
    """Overwrite /var/lib/braid/pool.json with literal contents."""
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed(f"printf '%s' {shlex.quote(contents)} > {POOL_JSON}")


def read_pool_json():
    """Return the literal pool.json contents."""
    return machine.succeed(f"cat {POOL_JSON}")


def assert_pool_json_absent():
    """Assert discover has not created pool.json."""
    machine.fail(f"test -e {POOL_JSON}")


def assert_existing_pool_json_refuses_preview(contents, label):
    """Assert unrecognized existing pool.json shapes keep the old refusal."""
    with subtest(label):
        write_pool_json(contents)
        before = read_pool_json()
        out = machine.fail("braid discover 2>&1")
        assert "pool.json already exists at /var/lib/braid/pool.json" in out, (
            "expected existing pool.json refusal; got:\n" + out
        )
        assert read_pool_json() == before, "pool.json must be byte-for-byte unchanged"


def legacy_pool_json():
    """Return a legacy name-keyed pool.json payload matching the fixture."""
    members = {}
    for index, name in enumerate(["disk1", "disk2", "disk3"], start=1):
        members[name] = {
            "by_id": "/dev/disk/by-id/virtio-" + name,
            "luks_uuid": DISK_UUIDS[name],
            "devid": index,
            "added_at": "2024-01-01T00:00:00Z",
        }
    return json.dumps({"disks": members}, sort_keys=True, separators=(",", ":"))


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("setup: seed legacy name-keyed pool.json"):
    assert_pool_json_absent()
    write_pool_json(legacy_pool_json())
    legacy_raw = read_pool_json()

with subtest("bare discover previews legacy shape and leaves pool.json unchanged"):
    out = machine.succeed("braid discover 2>&1")
    for name in ["disk1", "disk2", "disk3"]:
        assert name in out, "expected " + name + " in discover output"
    assert "legacy name-keyed pool.json detected" in out, (
        "expected migration hint in output:\n" + out
    )
    assert "braid discover --write --expect-count" in out, (
        "expected expect-count hint in output:\n" + out
    )
    assert read_pool_json() == legacy_raw, "pool.json must be byte-for-byte unchanged"

with subtest("discover --write refuses legacy shape"):
    out = machine.fail("braid discover --write 2>&1")
    assert "is not in UUID-keyed format" in out, (
        "expected UUID-keyed format refusal in output:\n" + out
    )
    assert read_pool_json() == legacy_raw, "pool.json must be byte-for-byte unchanged"

with subtest("operator moves legacy pool.json aside"):
    machine.succeed(f"mv {POOL_JSON} {LEGACY_POOL_JSON}")
    assert_pool_json_absent()

with subtest("discover --write refuses over-count mismatch"):
    out = machine.fail("braid discover --write --expect-count 2 2>&1")
    assert "expected exactly 2 members, found 3" in out, (
        "expected over-count refusal in output:\n" + out
    )
    assert_pool_json_absent()

with subtest("discover --write refuses under-count mismatch"):
    out = machine.fail("braid discover --write --expect-count 4 2>&1")
    assert "expected exactly 4 members, found 3" in out, (
        "expected under-count refusal in output:\n" + out
    )
    assert_pool_json_absent()

with subtest("discover --write accepts exact count and writes UUID-keyed pool.json"):
    out = machine.succeed("braid discover --write --expect-count 3 2>&1")
    assert "pool membership written to /var/lib/braid/pool.json" in out, (
        "expected write success in output:\n" + out
    )
    pool = json.loads(read_pool_json())
    assert set(pool["disks"].keys()) == set(DISK_UUIDS.values()), (
        "unexpected UUID-keyed disk set: " + json.dumps(pool, sort_keys=True)
    )
    for name, uuid in DISK_UUIDS.items():
        assert pool["disks"][uuid]["name"] == name, (
            "expected " + uuid + " to carry name " + name + ": " + json.dumps(pool)
        )

with subtest("new UUID-keyed pool.json is usable"):
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("bare discover refuses new UUID-keyed pool.json"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out, (
        "expected existing pool.json refusal; got:\n" + out
    )

assert_existing_pool_json_refuses_preview(
    '{"unexpected":true}',
    "bare discover refuses parseable unrecognized pool.json",
)

assert_existing_pool_json_refuses_preview(
    "not-json-at-all",
    "bare discover refuses unparseable pool.json",
)

machine.shutdown()
