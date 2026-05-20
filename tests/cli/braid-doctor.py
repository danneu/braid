# Test: braid doctor
#
# What: End-to-end tests for braid doctor against real config files — valid,
# missing, malformed JSON, bad schema, missing disks, canonical config
# permissions, and custom config skips in both human and --json modes.
#
# Why: Ensures doctor correctly categorizes config problems and produces
# structured output that operators and scripts can rely on.
#
# Dependencies: Rust braid binary, cryptsetup, btrfs-progs.

import json
import shlex

def assert_smart_selftest_shape(report, declared_members=None):
    selftest_rows = [c for c in report["checks"] if c["name"] == "smart_self_test"]
    assert len(selftest_rows) >= 1, f"missing smart_self_test row: {report['checks']}"
    if len(selftest_rows) == 1 and "subject" not in selftest_rows[0]:
        row = selftest_rows[0]
        assert row["status"] == "skip", f"unscoped smart_self_test: {row}"
        assert (
            "pool membership" in row["message"] or "no pool members" in row["message"]
        ), f"unscoped smart_self_test message: {row['message']}"
        return

    subjects = []
    for row in selftest_rows:
        assert row["name"] == "smart_self_test", f"smart_self_test row: {row}"
        assert "subject" in row, f"per-drive row missing subject: {row}"
        assert row["subject"], f"empty subject: {row}"
        subjects.append(row["subject"])

    assert len(subjects) == len(set(subjects)), f"duplicate selftest subjects: {subjects}"
    if declared_members is not None:
        assert set(subjects) == set(declared_members), (
            f"subjects {subjects} != declared {declared_members}"
        )

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
    assert "\x1b[" not in output, f"human output must be plain without a TTY:\n{output}"

with subtest("Valid config — JSON output"):
    raw = machine.succeed("braid doctor --json")
    print(f"Valid JSON:\n{raw}")
    report = json.loads(raw)
    assert report["status"] == "ok", f"Expected overall ok: {report['status']}"
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "ok", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "ok", f"config_schema: {checks['config_schema']}"
    assert checks["config_permissions"]["status"] == "ok", f"config_permissions: {checks['config_permissions']}"
    # This VM does not import the braid NixOS module, so the notifier
    # config file does not exist. The new beep_path check must skip with
    # the "monitor not configured" message — never Fail. Pinning this
    # branch here avoids needing a second VM just for the no-notifier case.
    assert checks["beep_path"]["status"] == "skip", f"beep_path: {checks['beep_path']}"
    assert "monitor not configured" in checks["beep_path"]["message"], (
        f"beep_path message: {checks['beep_path']['message']}"
    )
    assert_smart_selftest_shape(report)

# --- Permissions checks ---

# Intent: custom config paths skip config_permissions after JSON and schema pass.
# Why it exists: debug configs outside /etc/braid/config.json should not warn
# about ownership or mode bits that are irrelevant to the generated canonical file.
# Scenario: an operator copies the real config to /tmp, loosens permissions while
# editing, and runs `braid doctor --config` against that temporary file.
with subtest("Custom config -- permissions skipped"):
    machine.succeed("cp /etc/braid/config.json /tmp/world-writable.json")
    machine.succeed("chmod 666 /tmp/world-writable.json")
    raw = machine.succeed("braid doctor --json --config /tmp/world-writable.json")
    print(f"Custom world-writable JSON:\n{raw}")
    report = json.loads(raw)
    assert report["status"] == "ok", f"Expected overall ok: {report['status']}"
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "ok", f"config_file: {checks['config_file']}"
    assert checks["config_schema"]["status"] == "ok", f"config_schema: {checks['config_schema']}"
    assert checks["config_permissions"]["status"] == "skip", f"config_permissions: {checks['config_permissions']}"
    assert "custom config path" in checks["config_permissions"]["message"], (
        f"Expected custom path skip: {checks['config_permissions']['message']}"
    )

# Intent: the canonical default config still reports unsafe write permissions.
# Why it exists: custom-path skipping must not remove the guardrail for the real
# generated config file that commands read by default.
# Scenario: /etc/braid/config.json is accidentally replaced with a writable
# regular file on a deployed machine.
with subtest("Canonical config -- unsafe permissions warn"):
    machine.succeed("cp /etc/braid/config.json /tmp/braid-config.json.saved")
    try:
        machine.succeed("rm -f /etc/braid/config.json")
        machine.succeed("install -m 0666 /tmp/braid-config.json.saved /etc/braid/config.json")
        raw = machine.succeed("braid doctor --json")
        print(f"Canonical world-writable JSON:\n{raw}")
        report = json.loads(raw)
        assert report["status"] == "warn", f"Expected overall warn: {report['status']}"
        checks = {c["name"]: c for c in report["checks"]}
        assert checks["config_permissions"]["status"] == "warn", (
            f"config_permissions: {checks['config_permissions']}"
        )
        assert "world-writable" in checks["config_permissions"]["message"], (
            f"Expected 'world-writable' in message: {checks['config_permissions']['message']}"
        )
    finally:
        machine.succeed("install -m 0644 /tmp/braid-config.json.saved /etc/braid/config.json")

