# Test: braid doctor
#
# What: End-to-end tests for braid doctor against real config files — valid,
# missing, malformed JSON, and bad schema — in both human and --json modes.
#
# Why: Ensures doctor correctly categorizes config problems and produces
# structured output that operators and scripts can rely on.
#
# Dependencies: Rust braid binary.

import json

start_all()
machine.wait_for_unit("multi-user.target")

# --- Valid config (default /etc/braid/config.json) ---

with subtest("Valid config — human output"):
    output = machine.succeed("braid doctor")
    print(f"Valid human:\n{output}")
    assert "[ok" in output, f"Expected [ok] tag:\n{output}"
    assert "config file" in output, f"Expected 'config file':\n{output}"
    assert "config schema" in output, f"Expected 'config schema':\n{output}"
    assert "config perms" in output, f"Expected 'config perms':\n{output}"

with subtest("Valid config — JSON output"):
    raw = machine.succeed("braid doctor --json")
    print(f"Valid JSON:\n{raw}")
    report = json.loads(raw)
    assert report["schema_version"] == 1, f"Expected schema_version 1: {report}"
    assert report["status"] == "ok", f"Expected overall ok: {report['status']}"
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "ok", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "ok", f"config_schema: {checks['config_schema']}"
    assert checks["config_permissions"]["status"] == "ok", f"config_permissions: {checks['config_permissions']}"

# --- Missing config ---

with subtest("Missing config — exits 1, fail + skip"):
    result = machine.execute("braid doctor --json --config /tmp/nonexistent.json")
    exit_code = result[0]
    raw = result[1]
    print(f"Missing config JSON (exit {exit_code}):\n{raw}")
    assert exit_code != 0, f"Expected non-zero exit, got {exit_code}"
    report = json.loads(raw)
    assert report["status"] == "fail", f"Expected overall fail: {report['status']}"
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "fail", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "skip", f"config_schema: {checks['config_schema']}"
    assert checks["config_permissions"]["status"] == "skip", f"config_permissions: {checks['config_permissions']}"

# --- Invalid JSON ---

with subtest("Invalid JSON — exits 1, config_file fail"):
    machine.succeed("echo 'not json {{{' > /tmp/bad.json")
    result = machine.execute("braid doctor --json --config /tmp/bad.json")
    exit_code = result[0]
    raw = result[1]
    print(f"Invalid JSON (exit {exit_code}):\n{raw}")
    assert exit_code != 0, f"Expected non-zero exit, got {exit_code}"
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "fail", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "skip", f"config_schema: {checks['config_schema']}"

# --- Bad schema (valid JSON, fails validation) ---

with subtest("Bad schema — config_file ok, config_schema fail"):
    machine.succeed("""echo '{"disks":[],"mountPoint":""}' > /tmp/bad-schema.json""")
    result = machine.execute("braid doctor --json --config /tmp/bad-schema.json")
    exit_code = result[0]
    raw = result[1]
    print(f"Bad schema JSON (exit {exit_code}):\n{raw}")
    assert exit_code != 0, f"Expected non-zero exit, got {exit_code}"
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "ok", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "fail", f"config_schema: {checks['config_schema']}"

# --- Permissions warnings ---

with subtest("World-writable config warns"):
    machine.succeed("cp /etc/braid/config.json /tmp/world-writable.json")
    machine.succeed("chmod 666 /tmp/world-writable.json")
    raw = machine.succeed("braid doctor --json --config /tmp/world-writable.json")
    print(f"World-writable JSON:\n{raw}")
    report = json.loads(raw)
    # warn does not cause failure — overall should be ok or warn, not fail
    assert report["status"] == "warn", f"Expected overall warn: {report['status']}"
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_permissions"]["status"] == "warn", f"config_permissions: {checks['config_permissions']}"
    assert "world-writable" in checks["config_permissions"]["message"], (
        f"Expected 'world-writable' in message: {checks['config_permissions']['message']}"
    )

machine.shutdown()
