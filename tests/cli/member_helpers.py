# Shared helpers for VM tests that inspect braid's pool.json membership.
#
# This file is NOT a Python module. NixOS VM tests run their `testScript`
# as a single string passed to the test runner's Python interpreter; there
# is no module path on the runner so a sibling Python file cannot be
# `import`ed. Each consumer's `.nix` file concatenates this file at
# Nix-eval time so these definitions land in the test script's global
# namespace before the test code runs:
#
#   testScript = builtins.readFile ./member_helpers.py
#     + "\n\n"
#     + builtins.readFile ./<test-name>.py;


def member_names(pool):
    return {member["name"] for member in pool["disks"].values()}


def member(pool, name):
    for entry in pool["disks"].values():
        if entry["name"] == name:
            return entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def member_entry(pool, name):
    # Decision 024: the dict key is the member's persistent LUKS UUID identity,
    # distinct from the value-side display name.
    for luks_uuid, entry in pool["disks"].items():
        if entry["name"] == name:
            return luks_uuid, entry
    raise AssertionError(f"{name} missing from pool.json: {pool}")


def member_uuid(pool, name):
    uuid, _ = member_entry(pool, name)
    return uuid


def assert_member_keyed_by_uuid(pool, name, expected_uuid):
    # Pin Decision 024 end-to-end: the member named `name` is stored under the
    # object key `expected_uuid` -- the disk's real, live cryptsetup LUKS UUID.
    key = member_uuid(pool, name)
    assert key == expected_uuid, (
        f"member '{name}' keyed by {key}, expected live LUKS UUID {expected_uuid}: {pool}"
    )
