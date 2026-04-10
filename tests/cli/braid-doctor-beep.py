# Test: braid doctor — PC speaker probe (beep_path check)
#
# Intent: Verify the new doctor `beep_path` check across its live branches:
#   human-mode Ok (working wrapper plays the beep), human-mode Fail (broken
#   wrapper), recovery, and JSON-mode Skip (the beep is suppressed for
#   programmatic consumption regardless of speaker state).
#
# Why it exists: Doctor is the proactive diagnostic surface for "is braid
#   wired up correctly?" Without this check, a broken PC speaker is invisible
#   until a real alert silently fails to make a sound. This test pins the
#   check's behavior so a regression that re-introduces silent best-effort
#   beeping, OR a regression that lets `braid doctor --json` produce audible
#   side effects (which would surprise scripts piping it into a monitoring
#   system), is caught.
#
# Scenario: NixOS machine with braid.monitor.beep = true and pkgs.beep
#   replaced by a flag-file-gated mock. /etc/braid/notifier-config.json is
#   written by the module. Subtests touch/remove /tmp/beep-broken to drive
#   the healthy/broken branches.

import json

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Notifier config was written by the module"):
    cfg = json.loads(machine.succeed("cat /etc/braid/notifier-config.json"))
    assert cfg["beep_probe_path"] is not None
    assert "braid-beep-probe" in cfg["beep_probe_path"]

# Human-mode subtests: these actually invoke the wrapper (the mock beep).
# `braid doctor` without --json is the operator-facing path, where playing
# the beep is the entire point of the check.

with subtest("Healthy beep (human mode): doctor exits 0"):
    machine.succeed("rm -f /tmp/beep-broken")
    machine.succeed("braid doctor")

with subtest("Broken beep (human mode): doctor exits 1"):
    machine.succeed("touch /tmp/beep-broken")
    rc, _ = machine.execute("braid doctor")
    assert rc == 1, f"expected exit 1 for broken beep in human mode, got {rc}"

with subtest("Recovery (human mode): clearing the flag returns to exit 0"):
    machine.succeed("rm -f /tmp/beep-broken")
    machine.succeed("braid doctor")

# JSON-mode subtest: must NEVER play the beep regardless of speaker state.

with subtest("JSON mode: beep_path is always Skip (beep never played)"):
    # Use broken beep to prove that the wrapper is not invoked even when it
    # would fail — the Skip must happen before any subprocess is spawned.
    # Broken speaker in human mode → exit 1, but JSON mode → exit 0 (Skip ≠ Fail).
    machine.succeed("touch /tmp/beep-broken")
    out = machine.succeed("braid doctor --json")
    report = json.loads(out)
    beep = next(c for c in report["checks"] if c["name"] == "beep_path")
    assert beep["status"] == "skip", f"expected skip in json mode, got {beep}"
    assert "json" in beep["message"].lower(), (
        f"Skip message must explain suppression in --json mode: {beep['message']}"
    )
    # Clean up so the next VM test (if any) starts from the healthy state.
    machine.succeed("rm -f /tmp/beep-broken")

# NOTE on the missing non-root subtest:
#
# The Skip-when-not-root branch of `check_beep_path_inner` is covered
# deterministically by `doctor::tests::beep_path_skips_when_not_root`, which
# constructs the context directly and asserts the runner is never invoked.
# It is intentionally NOT exercised here. `sudo -u tester braid doctor` would
# exit 1 with "braid must be run as root" because main.rs:244-251 rejects
# every command except `tui --demo` for non-root users — the request never
# reaches cmd_doctor. Reaching the inner branch from a VM would require
# relaxing that top-level CLI gate, which is intentionally out of scope.

machine.shutdown()
