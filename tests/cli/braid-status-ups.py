# braid-status-ups parser canary.
#
# Runs `braid ups status` (human + JSON) against a live dummy-ups setup
# and confirms the parser round-trips the expected fields. This mirrors
# the golden-fixture contract under CLI invocation, catching any drift
# that would only show up at runtime (wrapper PATH regressions,
# config.json emission changes, etc.).

import json
import re

start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("upsd.service")
machine.wait_for_unit("upsmon.service")

# Wait until the dummy-ups driver publishes ups.status. Slow VMs
# occasionally race `upsc` against the first driver poll.
machine.wait_until_succeeds("upsc ups@localhost ups.status", timeout=60)

# --- Human output ---
human = machine.succeed("braid ups status")
assert "Status: OL" in human, f"expected `Status: OL` in human output, got:\n{human}"
assert "Battery: 100%" in human, f"expected `Battery: 100%` in human output, got:\n{human}"
# Runtime 1800s = 30m 0s.
assert "Runtime: 30m 0s" in human, f"expected `Runtime: 30m 0s`, got:\n{human}"
# Load 17% + realpower 330W -> estimated (17 * 330 + 50) / 100 = 56 W.
assert "estimated" in human, f"expected `estimated` in human output, got:\n{human}"
assert "Device: APC Back-UPS ES 550G" in human, (
    f"expected device line in human output, got:\n{human}"
)

# --- JSON output ---
# The --json shape is the serialized UpscOutput; scripts key off
# `.status_flags` as an array of token strings.
raw = machine.succeed("braid ups status --json")
parsed = json.loads(raw)
assert "OL" in parsed["status_flags"], (
    f"expected OL in status_flags, got: {parsed['status_flags']}"
)
assert parsed["battery"]["charge_pct"] == 100, parsed
assert parsed["battery"]["runtime_secs"] == 1800, parsed
assert parsed["load_pct"] == 17, parsed
assert parsed["realpower_nominal_watts"] == 330, parsed
assert parsed["device"]["model"] == "Back-UPS ES 550G", parsed
assert parsed["device"]["mfr"] == "APC", parsed
assert "warning" not in parsed, parsed

# --- Empty-status warning branch ---
# The secondary dummy UPS has useful telemetry but an empty ups.status.
# Point braid's config at it and confirm --json preserves the parsed
# body while adding the warning sentinel.
machine.wait_until_succeeds("upsc emptyups@localhost battery.charge", timeout=60)
machine.succeed("jq '.ups.name = \"emptyups\"' /etc/braid/config.json > /tmp/empty-ups.json")
raw_empty = machine.succeed("braid --config /tmp/empty-ups.json ups status --json")
parsed_empty = json.loads(raw_empty)
assert parsed_empty.get("warning") == "ups_status_empty", (
    f"expected warning=ups_status_empty, got {parsed_empty}"
)
assert parsed_empty.get("status_flags") == [], (
    f"expected empty status_flags, got {parsed_empty}"
)
assert parsed_empty["battery"]["charge_pct"] == 55, parsed_empty
assert parsed_empty["battery"]["runtime_secs"] == 900, parsed_empty
assert parsed_empty["load_pct"] == 12, parsed_empty
assert parsed_empty["device"]["model"] == "Back-UPS ES 550G", parsed_empty
assert parsed_empty["device"]["mfr"] == "APC", parsed_empty
assert "error" not in parsed_empty, parsed_empty
human_empty = machine.succeed("braid --config /tmp/empty-ups.json ups status")
assert "Status: (unknown -- ups.status missing)" in human_empty.splitlines(), (
    "expected empty-status sentinel as a whole line in human output, got:\n"
    + human_empty
)

# --- Query-failed branch ---
# Stop upsd and confirm the query-failed JSON shape has the sentinel
# error and that --json mode keeps stderr silent. The exit code is 1
# so machine.execute (tolerant).
machine.succeed("systemctl stop upsd.service")
exit_code = machine.execute(
    "braid ups status --json >/tmp/ups_qf.out 2>/tmp/ups_qf.err"
)[0]
assert exit_code != 0, (
    "braid ups status --json must exit non-zero when query fails; got 0"
)
out = machine.succeed("cat /tmp/ups_qf.out")
err = machine.succeed("cat /tmp/ups_qf.err")
parsed_down = json.loads(out)
assert parsed_down.get("error") == "query_failed", (
    f"expected error=query_failed, got {parsed_down}"
)
detail = parsed_down.get("detail", "")
# When upsd is stopped, upsc emits "Error: Connection failure: ..." on stderr.
# The "Connection failure" substring is the stable slice; this catches a
# regression that drops captured stderr from the JSON detail field.
assert isinstance(detail, str) and "Connection failure" in detail, (
    f"expected detail to contain upsc stderr 'Connection failure', got {parsed_down}"
)
assert err == "", (
    f"expected empty stderr in --json query-failed, got: {err!r}"
)

