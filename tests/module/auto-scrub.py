# Test: braid.autoScrub option mapping
#
# Intent: Verify that braid.autoScrub generates braid-owned scrub systemd
#   units with correct lifecycle binding to braid-online.service, that
#   disabling removes the units, that autoScrub.intervalDays reaches the CLI
#   as --fresh-for-secs, that the timer is a poll (OnActiveSec + hourly, no
#   Persistent, no WakeSystem) rather than a schedule, and that a busy skip
#   (exit 4) is a unit success carrying no retry apparatus of its own.
#
# Why it exists: braid owns the scrub timer to bind its lifecycle to the
#   pool's online state. The config test validates all unit properties —
#   lifecycle directives, scheduling priority, mount-point targeting, and
#   absence of the old nixpkgs timer. The negative asserts are the load-bearing
#   half: Persistent=, OnCalendar=monthly, WakeSystem=, RestartForceExitStatus=
#   and the deleted resume-trigger unit each reintroduce a schedule record or a
#   wakeup that ADR 035 deleted, and each would be invisible in normal use.
#
# Scenario: Three nodes — defaults (enabled, 30-day window, /mnt/storage),
#   disabled (no scrub units), and weekly (intervalDays = 7).

import shlex

start_all()

TIMER = "braid-scrub.timer"
SERVICE = "braid-scrub.service"


def show(node, unit, prop):
    return node.succeed(
        "systemctl show {} -p {} --value".format(unit, prop)
    ).strip()


def unit_content(node, unit):
    return node.succeed("systemctl cat {}".format(unit))


def exec_path(node, unit, prop):
    raw = show(node, unit, prop)
    marker = "path="
    start = raw.find(marker)
    assert start != -1, "Expected path= in {} for {}, got: {}".format(prop, unit, raw)
    start += len(marker)
    end = raw.find(" ", start)
    semi = raw.find(";", start)
    if end == -1 or (semi != -1 and semi < end):
        end = semi
    assert end != -1, "Could not parse command path from {}".format(raw)
    return raw[start:end]


def exec_script_content(node, unit, prop):
    return node.succeed("cat {}".format(shlex.quote(exec_path(node, unit, prop))))


# === defaults node ===

with subtest("defaults: braid-scrub.timer is loaded"):
    defaults.wait_for_unit("multi-user.target")
    defaults.succeed("systemctl cat {}".format(TIMER))

with subtest("defaults: timer is bound to braid-online.service"):
    binds_to = show(defaults, TIMER, "BindsTo")
    assert "braid-online.service" in binds_to, (
        "Expected braid-online.service in BindsTo, got: " + binds_to
    )
    after = show(defaults, TIMER, "After")
    assert "braid-online.service" in after, (
        "Expected braid-online.service in After, got: " + after
    )

with subtest("defaults: the timer is an hourly poll with a prompt post-unlock poke"):
    # Timer-section properties (OnCalendar, OnActiveSec, Persistent) are not
    # exposed by systemctl show. Read the unit file content instead.
    timer_content = unit_content(defaults, TIMER)
    assert "OnCalendar=hourly" in timer_content, (
        "Expected OnCalendar=hourly in timer unit, got:\n" + timer_content
    )
    assert "OnActiveSec=30s" in timer_content, (
        "Expected OnActiveSec=30s in timer unit, got:\n" + timer_content
    )
    assert "AccuracySec=1min" in timer_content, (
        "Expected AccuracySec=1min in timer unit, got:\n" + timer_content
    )

with subtest("defaults: the timer keeps no schedule record and wakes nothing"):
    # Persistent= maintains a stamp file that is a second "when did we last
    # scrub" record; btrfs's own record is the single anchor now. WakeSystem=
    # would wake a suspended NAS, which braid never does (ADR 016). A calendar
    # cadence would make the timer the schedule again rather than a poll.
    timer_content = unit_content(defaults, TIMER)
    assert "Persistent=" not in timer_content, (
        "The timer must keep no stamp-file schedule record, got:\n" + timer_content
    )
    assert "WakeSystem=" not in timer_content, (
        "braid must schedule no wakeups, got:\n" + timer_content
    )
    assert "OnCalendar=monthly" not in timer_content, (
        "The scrub cadence is the freshness window, not the timer, got:\n"
        + timer_content
    )

with subtest("defaults: scrub service targets pool mount point"):
    exec_start = show(defaults, SERVICE, "ExecStart")
    assert "scrub-resume-or-start" in exec_start, (
        "Expected 'scrub-resume-or-start' in ExecStart, got: " + exec_start
    )
    assert "/mnt/storage" in exec_start, (
        "Expected /mnt/storage in ExecStart, got: " + exec_start
    )
    exec_stop = exec_script_content(defaults, SERVICE, "ExecStop")
    assert "scrub-cancel" in exec_stop, (
        "Expected 'scrub-cancel' in ExecStop script, got: " + exec_stop
    )

with subtest("defaults: scrub service has correct scheduling priority"):
    nice = show(defaults, SERVICE, "Nice")
    assert nice == "19", "Expected Nice=19, got: " + nice
    # IOSchedulingClass=idle is reported as numeric value 3 by systemctl show.
    io_class = show(defaults, SERVICE, "IOSchedulingClass")
    assert io_class == "3", (
        "Expected IOSchedulingClass=3 (idle), got: " + io_class
    )

