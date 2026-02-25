start_all()
machine.wait_for_unit("multi-user.target")

passphrase = "testpassphrase"
disks = ["disk1", "disk2", "disk3"]

with subtest("LUKS format and open all drives"):
    for name in disks:
        dev = f"/dev/disk/by-id/virtio-{name}"
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksFormat --batch-mode --key-file=- --pbkdf pbkdf2 --pbkdf-force-iterations 1000 {dev}")
        machine.succeed(f"echo -n '{passphrase}' | cryptsetup luksOpen --key-file=- {dev} {name}")

with subtest("Mapper devices exist and are block devices"):
    for name in disks:
        machine.succeed(f"test -b /dev/mapper/{name}")

with subtest("LUKS devices report as active"):
    for name in disks:
        machine.succeed(f"cryptsetup status {name}")

machine.shutdown()
