start_all()

machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("btrnasd.service")

with subtest("socket file exists"):
    machine.succeed("test -S /run/btrnas/daemon.sock")

with subtest("ping returns ok"):
    result = machine.succeed(
        "echo '{\"method\":\"ping\"}' | socat - UNIX-CONNECT:/run/btrnas/daemon.sock"
    )
    assert '"status":"ok"' in result, f"unexpected response: {result}"

with subtest("unknown method returns error"):
    result = machine.succeed(
        "echo '{\"method\":\"bogus\"}' | socat - UNIX-CONNECT:/run/btrnas/daemon.sock"
    )
    assert '"error":"unknown method: bogus"' in result, f"unexpected response: {result}"
