# Intent: bare `braid discover` and `braid discover --write` refuse with a
#   non-zero exit and the no-members message, printing no preview rows, when
#   zero braid-labeled LUKS2 disks are attached; `--write` writes no pool.json.
# Why it exists: the members.is_empty() -> print_cli_error(NoMembersDiscovered)
#   -> exit(1) wiring in main.rs's Discover arm is the only discover refusal
#   with no end-to-end test. Its siblings are each driven through the real
#   binary in braid-discover.py, but that test always boots two labeled disks.
#   The unit test only checks NoMembersDiscovered.to_string(); the pool-lock
#   sentinel only asserts the string's ABSENCE under contention. A regression
#   that printed the empty preview and exited 0, dropped the refusal, or routed
#   it to stdout would pass every existing test.
# Scenario: an operator rebuilding a lost pool.json runs `braid discover` with
#   the array's disks momentarily detached/mislabeled and must get a clear
#   exit-1 refusal -- not a silent empty preview.

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

ERR = "error: no braid-labeled LUKS2 devices found"
POOL_JSON = "/var/lib/braid/pool.json"


def run_discover(label, command):
    """Assert one discover invocation refuses an empty scan: exit 1, no stdout
    preview, no-members message on stderr. Asserts internally; returns None."""
    rc, _ = machine.execute(command + " >/tmp/out 2>/tmp/err")
    out = machine.succeed("cat /tmp/out")
    err = machine.succeed("cat /tmp/err")
    # Exit 1 exactly, not just non-zero: a panic (rc 101) or a reroute to
    # another code (e.g. exit 2) must not pass as a clean refusal. The plain
    # redirect preserves braid's own exit status, so the precise code is
    # observable. The documented contract (docs/commands/discover.md) is "exits 1".
    assert rc == 1, label + ": expected exit 1 refusal; rc=" + str(rc) + " err=" + err
    assert out.strip() == "", label + ": printed preview rows on stdout:\n" + out
    assert ERR in err, label + ": missing no-members refusal on stderr:\n" + err


with subtest("precondition: no pool.json so bare discover reaches the scan"):
    # check_pool_json_for_bare_discover passes on a Missing pool.json, so bare
    # discover proceeds past the gate to the (empty) by-id scan.
    machine.succeed("test ! -e " + POOL_JSON)

with subtest("bare discover refuses empty scan with exit 1 and no preview"):
    run_discover("bare discover", "braid discover")

with subtest("discover --write hits the same gate and writes no pool.json"):
    run_discover("discover --write", "braid discover --write")
    machine.succeed("test ! -e " + POOL_JSON)
