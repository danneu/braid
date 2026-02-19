start_all()
machine.wait_for_unit("multi-user.target")

with subtest("VM booted successfully"):
    machine.succeed("uname -a")

with subtest("Virtual drives are present"):
    machine.succeed("test -b /dev/disk/by-id/virtio-disk1")
    machine.succeed("test -b /dev/disk/by-id/virtio-disk2")
    machine.succeed("test -b /dev/disk/by-id/virtio-disk3")

machine.shutdown()
