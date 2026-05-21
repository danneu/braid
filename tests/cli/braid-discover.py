# Intent: verify braid discover's recovery workflow end-to-end.
#
# Why it exists: discover is the sole way to rebuild pool.json from labeled
# drives; a regression here leaves users unable to recover a lost pool config.
#
# Scenario: user reinstalled NixOS on their NAS; pool.json is gone or corrupt
# but drives retain their braid LUKS labels. They run discover to reconstruct
# pool.json, then verify the recovered config actually unlocks the pool.

import json
import shlex


POOL_JSON = "/var/lib/braid/pool.json"
DISK_UUIDS = {
    "disk1": "11111111-1111-1111-1111-111111111111",
    "disk2": "22222222-2222-2222-2222-222222222222",
}


def write_pool_json(contents):
    """Overwrite /var/lib/braid/pool.json with literal contents."""
    machine.succeed("mkdir -p /var/lib/braid")
    machine.succeed("printf '%s' " + shlex.quote(contents) + " > " + POOL_JSON)


def read_pool_json():
    """Return the literal pool.json contents."""
    return machine.succeed("cat " + POOL_JSON)


def assert_pool_json_absent():
    """Assert discover has not created pool.json."""
    machine.fail("test -e " + POOL_JSON)


def assert_no_corrupt_sidecars():
    """Assert no forensic corrupt sidecar exists."""
    machine.fail("ls /var/lib/braid/pool.json.corrupt-*")


def corrupt_sidecars():
    """Return forensic corrupt sidecar paths."""
    return machine.succeed("ls -1 /var/lib/braid/pool.json.corrupt-*").splitlines()


def assert_corrupt_preview_refuses(contents):
    """Assert corrupt/off-schema pool.json gets the rebuild remediation."""
    write_pool_json(contents)
    before = read_pool_json()
    out = machine.fail("braid discover 2>&1")
    assert (
        "is corrupt or unreadable -- run 'braid discover --write' "
        "to rebuild from existing disks"
    ) in out, (
        "expected corrupt pool.json rebuild remediation; got:\n" + out
    )
    assert read_pool_json() == before, "pool.json must be byte-for-byte unchanged"


def assert_uuid_keyed_pool_json():
    """Assert pool.json has the expected UUID-keyed two-disk membership."""
    pool = json.loads(read_pool_json())
    assert set(pool["disks"].keys()) == set(DISK_UUIDS.values()), (
        "unexpected UUID-keyed disk set: " + json.dumps(pool, sort_keys=True)
    )
    for name, luks_uuid in DISK_UUIDS.items():
        member = pool["disks"][luks_uuid]
        assert member["name"] == name, (
            "expected " + luks_uuid + " to carry name " + name + ": " + json.dumps(pool)
        )
        assert member["by_id"] == "/dev/disk/by-id/virtio-" + name, (
            "expected by-id for " + name + ": " + json.dumps(pool)
        )


def non_uuid_keyed_pool_json():
    """Return a non-UUID-keyed pool.json payload matching the fixture."""
    members = {}
    for index, name in enumerate(["disk1", "disk2"], start=1):
        members[name] = {
            "name": name,
            "by_id": "/dev/disk/by-id/virtio-" + name,
            "devid": index,
        }
    return json.dumps({"disks": members}, sort_keys=True, separators=(",", ":"))


start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

with subtest("discover lists labeled disks and prints write hint"):
    out = machine.succeed("braid discover 2>&1")
    assert "disk1" in out, "expected disk1 in discover output"
    assert "disk2" in out, "expected disk2 in discover output"
    assert "pass --write to save to /var/lib/braid/pool.json" in out, "expected write hint"

with subtest("discover without --write does not create pool.json"):
    assert_pool_json_absent()

with subtest("parseable off-schema preview refuses and leaves pool.json unchanged"):
    # Intent: bare discover refuses parseable but off-schema pool.json
    # with rebuild guidance and makes no changes.
    # Why it exists: preview mode must not overwrite possibly forensic
    # state just because the file is invalid.
    # Scenario: an operator or failed experiment wrote unrelated JSON
    # to /var/lib/braid/pool.json.
    assert_corrupt_preview_refuses('{"unexpected":true}')

with subtest("unparseable preview refuses and leaves pool.json unchanged"):
    # Intent: bare discover refuses unparseable pool.json with rebuild
    # guidance and makes no changes.
    # Why it exists: truncated state files must route to the explicit
    # rebuild workflow instead of continuing as if no state exists.
    # Scenario: power loss leaves /var/lib/braid/pool.json as non-JSON
    # bytes while the labeled disks are still attached.
    assert_corrupt_preview_refuses("not-json-at-all")

