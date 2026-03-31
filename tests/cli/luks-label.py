# Test: luks-label
#
# What: After `braid add disk1`, the LUKS2 volume must carry the label
# "braid-disk1" so that luksDump / blkid can identify it by braid name.
#
# Why: Without a label, an operator inspecting raw drives during recovery
# has no way to map a LUKS device back to its braid disk name.
#
# Scenario: Operator runs `cryptsetup luksDump /dev/sda` on a pulled drive
# and expects to see "Label: braid-disk1".

# import json

start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"

with subtest("braid add disk1 formats the volume"):
    machine.succeed(
        f"echo -n '{passphrase}' | braid add disk1=/dev/disk/by-id/virtio-disk1 --passphrase-stdin --yes"
    )

with subtest("LUKS label is braid-disk1"):
    dump = machine.succeed("cryptsetup luksDump /dev/disk/by-id/virtio-disk1")
    # luksDump prints "Label:" followed by the label value (or "(no label)")
    found = False
    for line in dump.splitlines():
        if line.strip().startswith("Label:"):
            label = line.split(":", 1)[1].strip()
            assert label == "braid-disk1", (
                f"expected LUKS label 'braid-disk1', got '{label}'"
            )
            found = True
            break
    assert found, f"no Label: line found in luksDump output:\n{dump}"

machine.shutdown()
