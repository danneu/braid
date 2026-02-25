start_all()

# Boot will take a while — LUKS units wait 10s for devices that never appear.
machine.wait_for_unit("multi-user.target", timeout=180)

with subtest("Boot completed despite missing drives"):
    # multi-user.target reached means boot didn't hang
    machine.succeed("systemctl is-active multi-user.target")

with subtest("/mnt/storage is NOT mounted"):
    machine.fail("mountpoint /mnt/storage")

with subtest("System is functional"):
    machine.succeed("echo 'system works' > /tmp/test.txt")
    content = machine.succeed("cat /tmp/test.txt").strip()
    assert content == "system works", f"Expected 'system works', got '{content}'"

machine.shutdown()
