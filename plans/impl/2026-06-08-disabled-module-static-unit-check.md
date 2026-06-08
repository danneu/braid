# Fix the D-Bus-timeout flake in braid-module-disabled

## Context

`tests/module/disabled.py` (the `braid-module-disabled` check) verifies the braid
NixOS module is inert when `enable = false`: it must leak no `braid-*` systemd
units. Its assertion (`disabled.py`, the "Module is inert when disabled" subtest)
is currently:

```python
machine.succeed("systemctl list-unit-files >/tmp/all-units && ! grep -q '^braid-' /tmp/all-units")
```

It fails intermittently. The captured failure was `systemctl list-unit-files`
exiting 1 with `Failed to list unit files: Connection timed out` -- the probe
died *before* the `grep`, so it was never about braid units existing.

Root cause: `systemctl` is a D-Bus client. `list-unit-files` asks the systemd
manager (PID 1) to walk the whole unit search path, stat/parse every file, and
serialize enable state -- one of the heavier introspection calls. sd-bus bounds
the wait at systemd's default method-call timeout (25s). `wait_for_unit(
"multi-user.target")` returns the instant the target is reached, but the manager
can still be settling (draining its job queue, dbus-broker just up) on a slow
emulated VM (aarch64 on the darwin linux-builder, contending with parallel
checks). The probe fires into a momentarily-busy PID 1 and times out. That is an
infrastructure flake, not a real assertion failure.

Intended outcome: make the check deterministic by removing the D-Bus dependency
entirely. Absence-of-units is a static, build-output property -- NixOS renders
every module-defined unit into `/etc/systemd/system` -- so it should be read off
the filesystem, not by querying the live manager. This deletes the entire
timeout failure class rather than retrying around it.

Confirmed during exploration:
- Every braid unit is declared via `systemd.services|timers|targets` in
  `modules/braid/` (`storage.nix`, `monitor.nix`, `ups.nix`, `fan-control.nix`)
  and lands only in `/etc/systemd/system`. No units ship via
  `environment.systemPackages` or any other search path. So a `braid-*` glob
  there is faithful to the old `list-unit-files | grep '^braid-'`.
- `disabled.py` is the *only* test issuing a global `list-unit-files`
  enumeration right after boot. All other tests use `systemctl show <named-unit>`
  or post-modification `daemon-reload` (scoped, responsive) -- no flake-siblings,
  so nothing else needs the same change.

## Approach: assert on the filesystem instead of the manager

Replace the `systemctl`/D-Bus assertion in `disabled.py`'s "Module is inert when
disabled" subtest with a filesystem glob:

```python
    # The disabled module must leak no braid-* units. NixOS renders every
    # module-defined unit (and any *.wants activation symlink) into
    # /etc/systemd/system, so this filesystem glob is the faithful inertness
    # check -- and unlike `systemctl list-unit-files` it makes no D-Bus call to
    # PID 1, which can time out on a slow VM just past multi-user.target. That
    # timeout, not a real leaked unit, was this test's prior transient failure.
    leaked = machine.succeed("find /etc/systemd/system -name 'braid-*'").strip()
    assert leaked == "", f"disabled module leaked braid units:\n{leaked}"
```

Why this form:
- **`find` over `/etc/systemd/system`** is deterministic and can never time out.
  `find` exits 0 on a readable, existing directory (which this always is) whether
  or not it matches, so `machine.succeed` is honest; the assertion is on its
  output, not its exit code.
- **Recursive** so it catches both a leaked unit *file*
  (`/etc/systemd/system/braid-*.service`) and a leaked *activation* symlink
  (`*.wants/braid-*`), matching the preamble's "could leak a definition and
  silently activate" intent. `find` does not follow symlinks by default, so it
  matches the unit symlink entries by name without traversing into store paths --
  no runaway walk.
- **Names the offender on failure** (`leaked` in the assert message), which the
  old `&& ! grep` form did not -- better diagnostics on a real regression.
- **`braid-*` scope preserved** exactly from the old `^braid-` grep. (The
  fan-control service `hddfancontrol-braid` lies outside this prefix and is caught
  by neither old nor new check -- a pre-existing, intentional scope choice, not
  changed here.)

Equivalent compact alternative if brevity is preferred:
`machine.fail("find /etc/systemd/system -name 'braid-*' | grep -q .")` (no-match
-> grep exits 1 -> `fail` passes; match -> grep exits 0 -> `fail` raises). The
named-output form above is recommended for its failure diagnostics.

The preamble (Intent / Why it exists / Scenario) stays accurate as written --
"zero braid-* unit files installed" still describes the filesystem check -- so no
preamble edit is required. `tests/module/disabled.nix` makes no systemctl/D-Bus
claim and is unchanged.

## Files to modify

- `tests/module/disabled.py` -- the two-line assertion in the "Module is inert
  when disabled" subtest, plus its inline comment. Only change.

No `.nix`, Rust, or module changes. No sibling-test follow-ups (none share the
pattern). Closest neighbor `tests/ups-credential-lifecycle.py` uses
`systemctl show` on *named* units (not a global list) right after boot -- much
smaller scope; note as watch-only, do not touch.

## Verification

1. `just test-vm braid-module-disabled -v` -- the subtest passes; `find` reports
   no braid units on a disabled module.
2. Re-run to confirm non-flaky: `just test-vm braid-module-disabled -rebuild`
   (a few times). The determinism here is structural (no D-Bus call is possible),
   not probabilistic, so passing re-runs are a sanity check rather than the proof.
3. (Optional) Exercise the failure path so the assertion isn't vacuous: in a
   throwaway run, `touch /etc/systemd/system/braid-fake.service` before the
   assertion and confirm it fails naming `braid-fake.service`; then revert.
4. No Rust or module changes, so `just test-rust` and the other VM checks are
   unaffected.
