# Shared bounded-rate scrub launcher for tests that need a live-scrub window.
#
# Owns both the rate and the launch: callers never run `btrfs scrub start`
# themselves. Passing the same nonzero rate to `scrub limit` and to
# `scrub start --limit` is what makes the window sound -- progs' scrub_start
# writes its --limit value to every device's scrub_speed_max knob before
# spawning the scrub threads and then restores the saved (identical) value,
# so no device is ever set to 0 and every device is bounded from
# scrub-thread launch onward (window = payload / rate). Configuring the rate
# separately and launching plainly would leave every device but the first
# unlimited for the whole run. The tool properties this rests on are locked
# by `tests/repro/btrfs-scrub-limit-bounds-rate.py`.


def scrub_throttle_start(node, mount, *, rate_mib):
    """Persist a per-device scrub rate of rate_mib MiB/s, assert the sysfs
    knob readback on every device, then launch a background scrub bounded by
    that rate for its whole run."""
    rate_arg = f"{rate_mib}m"
    rate_bytes = str(rate_mib * 1024 * 1024)

    # The operator-legible surface; sets scrub_speed_max on every device.
    node.succeed(f"btrfs scrub limit -a -l {rate_arg} {mount}")

    # Die at setup if the subcommand or the kernel knob vanishes on a future
    # pin, instead of going slow-path green with a vanished window.
    fsid = node.succeed(
        f"btrfs filesystem show {mount} | sed -n 's/.*uuid: //p'"
    ).strip()
    devids = node.succeed(f"ls /sys/fs/btrfs/{fsid}/devinfo").split()
    assert devids, f"no devices under /sys/fs/btrfs/{fsid}/devinfo"
    for devid in devids:
        knob = f"/sys/fs/btrfs/{fsid}/devinfo/{devid}/scrub_speed_max"
        value = node.succeed(f"cat {knob}").strip()
        assert value == rate_bytes, (
            f"scrub limit -a -l {rate_arg} did not land on devid {devid}: "
            f"{knob} reads {value!r}, expected {rate_bytes!r}. The throttle "
            "is not in force; see tests/repro/btrfs-scrub-limit-bounds-rate.py."
        )

    # The non-blocking form forks and the child inherits stdout; without the
    # redirect the test driver would sit on the open pipe until the scrub
    # finishes, silently discarding the live-scrub window.
    node.succeed(f"btrfs scrub start --limit {rate_arg} {mount} >/dev/null 2>&1")