with subtest("defaults: scrub service has ConditionPathIsMountPoint"):
    # ConditionPathIsMountPoint shows as part of the unit conditions.
    # Read from the unit file content.
    svc_content = unit_content(defaults, SERVICE)
    assert "ConditionPathIsMountPoint=/mnt/storage" in svc_content, (
        "Expected ConditionPathIsMountPoint=/mnt/storage in service unit, got:\n"
        + svc_content
    )

with subtest("defaults: scrub service conflicts with shutdown (default deps) and sleep (explicit)"):
    conflicts = show(defaults, SERVICE, "Conflicts")
    assert "shutdown.target" in conflicts, (
        "Expected shutdown.target in Conflicts, got: " + conflicts
    )
    assert "sleep.target" in conflicts, (
        "Expected sleep.target in Conflicts, got: " + conflicts
    )
    before = show(defaults, SERVICE, "Before")
    assert "shutdown.target" in before, (
        "Expected shutdown.target in Before, got: " + before
    )
    assert "sleep.target" in before, (
        "Expected sleep.target in Before, got: " + before
    )

with subtest("defaults: scrub service is bound to braid-online"):
    binds_to = show(defaults, SERVICE, "BindsTo")
    assert "braid-online.service" in binds_to, (
        "Expected braid-online.service in BindsTo, got: " + binds_to
    )
    after = show(defaults, SERVICE, "After")
    assert "braid-online.service" in after, (
        "Expected braid-online.service in After, got: " + after
    )

with subtest("defaults: a busy skip is a success with no retry apparatus"):
    # A scrub skipped because braid was already working on the pool exits 4.
    # SuccessExitStatus keeps it off onFailure (no beep, no scrub-failed flag).
    # The retry is the next hourly poll, so RestartForceExitStatus/RestartSec
    # must be gone -- a unit-level restart would be a second scheduler racing
    # the timer. StartLimitIntervalSec=0 keeps hourly polls during a days-long
    # balance from exhausting the start limit and giving up silently.
    # Read the unit file rather than `systemctl show`: the exit-status list
    # properties render in a systemd-internal shape, while the directives
    # themselves are the contract.
    svc_content = unit_content(defaults, SERVICE)
    assert "SuccessExitStatus=3" in svc_content, (
        "Expected SuccessExitStatus=3 in service unit, got:\n" + svc_content
    )
    assert "SuccessExitStatus=4" in svc_content, (
        "Expected SuccessExitStatus=4 in service unit, got:\n" + svc_content
    )
    assert "RestartForceExitStatus=" not in svc_content, (
        "The poll is the retry; no restart apparatus may remain, got:\n" + svc_content
    )
    assert "RestartSec=" not in svc_content, (
        "The poll is the retry; no restart interval may remain, got:\n" + svc_content
    )
    assert "StartLimitIntervalSec=0" in svc_content, (
        "Expected StartLimitIntervalSec=0 in service unit, got:\n" + svc_content
    )
    # A skip must never be treated as a failure by the alert path.
    restart = show(defaults, SERVICE, "Restart")
    assert restart == "no", "Expected Restart=no, got: " + restart

with subtest("defaults: the freshness window reaches the CLI as --fresh-for-secs"):
    # The window is passed on the command line, never read from a config file:
    # the scrub units stay config-file-free (ADR 018). 30 days = 2592000s.
    exec_start = show(defaults, SERVICE, "ExecStart")
    assert "--fresh-for-secs 2592000" in exec_start, (
        "Expected the default 30-day window as --fresh-for-secs, got: " + exec_start
    )

with subtest("defaults: old long-running resume service does not exist"):
    defaults.fail("systemctl cat braid-scrub-resume.service")

with subtest("defaults: the pool-online resume trigger is gone"):
    # The scrub service self-gates now, and the timer's OnActiveSec poke
    # resumes an aborted scrub within ~30s of unlock, so the trigger unit and
    # its scrub-needs-resume predicate were deleted rather than kept in sync.
    # A dead-name guard: a stale unit left behind would start scrubs on every
    # pool-online, outside the freshness window entirely.
    defaults.fail("systemctl cat braid-scrub-resume-trigger.service")
    defaults.fail("braid scrub-needs-resume --mount /mnt/storage")

with subtest("defaults: nixpkgs scrub timer does not exist"):
    defaults.fail("systemctl cat btrfs-scrub-mnt-storage.timer")

# === disabled node ===

with subtest("disabled: braid-scrub.timer does not exist"):
    disabled.wait_for_unit("multi-user.target")
    disabled.fail("systemctl cat {}".format(TIMER))

with subtest("disabled: braid-scrub-resume.service does not exist"):
    disabled.fail("systemctl cat braid-scrub-resume.service")

with subtest("disabled: braid-scrub-resume-trigger.service does not exist"):
    disabled.fail("systemctl cat braid-scrub-resume-trigger.service")

# === weekly node ===

with subtest("weekly: intervalDays passes through as a smaller freshness window"):
    weekly.wait_for_unit("multi-user.target")
    exec_start = show(weekly, SERVICE, "ExecStart")
    assert "--fresh-for-secs 604800" in exec_start, (
        "Expected intervalDays=7 as 604800s in --fresh-for-secs, got: " + exec_start
    )

with subtest("weekly: the poll cadence does not follow the freshness window"):
    # The window is the schedule; the timer stays an hourly poll regardless.
    timer_content = unit_content(weekly, TIMER)
    assert "OnCalendar=hourly" in timer_content, (
        "Expected OnCalendar=hourly in timer unit, got:\n" + timer_content
    )

defaults.shutdown()
disabled.shutdown()
weekly.shutdown()