# Intent: the custom-path decision is lexical, not filesystem-canonicalized.
# Why it exists: the product rule is intentionally simple: only the exact normal
# path gets config_permissions enforcement.
# Scenario: an operator passes /etc/braid/./config.json, which reaches the same
# file but is not the canonical CLI default string.
with subtest("Lexical custom config path -- permissions skipped"):
    raw = machine.succeed("braid doctor --json --config /etc/braid/./config.json")
    print(f"Lexical custom JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    assert checks["config_file"]["status"] == "ok", f"config_file: {checks['config_file']}"
    assert checks["config_permissions"]["status"] == "skip", f"config_permissions: {checks['config_permissions']}"
    assert "custom config path" in checks["config_permissions"]["message"], (
        f"Expected custom path skip: {checks['config_permissions']['message']}"
    )

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

# --- Data profile mismatch (pool-based checks) ---

passphrase = "testpassphrase"
luks_opts = "--pbkdf pbkdf2 --pbkdf-force-iterations 1000"

def add_cmd(key):
    passphrase_q = shlex.quote(passphrase)
    return (
        f"printf '%s\\n' {passphrase_q} | "
        f"braid add --luks-format-arg=--pbkdf --luks-format-arg=pbkdf2 --luks-format-arg=--pbkdf-force-iterations --luks-format-arg=1000 {key}=/dev/disk/by-id/virtio-{key} --passphrase-stdin --yes"
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
    assert_smart_selftest_shape(report, declared_members={"disk1", "disk2"})

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

# --- Corrupted LUKS header ---
#
# Intent: end-to-end coverage that braid doctor's declared_disks check
#   detects an unreadable LUKS header on a real device via real cryptsetup,
#   and that the rendered remediation message tells the user to restore from
#   an off-system backup — never from a local /var/lib/braid/luks-headers/
#   file. The unit tests in cli/src/doctor.rs cover the message-rendering
#   half; this subtest covers the detection half that unit tests cannot
#   reach (no MockRunner can fabricate a real block device).
# Why it exists: previously, doctor never probed LUKS header health on
#   declared disks, so a wiped header passed silently and surfaced only as
#   a generic exit-1 from cryptsetup at unlock time. The product invariant
#   is that doctor must not point users at local .luksheader files; status
#   and the TUI already warn about persistent local copies.
# Scenario: an HDD whose first sectors get clobbered by a misdirected dd or
#   a controller bug. The dm-crypt mapping in kernel is still active so the
#   pool keeps running, but cryptsetup probes against the raw device fail.
with subtest("Corrupted LUKS header — declared_disks warns and stays generic"):
    # Diagnostic: confirm what /dev/disk/by-id/virtio-disk1 actually points at
    # and that the LUKS magic is in fact at offset 0 of the underlying device.
    print("by-id listing before:")
    print(machine.succeed("ls -la /dev/disk/by-id/"))
    print("hex dump of disk1 first 16 bytes BEFORE corruption:")
    print(machine.succeed("od -An -tx1 -N 16 /dev/disk/by-id/virtio-disk1"))

    # Wipe 16 MiB at offset 0. LUKS2's primary header + binary keyslot area is
    # within the first ~16 MiB, and 16 MiB is large enough that a misalignment
    # or partial-write bug cannot leave the magic intact. Without oflag=direct,
    # the write would land in the page cache and cryptsetup's later read could
    # bypass it. After the write, drop_caches invalidates any read caches that
    # might still be holding a stale header.
    machine.succeed(
        "dd if=/dev/zero of=/dev/disk/by-id/virtio-disk1 bs=1M count=16 "
        "conv=notrunc oflag=direct status=none"
    )
    machine.succeed("sync && echo 3 > /proc/sys/vm/drop_caches")

    print("hex dump of disk1 first 16 bytes AFTER corruption:")
    print(machine.succeed("od -An -tx1 -N 16 /dev/disk/by-id/virtio-disk1"))

    # Sanity-check: confirm cryptsetup itself now rejects the header. If this
    # fails, the dd above did not actually corrupt the on-disk header and the
    # later assertions would mis-diagnose the failure.
    is_luks_exit, is_luks_out = machine.execute(
        "cryptsetup isLuks /dev/disk/by-id/virtio-disk1"
    )
    print(f"cryptsetup isLuks after dd (exit {is_luks_exit}):\n{is_luks_out}")
    assert is_luks_exit != 0, (
        "dd did not corrupt the LUKS header: cryptsetup isLuks still succeeds"
    )
    raw = machine.succeed("braid doctor --json")
    print(f"Corrupted header JSON:\n{raw}")
    report = json.loads(raw)
    checks = {c["name"]: c for c in report["checks"]}
    declared = checks["declared_disks"]
    assert declared["status"] == "warn", f"declared_disks: {declared}"
    msg = declared["message"]
    assert "disk1" in msg, f"missing disk1 in message: {msg}"
    assert "header unreadable" in msg, f"missing 'header unreadable' in message: {msg}"
    assert "luksHeaderRestore" in msg, f"missing 'luksHeaderRestore' in message: {msg}"
    # Classified as unreadable (severe), not damaged — must NOT recommend repair.
    assert "cryptsetup repair" not in msg, (
        f"unreadable header must not suggest cryptsetup repair: {msg}"
    )
    # Cross-command consistency invariant: doctor must NEVER reference local
    # /var/lib/braid/luks-headers/ files. status and the TUI already warn
    # about local copies; doctor must be consistent with that posture.
    assert "/var/lib/braid/luks-headers/" not in msg, (
        f"doctor must not reference local backup directory: {msg}"
    )
    assert ".luksheader" not in msg, (
        f"doctor must not reference local .luksheader files: {msg}"
    )

machine.shutdown()
