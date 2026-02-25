start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Module is inert when disabled"):
    machine.succeed("uname -a")
    machine.fail("mountpoint /mnt/storage")

machine.shutdown()
