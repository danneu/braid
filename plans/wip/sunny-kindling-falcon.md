# Add TimeoutStopSec to braid-online.service

## Context

`braid-online.service` uses `ExecStop = braid lock` to unmount btrfs and close all LUKS devices on shutdown. Without an explicit `TimeoutStopSec`, the service inherits systemd's default of 90 seconds (`reference/systemd/meson_options.txt:200`). If `braid lock` exceeds that (plausible under heavy I/O), systemd kills it mid-operation, leaving LUKS devices not cleanly closed.

## Changes

### 1. Add TimeoutStopSec — `modules/braid/storage.nix:87-92`

Add `TimeoutStopSec = "5min";` to `serviceConfig`:

```nix
serviceConfig = {
  Type = "oneshot";
  RemainAfterExit = true;
  ExecStart = "${pkgs.coreutils}/bin/true";
  ExecStop = "${braidWrapped}/bin/braid lock";
  # braid lock flushes dirty pages, unmounts btrfs, and closes every LUKS
  # device. Under heavy I/O the default 90s can be exceeded; systemd would
  # then SIGKILL the lock process mid-operation. 5 minutes covers worst-case
  # flush without delaying shutdown indefinitely.
  TimeoutStopSec = "5min";
};
```

### 2. Update ADR — `docs/decisions/systemd-lifecycle.md:73-81`

Add `TimeoutStopSec = 5min` to the `braid-online.service` bullet list with rationale. Something like:

> - `TimeoutStopSec = 5min` — `braid lock` flushes btrfs dirty pages, unmounts, and closes LUKS. Under heavy I/O the default 90s can be exceeded; systemd would then SIGKILL the lock process mid-operation. 5 minutes is generous enough to cover worst-case flush without delaying shutdown indefinitely.

### 3. Add assertion — `tests/module/systemd-lifecycle.py`

Add a subtest early in the file (after the existing precondition checks) that verifies the timeout property. Follow the `systemctl show` pattern used in `tests/module/scrub-lifecycle.py`:

```python
with subtest("braid-online has generous stop timeout"):
    timeout = machine.succeed(
        "systemctl show braid-online.service -p TimeoutStopUSec --value"
    ).strip()
    assert timeout == "5min", f"Expected TimeoutStopUSec=5min, got {timeout}"
```

Note: `systemctl show` exposes the property as `TimeoutStopUSec` (D-Bus name), not `TimeoutStopSec` (unit file directive).

## Verification

```
just test-vm systemd-lifecycle
```

The new subtest asserts the timeout value automatically. If it passes, both the NixOS config and the assertion are correct.
