# Test: braid.autoScrub option mapping
#
# Intent: Verify that braid.autoScrub generates braid-owned scrub systemd
#   units with correct lifecycle binding to braid-online.service, that
#   disabling removes the units, and that a custom interval passes through.
#
# Why it exists: braid owns the scrub timer to bind its lifecycle to the
#   pool's online state. The config test validates all unit properties —
#   lifecycle directives, scheduling priority, mount-point targeting, and
#   absence of the old nixpkgs timer.
#
# Scenario: Three nodes — defaults (enabled, monthly, /mnt/storage),
#   disabled (no scrub units), and weekly (custom interval).

import shlex

start_all()

TIMER = "braid-scrub.timer"
SERVICE = "braid-scrub.service"
TRIGGER_SERVICE = "braid-scrub-resume-trigger.service"


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

with subtest("defaults: timer fires monthly with Persistent=true"):
    # Timer-section properties (OnCalendar, Persistent) are not exposed
    # by systemctl show. Read the unit file content instead.
    timer_content = unit_content(defaults, TIMER)
    assert "OnCalendar=monthly" in timer_content, (
        "Expected OnCalendar=monthly in timer unit, got:\n" + timer_content
    )
    assert "Persistent=true" in timer_content, (
        "Expected Persistent=true in timer unit, got:\n" + timer_content
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

with subtest("defaults: old long-running resume service does not exist"):
    defaults.fail("systemctl cat braid-scrub-resume.service")

with subtest("defaults: scrub resume trigger is loaded and lifecycle-bound"):
    trigger_content = unit_content(defaults, TRIGGER_SERVICE)
    trigger_type = show(defaults, TRIGGER_SERVICE, "Type")
    assert trigger_type == "oneshot", (
        "Expected Type=oneshot for resume trigger, got: " + trigger_type
    )
    exec_start = exec_script_content(defaults, TRIGGER_SERVICE, "ExecStart")
    assert "scrub-needs-resume --mount /mnt/storage" in exec_start, (
        "Expected scrub-needs-resume predicate in ExecStart script, got: " + exec_start
    )
    assert "systemctl start --no-block braid-scrub.service" in exec_start, (
        "Expected no-block scrub service start in ExecStart script, got: " + exec_start
    )
    binds_to = show(defaults, TRIGGER_SERVICE, "BindsTo")
    assert "braid-online.service" in binds_to, (
        "Expected braid-online.service in BindsTo, got: " + binds_to
    )
    after = show(defaults, TRIGGER_SERVICE, "After")
    assert "braid-online.service" in after, (
        "Expected braid-online.service in After, got: " + after
    )
    assert "WantedBy=braid-online.service" in trigger_content, (
        "Expected WantedBy=braid-online.service in trigger unit, got:\n"
        + trigger_content
    )
    assert "ConditionPathIsMountPoint=/mnt/storage" in trigger_content, (
        "Expected ConditionPathIsMountPoint=/mnt/storage in trigger unit, got:\n"
        + trigger_content
    )
    assert "ProtectSystem=strict" in trigger_content, (
        "Expected ProtectSystem=strict in trigger unit, got:\n"
        + trigger_content
    )
    assert "CapabilityBoundingSet=" in trigger_content, (
        "Expected empty CapabilityBoundingSet in trigger unit, got:\n"
        + trigger_content
    )
    assert "RestrictAddressFamilies=AF_UNIX" in trigger_content, (
        "Expected AF_UNIX-only address families in trigger unit, got:\n"
        + trigger_content
    )
    conflicts = show(defaults, TRIGGER_SERVICE, "Conflicts")
    assert "sleep.target" in conflicts, (
        "Expected sleep.target in Conflicts, got: " + conflicts
    )
    before = show(defaults, TRIGGER_SERVICE, "Before")
    assert "sleep.target" in before, (
        "Expected sleep.target in Before, got: " + before
    )

with subtest("defaults: nixpkgs scrub timer does not exist"):
    defaults.fail("systemctl cat btrfs-scrub-mnt-storage.timer")

# === disabled node ===

with subtest("disabled: braid-scrub.timer does not exist"):
    disabled.wait_for_unit("multi-user.target")
    disabled.fail("systemctl cat {}".format(TIMER))

with subtest("disabled: braid-scrub-resume.service does not exist"):
    disabled.fail("systemctl cat braid-scrub-resume.service")

with subtest("disabled: braid-scrub-resume-trigger.service does not exist"):
    disabled.fail("systemctl cat {}".format(TRIGGER_SERVICE))

# === weekly node ===

with subtest("weekly: timer fires weekly"):
    weekly.wait_for_unit("multi-user.target")
    weekly.succeed("systemctl cat {}".format(TIMER))
    timer_content = unit_content(weekly, TIMER)
    assert "OnCalendar=weekly" in timer_content, (
        "Expected OnCalendar=weekly in timer unit, got:\n" + timer_content
    )

defaults.shutdown()
disabled.shutdown()
weekly.shutdown()
