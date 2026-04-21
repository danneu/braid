# Capture `upsc` output fixtures for the NUT parser-critical surface.
#
# Capture approach: the companion .nix seeds five dummy-ups drivers, one
# per target state (online / onbattery / lowbattery / replace-battery).
# For each state this script just invokes `upsc <state>@localhost` and
# copies the output into the fixtures dir. A fifth capture -- daemon
# down -- stops `upsd.service` and records the stderr `upsc` emits when
# it can't connect.
#
# Why driver-per-state vs one-driver + upsrw: NUT 2.8.4's dummy-ups
# intermittently re-reads the .dev file even in `dummy-once` mode, which
# races with `upsrw` writes. Keeping each state in its own static .dev
# file avoids the race and produces deterministic fixtures across
# stable and unstable NUT pins. See the .nix header for context.

FIXTURE_DIR = "/tmp/fixtures"

# Match the state names in the companion .nix. Each key is both the
# upsd UPS name AND the fixture file basename (minus extension).
STATES = ["online", "onbattery", "lowbattery", "replace-battery"]

start_all()
machine.wait_for_unit("multi-user.target")
machine.wait_for_unit("upsd.service")

# The dummy-ups driver publishes ups.status to upsd once its first poll
# cycle completes. Wait for each UPS's status to be queryable before
# we start capturing -- this avoids an empty-dump race on slow VMs.
for name in STATES:
    machine.wait_until_succeeds(
        f"upsc {name}@localhost ups.status", timeout=60
    )

machine.succeed(f"mkdir -p {FIXTURE_DIR}")

# --- Per-state captures ---
for name in STATES:
    machine.succeed(
        f"upsc {name}@localhost > {FIXTURE_DIR}/upsc-{name}.txt"
    )

# --- Daemon-down stderr ---
# Stop upsd so `upsc` can't connect. Tolerate upsc's non-zero exit via
# machine.execute -- we want the stderr regardless of exit code.
machine.succeed("systemctl stop upsd.service")
machine.execute(
    f"upsc online@localhost 2> {FIXTURE_DIR}/upsc-daemon-down.stderr"
)

# Copy every captured fixture out of the VM. `just capture-ups-fixtures`
# picks them up from result/fixtures/.
machine.copy_from_vm(FIXTURE_DIR, "")