with subtest("non-UUID-keyed file is treated as corrupt during preview"):
    # Intent: bare discover treats non-UUID-keyed pool.json as corrupt,
    # not as a separately hinted format.
    # Why it exists: old format detection was removed; all non-current
    # membership shapes now share the corrupt-state remediation.
    # Scenario: an obsolete state file is still present when the
    # operator previews discover.
    payload = non_uuid_keyed_pool_json()
    write_pool_json(payload)
    before = read_pool_json()
    out = machine.fail("braid discover 2>&1")
    assert (
        "is corrupt or unreadable -- run 'braid discover --write' "
        "to rebuild from existing disks"
    ) in out, (
        "expected corrupt pool.json rebuild remediation; got:\n" + out
    )
    assert "detected" not in out, "unexpected special-case hint in output:\n" + out
    assert read_pool_json() == before, "pool.json must be byte-for-byte unchanged"

with subtest("expect-count mismatch refuses and writes nothing"):
    # Intent: discover --write refuses both low and high expected counts
    # before writing pool.json or a corrupt sidecar.
    # Why it exists: expected-count must be a fail-closed guard for
    # detached intended members and stray braid-labeled disks.
    # Scenario: operator knows the pool should have two members, but
    # passes an impossible count while rebuilding corrupt state.
    corrupt = '{"unexpected":true}'
    write_pool_json(corrupt)
    for expected in [1, 3]:
        out = machine.fail("braid discover --write --expect-count " + str(expected) + " 2>&1")
        assert "expected exactly " + str(expected) + " members, found 2" in out, (
            "expected count mismatch refusal in output:\n" + out
        )
        assert read_pool_json() == corrupt, "pool.json must be byte-for-byte unchanged"
        assert_no_corrupt_sidecars()

with subtest("expect-count exact match rebuilds corrupt pool.json with sidecar"):
    # Intent: discover --write with the exact expected count rebuilds
    # corrupt pool.json, writes UUID-keyed membership, and snapshots the
    # original bytes first.
    # Why it exists: corrupt-state rebuild is the supported recovery
    # path and must preserve forensic material before overwrite.
    # Scenario: all intended disks are attached and the operator can
    # name the expected two-member count ahead of time.
    corrupt = "not-json-at-all"
    write_pool_json(corrupt)
    out = machine.succeed("braid discover --write --expect-count 2 2>&1")
    assert "pool membership written to /var/lib/braid/pool.json" in out, (
        "expected rebuild success in output:\n" + out
    )
    assert_uuid_keyed_pool_json()
    sidecars = corrupt_sidecars()
    assert len(sidecars) == 1, "expected exactly one corrupt sidecar: " + str(sidecars)
    sidecar_raw = machine.succeed("cat " + shlex.quote(sidecars[0]))
    assert sidecar_raw == corrupt, "sidecar must preserve the corrupt original bytes"

with subtest("rebuilt pool.json is usable -- unlock succeeds"):
    # Intent: the rebuilt pool.json can actually unlock and mount the
    # pool, rather than merely passing JSON shape checks.
    # Why it exists: running unlock immediately after rebuild proves the
    # file written by discover is sufficient for real recovery.
    # Scenario: user rebuilds missing/corrupt state and then brings the
    # NAS storage pool online.
    machine.succeed("echo -n 'testpassphrase' | braid unlock --passphrase-stdin")
    machine.succeed("mountpoint /mnt/storage")

with subtest("bare discover refuses existing UUID-keyed pool.json"):
    out = machine.fail("braid discover 2>&1")
    assert "pool.json already exists at /var/lib/braid/pool.json" in out, (
        "expected existing pool.json refusal; got:\n" + out
    )
    # Pin the two load-bearing clauses of the bare-mode refusal text.
    assert "live discovery is not authoritative once pool.json exists" in out, (
        "expected authority-principle clause; got:\n" + out
    )
    assert "rebuilding missing or corrupt pool state" in out, (
        "expected command-purpose clause; got:\n" + out
    )

with subtest("discover --write also refuses healthy UUID-keyed pool.json"):
    before = read_pool_json()
    out = machine.fail("braid discover --write 2>&1")
    assert "is already a healthy UUID-keyed membership" in out, (
        "expected ValidUuidKeyed refusal; got:\n" + out
    )
    after = read_pool_json()
    assert before == after, "pool.json must be byte-for-byte unchanged"

machine.shutdown()
