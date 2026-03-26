# Test: braid doctor
#
# What: End-to-end tests for braid doctor against real config files — valid,
# missing, malformed JSON, bad schema, missing disks, and world-writable
# permissions — in both human and --json modes.
#
# Why: Ensures doctor correctly categorizes config problems and produces
# structured output that operators and scripts can rely on.
#
# Dependencies: Rust braid binary, cryptsetup, btrfs-progs.

import json
import shlex

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
    machine.succeed("""echo '{"mount_point":""}' > /tmp/bad-schema.json""")
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

# --- Data profile mismatch (pool-based checks) ---

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"BRAID_LUKS_OPTS='{luks_opts}' "
        f"braid add {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
    )

with subtest("Data profile mismatch — skip when pool not mounted"):
    raw = machine.succeed("braid doctor --json")
    print(f"Pool not mounted JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["data_profile_mismatch"]["status"] == "skip", (
        f"data_profile_mismatch: {checks['data_profile_mismatch']}"
    )
    assert checks["metadata_profile_mismatch"]["status"] == "skip", (
        f"metadata_profile_mismatch: {checks['metadata_profile_mismatch']}"
    )

# Set up the pool: add two disks → RAID1, write data
with subtest("Setup pool — add disk1 and disk2"):
    machine.succeed(add_cmd("disk1"))
    machine.succeed(add_cmd("disk2"))
    machine.succeed("dd if=/dev/urandom of=/mnt/storage/testdata bs=1M count=80 status=none")
    machine.succeed("sync")

with subtest("Data profile mismatch — clean RAID1 is ok"):
    raw = machine.succeed("braid doctor --json")
    print(f"Clean RAID1 JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["data_profile_mismatch"]["status"] == "ok", (
        f"data_profile_mismatch: {checks['data_profile_mismatch']}"
    )
    assert "RAID1" in checks["data_profile_mismatch"]["message"], (
        f"Expected RAID1 in message: {checks['data_profile_mismatch']['message']}"
    )
    assert checks["metadata_profile_mismatch"]["status"] == "ok", (
        f"metadata_profile_mismatch: {checks['metadata_profile_mismatch']}"
    )
    assert "RAID1" in checks["metadata_profile_mismatch"]["message"], (
        f"Expected RAID1 in message: {checks['metadata_profile_mismatch']['message']}"
    )

# Create mixed state: convert one block group to single
with subtest("Create mixed data profiles"):
    machine.succeed("btrfs balance start -dconvert=single,limit=1 /mnt/storage")

with subtest("Data profile mismatch — mixed profiles warns"):
    raw = machine.succeed("braid doctor --json")
    print(f"Mixed profiles JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["data_profile_mismatch"]["status"] == "warn", (
        f"data_profile_mismatch: {checks['data_profile_mismatch']}"
    )
    assert "mixed" in checks["data_profile_mismatch"]["message"], (
        f"Expected 'mixed' in message: {checks['data_profile_mismatch']['message']}"
    )
    # Metadata should still be ok (only data was converted)
    assert checks["metadata_profile_mismatch"]["status"] == "ok", (
        f"metadata_profile_mismatch: {checks['metadata_profile_mismatch']}"
    )

with subtest("Data profile mismatch — human output contains label"):
    output = machine.succeed("braid doctor")
    print(f"Mixed human:\n{output}")
    assert "data profiles" in output, f"Expected 'data profiles':\n{output}"
    assert "meta profiles" in output, f"Expected 'meta profiles':\n{output}"

machine.shutdown()
