# Test: braid doctor -- PC speaker probe (beep_path check)
#
# Intent: Verify the doctor `beep_path` check across its live branches:
#   plain doctor skips without playing sound, explicit `--beep` reports Ok
#   or Fail from the wrapper, and JSON mode skips even when combined with
#   `--beep`.
#
# Why it exists: Doctor is the proactive diagnostic surface for "is braid
#   wired up correctly?" The alert test sound must be explicit so routine
#   doctor runs stay quiet, while `--beep` still catches a broken speaker
#   before a real disk alert silently fails. JSON output must never produce
#   audible side effects.
#
# Scenario: NixOS machine with braid.monitor.beep = true and pkgs.beep
#   replaced by a flag-file-gated mock. /etc/braid/notifier-config.json is
#   written by the module. Subtests touch/remove /tmp/beep-broken to drive
#   the healthy/broken branches and inspect /tmp/beep-invoked to prove
#   whether the wrapper ran.

import json

start_all()
machine.wait_for_unit("multi-user.target")

with subtest("Notifier config was written by the module"):
    cfg = json.loads(machine.succeed("cat /etc/braid/notifier-config.json"))
    assert cfg["beep_probe_path"] is not None
    assert "braid-beep-probe" in cfg["beep_probe_path"]

with subtest("Plain doctor skips beep even when mock beep is broken"):
    machine.succeed("rm -f /tmp/beep-invoked; touch /tmp/beep-broken")
    out = machine.succeed("braid doctor")
    assert (
        "skipped (pass --beep to play the audible alert test beep)" in out
    ), f"expected default skip message, got: {out}"
    machine.fail("test -e /tmp/beep-invoked")

with subtest("Explicit --beep succeeds when mock beep is healthy"):
    machine.succeed("rm -f /tmp/beep-broken /tmp/beep-invoked")
    out = machine.succeed("braid doctor --beep")
    assert (
        "alert test beep command succeeded -- you should have heard a 1 kHz, 500 ms disk-alert beep"
        in out
    ), f"expected --beep success message, got: {out}"
    machine.succeed("test -e /tmp/beep-invoked")

with subtest("Explicit --beep fails when mock beep is broken"):
    machine.succeed("rm -f /tmp/beep-invoked; touch /tmp/beep-broken")
    rc, _ = machine.execute("braid doctor --beep")
    assert rc == 1, f"expected exit 1 for broken beep with --beep, got {rc}"
    machine.succeed("test -e /tmp/beep-invoked")

with subtest("JSON plus --beep skips without invoking wrapper"):
    # Use broken beep to prove that the wrapper is not invoked even when it
    # would fail -- the Skip must happen before any subprocess is spawned.
    machine.succeed("rm -f /tmp/beep-invoked; touch /tmp/beep-broken")
    out = machine.succeed("braid doctor --json --beep")
    report = json.loads(out)
    beep = next(c for c in report["checks"] if c["name"] == "beep_path")
    assert beep["status"] == "skip", f"expected skip in json mode, got {beep}"
    assert (
        beep["message"]
        == "skipped in --json mode -- rerun with --beep without --json to play the alert test beep"
    ), (
        "Skip message must explain suppression in --json mode: "
        f"{beep['message']}"
    )
    machine.fail("test -e /tmp/beep-invoked")
    # Clean up so the next VM test (if any) starts from the healthy state.
    machine.succeed("rm -f /tmp/beep-broken /tmp/beep-invoked")

machine.shutdown()