# --- Invocation-failed branch ---
# Force upsc to fail to spawn by running the unwrapped braid with a PATH
# that does not include nut. This pins the invocation_failed sentinel
# that distinguishes "your braid wrapper / nut package is broken" from
# "your upsd is down" (the query_failed block above). Also pins the
# stdout-only contract: --json must not print a redundant human error to
# stderr.
braid_wrapped_path = machine.succeed("readlink -f $(command -v braid)").strip()


def unwrap_braid(path):
    source = machine.succeed(f"cat {path}")
    matches = re.findall(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', source)
    for target in matches:
        if "-braid-cli-" in target:
            return target
    assert matches, f"could not locate wrapped braid target in wrapper:\n{source}"
    nested_source = machine.succeed(f"cat {matches[0]}")
    nested_matches = re.findall(r'(/nix/store/[^"\s]+/bin/braid)(?!\-)', nested_source)
    for target in nested_matches:
        if "-braid-cli-" in target:
            return target
    assert False, (
        "could not locate unwrapped braid-cli target in wrappers:\n"
        + source
        + "\n--- nested wrapper ---\n"
        + nested_source
    )


unwrapped_braid = unwrap_braid(braid_wrapped_path)

exit_code = machine.execute(
    f"PATH=/nonexistent {unwrapped_braid} ups status --json "
    ">/tmp/ups_if.out 2>/tmp/ups_if.err"
)[0]
assert exit_code != 0, (
    "braid ups status --json must exit non-zero on invocation failure; got 0"
)
out_if = machine.succeed("cat /tmp/ups_if.out")
err_if = machine.succeed("cat /tmp/ups_if.err")
parsed_if = json.loads(out_if)
assert parsed_if.get("error") == "invocation_failed", (
    f"expected error=invocation_failed, got {parsed_if}"
)
detail_if = parsed_if.get("detail", "")
assert isinstance(detail_if, str) and detail_if.startswith("command failed: upsc "), (
    f"expected detail to start with 'command failed: upsc ', got {parsed_if}"
)
assert "invocation failed" not in detail_if, (
    f"legacy invocation prefix leaked into detail, got {parsed_if}"
)
assert err_if == "", (
    f"expected empty stderr in --json invocation-failed, got: {err_if!r}"
)

exit_code = machine.execute(
    f"PATH=/nonexistent {unwrapped_braid} ups status "
    ">/tmp/ups_if_human.out 2>/tmp/ups_if_human.err"
)[0]
assert exit_code != 0, (
    "braid ups status must exit non-zero on human invocation failure; got 0"
)
out_if_human = machine.succeed("cat /tmp/ups_if_human.out")
err_if_human = machine.succeed("cat /tmp/ups_if_human.err")
assert out_if_human == "", (
    f"expected empty stdout in human invocation-failed, got: {out_if_human!r}"
)
assert err_if_human.startswith("error: upsc invocation failed:"), (
    f"expected human invocation failure prefix, got: {err_if_human!r}"
)
assert "-- is pkgs.nut on PATH?" in err_if_human, (
    f"expected PATH hint in human invocation failure, got: {err_if_human!r}"
)
assert "upsc query failed" not in err_if_human, (
    f"query-failed wording leaked into human invocation failure: {err_if_human!r}"
)

# --- Not-enabled branch ---
# Materialize a config without the optional ups block and confirm the
# informational --json path exits 0 with its stable sentinel.
machine.succeed("jq 'del(.ups)' /etc/braid/config.json > /tmp/no-ups.json")
raw_no_ups = machine.succeed("braid --config /tmp/no-ups.json ups status --json")
parsed_no_ups = json.loads(raw_no_ups)
assert parsed_no_ups.get("error") == "ups_not_enabled", (
    f"expected error=ups_not_enabled, got {parsed_no_ups}"
)

# Human mode against the same no-ups config: the enable hint must land
# on stdout (so `braid ups status > log.txt` captures it) with empty
# stderr and exit 0. Substring is stable; full wording lives in
# print_not_enabled and is intentionally not snapshotted here.
exit_code = machine.execute(
    "braid --config /tmp/no-ups.json ups status "
    ">/tmp/no_ups_human.out 2>/tmp/no_ups_human.err"
)[0]
assert exit_code == 0, (
    f"braid ups status (no ups configured) must exit 0; got {exit_code}"
)
out_no_ups = machine.succeed("cat /tmp/no_ups_human.out")
err_no_ups = machine.succeed("cat /tmp/no_ups_human.err")
assert "braid.ups.enable = true" in out_no_ups, (
    f"expected enable-hint substring on stdout, got: {out_no_ups!r}"
)
assert err_no_ups == "", (
    f"expected empty stderr in human not-enabled, got: {err_no_ups!r}"
)
