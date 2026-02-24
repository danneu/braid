start_all()

machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("braid.socket")

with subtest("socket file exists"):
    machine.succeed("test -S /run/braid/daemon.sock")

with subtest("ping returns ok"):
    result = machine.succeed(
        "echo '{\"method\":\"ping\"}' | socat - UNIX-CONNECT:/run/braid/daemon.sock"
    )
    assert '"status":"ok"' in result, f"unexpected response: {result}"

with subtest("unknown method returns error"):
    result = machine.succeed(
        "echo '{\"method\":\"bogus\"}' | socat - UNIX-CONNECT:/run/braid/daemon.sock"
    )
    assert '"error":"unknown method: bogus"' in result, f"unexpected response: {result}"

with subtest("invalid json returns error"):
    result = machine.succeed(
        "echo 'not json' | socat - UNIX-CONNECT:/run/braid/daemon.sock"
    )
    assert '"error":"invalid json"' in result, f"unexpected response: {result}"
