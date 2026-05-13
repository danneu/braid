# Intent: braid discover prints members in DiskName order, even when
# their LUKS UUIDs sort opposite to their names.
#
# Why it exists: decision 024 requires operator-visible output to use
# DiskName order. Helper unit tests do not exercise the binary wiring.
#
# Scenario: two LUKS-labeled disks where UUID order is opposite name
# order; operator previews the discovered pool membership.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

out = machine.succeed("braid discover 2>&1")
names = [
    line.strip().split(" = ")[0]
    for line in out.splitlines()
    if " = /dev/disk/by-id/" in line
]
assert names == ["alpha", "zeta"], (
    "discover output must be in DiskName order, got: " + str(names)
)

machine.shutdown()
