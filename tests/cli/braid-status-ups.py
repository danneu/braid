# braid-status-ups parser canary.
#
# Runs `braid ups status` (human + JSON) against a live dummy-ups setup
# and confirms the parser round-trips the expected fields. This mirrors
# the golden-fixture contract under CLI invocation, catching any drift
# that would only show up at runtime (wrapper PATH regressions,
# config.json emission changes, etc.).

import json

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
# Runtime 1800s = 30:00.
assert "Runtime: 30:00" in human, f"expected `Runtime: 30:00`, got:\n{human}"
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

# --- Not-enabled branch ---
# Materialize a config without the optional ups block and confirm the
# informational --json path exits 0 with its stable sentinel.
machine.succeed("jq 'del(.ups)' /etc/braid/config.json > /tmp/no-ups.json")
raw_no_ups = machine.succeed("braid --config /tmp/no-ups.json ups status --json")
parsed_no_ups = json.loads(raw_no_ups)
assert parsed_no_ups.get("error") == "ups_not_enabled", (
    f"expected error=ups_not_enabled, got {parsed_no_ups}"
)
