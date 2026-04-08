# Shared helpers for VM tests that observe braid's sleep inhibitor.
#
# This file is NOT a Python module. NixOS VM tests run their `testScript`
# as a single string passed to the test runner's Python interpreter; there
# is no module path on the runner so a sibling Python file cannot be
# `import`ed. Each consumer's `.nix` file concatenates this file at
# Nix-eval time so these definitions land in the test script's global
# namespace before the test code runs:
#
#   testScript = builtins.readFile ./inhibitor_helpers.py
#     + "\n\n"
#     + builtins.readFile ./<test-name>.py;
#
# `machine` is referenced inside list_inhibitors but resolved at call time,
# so it just needs to exist in the global scope when the function is
# invoked (after `start_all()` runs in the consumer test script).

import shlex


def list_inhibitors():
    # Query logind directly via D-Bus. We avoid `systemd-inhibit --list`
    # because it depends on TTY/terminal context that NixOS VM tests do not
    # provide.
    #
    # ListInhibitors returns a(ssssuu) — an array of (what, who, why, mode,
    # uid, pid) tuples. busctl's default text output renders this as:
    #
    #   a(ssssuu) <count> "what1" "who1" "why1" "mode1" uid1 pid1 ...
    #
    # Strings containing spaces (e.g. "replace in progress") are
    # double-quoted, so shlex.split parses them correctly.
    #
    # Defensive parsing: assert the expected token shape before indexing
    # so a busctl format change fails loudly with a clear message instead
    # of an opaque IndexError or ValueError on a downstream test assert.
    out = machine.succeed(
        "busctl call org.freedesktop.login1 /org/freedesktop/login1 "
        "org.freedesktop.login1.Manager ListInhibitors"
    ).strip()
    tokens = shlex.split(out)
    assert len(tokens) >= 2, (
        f"busctl ListInhibitors output too short to parse: {out!r}"
    )
    assert tokens[0] == "a(ssssuu)", (
        f"busctl ListInhibitors returned unexpected type signature "
        f"{tokens[0]!r} (expected 'a(ssssuu)'). Output: {out!r}"
    )
    try:
        count = int(tokens[1])
    except ValueError as e:
        raise AssertionError(
            f"busctl ListInhibitors count token {tokens[1]!r} is not an int. "
            f"Output: {out!r}"
        ) from e
    expected_token_count = 2 + count * 6
    assert len(tokens) == expected_token_count, (
        f"busctl ListInhibitors token count {len(tokens)} does not match "
        f"expected {expected_token_count} for {count} inhibitor(s) "
        f"(2 header + 6-tuple per entry). Output: {out!r}"
    )
    inhibitors = []
    for i in range(count):
        base = 2 + i * 6
        try:
            uid = int(tokens[base + 4])
            pid = int(tokens[base + 5])
        except ValueError as e:
            raise AssertionError(
                f"busctl ListInhibitors uid/pid tokens at entry {i} are not "
                f"ints: {tokens[base + 4]!r} / {tokens[base + 5]!r}. "
                f"Output: {out!r}"
            ) from e
        inhibitors.append({
            "what": tokens[base],
            "who": tokens[base + 1],
            "why": tokens[base + 2],
            "mode": tokens[base + 3],
            "uid": uid,
            "pid": pid,
        })
    return inhibitors


def find_braid_sleep_inhibitor(inhibitors):
    for inh in inhibitors:
        if inh["who"] == "braid" and "sleep" in inh["what"] and inh["mode"] == "block":
            return inh
    return None
