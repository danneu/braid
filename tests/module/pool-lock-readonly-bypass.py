# Test: pool-lock-readonly-bypass
#
# Intent:
#   Read-only operator diagnostics `braid status` and `braid doctor`, including
#   their JSON modes, must not acquire /run/braid-pool.lock.
#
# Why it exists:
#   Status and doctor are the diagnostic surface operators need during an
#   incident. A future refactor that routes them through the mutating lock path
#   would make those diagnostics fail or hang while another operation is active.
#
# Scenario:
#   Another shell holds the pool operation lock for a long-running maintenance
#   action. The operator asks for status and doctor output; each command should
#   reach its diagnostic renderer instead of reporting pool-lock contention.

import json
import shlex

start_all()
machine.wait_for_unit("multi-user.target", timeout=120)

contention = "another braid operation is already in progress"


def quote(value):
    return shlex.quote(str(value))


def start_lock_holder():
    holder_pid = machine.succeed(
        "rm -f /tmp/holder.ready; "
        "nohup sh -c 'exec 9>/run/braid-pool.lock; "
        "flock -x 9; touch /tmp/holder.ready; sleep 60' "
        ">/dev/null 2>&1 & echo $!"
    ).strip()
    machine.wait_until_succeeds("test -e /tmp/holder.ready", timeout=10)
    locks = machine.succeed("cat /proc/locks")
    assert "FLOCK" in locks, (
        "no flock in /proc/locks after holder readiness signal:\n"
        f"{locks}"
    )
    return holder_pid


def stop_lock_holder(holder_pid):
    machine.execute(f"kill {quote(holder_pid)} 2>/dev/null || true")
    machine.execute("rm -f /tmp/holder.ready")


def run_capture(name, command):
    stdout_path = f"/tmp/{name}.out"
    stderr_path = f"/tmp/{name}.err"
    machine.execute(f"rm -f {quote(stdout_path)} {quote(stderr_path)}")
    rc, _ = machine.execute(
        f"timeout 5 sh -c {quote(command)} "
        f">{quote(stdout_path)} 2>{quote(stderr_path)}"
    )
    stdout = machine.succeed(f"cat {quote(stdout_path)}")
    stderr = machine.succeed(f"cat {quote(stderr_path)}")
    return rc, stdout, stderr


def assert_reached_diagnostic(name, rc, stdout, stderr):
    assert rc != 124, f"{name}: hung waiting for lock\nstdout={stdout}\nstderr={stderr}"
    combined = stdout + stderr
    assert contention not in combined, (
        f"{name}: reported pool-lock contention\nstdout={stdout}\nstderr={stderr}"
    )


holder_pid = start_lock_holder()
try:
    with subtest("status human bypasses held pool lock"):
        rc, stdout, stderr = run_capture("status-human", "braid status")
        assert_reached_diagnostic("braid status", rc, stdout, stderr)
        assert "not mounted" in stdout, f"braid status did not render status:\n{stdout}"

    with subtest("status json bypasses held pool lock"):
        rc, stdout, stderr = run_capture("status-json", "braid status --json")
        assert_reached_diagnostic("braid status --json", rc, stdout, stderr)
        report = json.loads(stdout)
        assert report["status"] == "not_mounted", report

    with subtest("doctor human bypasses held pool lock"):
        rc, stdout, stderr = run_capture("doctor-human", "braid doctor")
        assert_reached_diagnostic("braid doctor", rc, stdout, stderr)
        assert "config file" in stdout, f"braid doctor did not render checks:\n{stdout}"

    with subtest("doctor json bypasses held pool lock"):
        rc, stdout, stderr = run_capture("doctor-json", "braid doctor --json")
        assert_reached_diagnostic("braid doctor --json", rc, stdout, stderr)
        report = json.loads(stdout)
        assert any(
            check.get("name") == "config_file" for check in report["checks"]
        ), report
finally:
    stop_lock_holder(holder_pid)
