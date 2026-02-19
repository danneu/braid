start_all()
machine.wait_for_unit("multi-user.target", timeout=180)

with subtest("Boot completed despite dead drive"):
    machine.succeed("systemctl is-active multi-user.target")

with subtest("/mnt/storage is NOT mounted"):
    machine.fail("mountpoint /mnt/storage")

with subtest("System is functional"):
    machine.succeed("echo 'system works' > /tmp/test.txt")
    content = machine.succeed("cat /tmp/test.txt").strip()
    assert content == "system works", f"Expected 'system works', got '{content}'"

machine.shutdown()
