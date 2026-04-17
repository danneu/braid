# Plan: replace `braid.fanControl.pwmPath` with grouped `pwm` option

## Context

The just-landed `braid.fanControl` module takes `pwmPath` as a single string
that either embeds shell-globbing backticks (to handle `hwmonN` renumbering
across reboots) or pins to an unstable literal `/sys/class/hwmon/hwmonN/...`
path. Both are footguns:

- The backtick form puts shell syntax inside a Nix string. Users see
  `` "`echo /sys/devices/platform/f71882fg.656/hwmon/hwmon[[:print:]]`/device/pwm2" ``,
  which is easy to typo and impossible to validate at eval time.
- The literal form is stable only until a kernel bump or driver load-order
  change renumbers `hwmonN`, at which point fan control silently breaks.

This plan replaces `pwmPath` with a grouped `pwm` sub-option that captures
the stable part (platform device name) and the PWM channel number
separately, and moves resolution into the service script. While the module
is open, add a `warnings` entry for `minFanSpeedPercent = 0`, which lets the
fan stop entirely below `minTemp` and deserves a visible flag at rebuild.

Breaking API change. Per `AGENTS.md`'s no-backwards-compatibility rule
(braid is unreleased), replace `pwmPath` outright with no shim or
deprecation.

Target API:

```nix
braid.fanControl = {
  enable = true;
  pwm = {
    platformDevice = "f71882fg.656";
    number = 2;
  };
  minStart = 65;
  maxStop = 60;
};
```

---

## 1. Option changes in `modules/braid/fan-control.nix`

**Remove:** `pwmPath` option and the top-of-file `pwmSpec` `let`-binding.

**Add:** grouped `pwm` sub-option:

```nix
pwm = {
  platformDevice = lib.mkOption {
    type = lib.types.str;
    default = "";
    example = "f71882fg.656";
    description = ''
      Platform device name of the Super I/O chip driving the chassis fan,
      as shown in /sys/devices/platform/ (e.g. "nct6775", "f71882fg.656",
      "it87.2608"). Identified via:

        pwm=/sys/class/hwmon/hwmonN/device/pwmN
        basename "$(readlink -f "$(dirname "$pwm")")"

      This name is stable across reboots; the hwmonN number is not.
    '';
  };

  number = lib.mkOption {
    type = lib.types.ints.positive;
    example = 2;
    description = ''
      PWM channel number within the platform device (1-based; matches
      pwmN in sysfs). Identified via `pwmconfig`. No default --
      pwm1 is frequently the CPU fan or an unpopulated header.
    '';
  };
};
```

**Update assertions.** Remove the existing `fc.pwmPath != ""` assertion.
Add two platformDevice assertions:

```nix
{
  assertion = fc.pwm.platformDevice != "";
  message = "braid.fanControl.pwm.platformDevice is required. "
    + "See the fan control guide for the discovery workflow.";
}
{
  assertion = builtins.match "[A-Za-z0-9_.-]+" fc.pwm.platformDevice != null;
  message = "braid.fanControl.pwm.platformDevice (${fc.pwm.platformDevice}) "
    + "must be a platform device identifier (e.g. \"f71882fg.656\"), "
    + "not a full path or shell expression. "
    + "Expected characters: A-Z a-z 0-9 _ . -";
}
```

Leave the `maxStop <= minStart` and `minTemp < maxTemp` assertions
unchanged.

**Add warning** for aggressive minimum fan speed:

```nix
warnings = lib.optional (fc.minFanSpeedPercent == 0) ''
  braid.fanControl.minFanSpeedPercent is 0, so hddfancontrol may stop the
  fan entirely below minTemp. Only use this if the chassis has other
  cooling or is designed for passive airflow.
'';
```

## 2. Resolver in the service `script`

Replace the current one-line `exec` (which interpolates `${pwmSpec}`) with a
resolver preamble that matches exactly one PWM path, then execs the daemon:

```nix
script = ''
  matches=( \
    /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/device/pwm${toString fc.pwm.number} \
    /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/pwm${toString fc.pwm.number} \
  )
  existing=()
  for path in "''${matches[@]}"; do
    [ -e "$path" ] && existing+=("$path")
  done
  if [ "''${#existing[@]}" -ne 1 ]; then
    echo "braid.fanControl: expected exactly one PWM path matching" >&2
    echo "  /sys/devices/platform/${fc.pwm.platformDevice}/hwmon/hwmon*/{device/,}pwm${toString fc.pwm.number}," >&2
    echo "  found ''${#existing[@]}." >&2
    if [ "''${#existing[@]}" -eq 0 ]; then
      echo "Is the kernel module for ${fc.pwm.platformDevice} loaded and bound?" >&2
      echo "Check: ls /sys/devices/platform/ | grep -i ${fc.pwm.platformDevice}" >&2
    else
      echo "Multiple PWM paths resolved; narrow platformDevice or verify board driver binding." >&2
    fi
    exit 1
  fi
  pwm_path="''${existing[0]}"
  exec ${lib.getExe pkgs.hddfancontrol} -v INFO daemon \
    -d ata \
    -p "$pwm_path:${toString fc.minStart}:${toString fc.maxStop}" \
    --drive-temp-range ${toString fc.minTemp} ${toString fc.maxTemp} \
    --min-fan-speed-prct ${toString fc.minFanSpeedPercent} \
    --interval ${lib.escapeShellArg fc.interval} \
    --restore-fan-settings
'';
```

Fallback order: `hwmon*/device/pwmN` before `hwmon*/pwmN`. Matches the
common f71882fg / nct6775 layout first. Double-escape (`''${...}`) bash
interpolation inside Nix's `''...''`.

## 3. Test updates

### `tests/module/fan-control.nix`

Replace:

```nix
pwmPath = "/sys/class/hwmon/hwmon0/pwm1";
```

with:

```nix
pwm = {
  platformDevice = "braid-test.0";
  number = 2;
};
```

Keep the other fields (`minStart = 65; maxStop = 60; minTemp = 25;
maxTemp = 45; minFanSpeedPercent = 10; interval = "20s";`).

### `tests/module/fan-control.py`

Replace the current "hddfancontrol-braid service has correct arguments"
subtest's PWM-path assertion with resolver-shape assertions:

```python
with subtest("resolver script references correct platform device and pwm number"):
    assert "/sys/devices/platform/braid-test.0/hwmon/hwmon*/device/pwm2" in script, \
        f"Expected hwmon/device/pwm fallback in script:\n{script}"
    assert "/sys/devices/platform/braid-test.0/hwmon/hwmon*/pwm2" in script, \
        f"Expected hwmon/pwm fallback in script:\n{script}"
    assert 'braid.fanControl: expected exactly one PWM path' in script, \
        f"Expected resolver failure message in script:\n{script}"
    assert ":65:60" in script, \
        f"Expected minStart:maxStop suffix on -p arg:\n{script}"
```

Keep the other arg assertions (`-d ata`, `--drive-temp-range 25 45`,
`--min-fan-speed-prct 10`, `--interval 20s`, `--restore-fan-settings`).

Keep the other subtests (drivetemp, Restart=always, no-hddtemp, oneshot
debounce, udev rules).

### `tests/module/fan-control-hotswap.nix`

Replace the `pwmPath = ...` line with the same grouped `pwm` block as the
wiring test. Daemon stays stubbed with `sleep infinity`; the resolver
never runs in this test.

### `tests/module/fan-control-hotswap.py`

No change.

## 4. Manual updates

### `manual/guides/fan-control.md`

- Discovery section: add a step after "Map fans to PWMs with pwmconfig"
  that translates the pwmconfig-surfaced hwmon path to a platform device.
  The snippet handles both the `hwmon*/device/pwmN` layout (common on
  f71882fg, nct6775) and the `hwmon*/pwmN` fallback layout -- without
  the `if` branch, users on the fallback layout resolve to `hwmon4`
  instead of the platform device:

  ```sh
  pwm=/sys/class/hwmon/hwmon4/device/pwm2  # from pwmconfig output
  pwm_dir=$(dirname "$pwm")
  if [ "$(basename "$pwm_dir")" != device ]; then
    pwm_dir="$pwm_dir/device"
  fi
  basename "$(readlink -f "$pwm_dir")"
  # -> f71882fg.656
  ```

  The PWM number is the numeric suffix on the pwmN filename (2 in the
  example above).

- Committing to Nix section: replace the `pwmPath` recipe with:

  ```nix
  braid.fanControl = {
    enable = true;
    pwm = {
      platformDevice = "f71882fg.656";
      number = 2;
    };
    minStart = 65;
    maxStop = 60;
  };
  ```

- Worked example (ASRock IMB-X1231): update the final Nix config block to
  use the grouped `pwm` option.

- Remove all references to the `hwmon[[:print:]]` backtick trick.

### `manual/guides/nixos-configuration.md`

- Replace the `braid.fanControl.pwmPath` row in the option table with two
  rows:

  ```markdown
  | `braid.fanControl.pwm.platformDevice` | string | (required) | Platform device name under `/sys/devices/platform/` |
  | `braid.fanControl.pwm.number` | int | (required) | PWM channel number (1-based) |
  ```

- Update the full config example's `fanControl` block accordingly.

### `manual/index.md`

No changes.

### `manual/book/`

Rebuild via `mdbook build` after markdown changes. Gitignored, local
preview only.

## 5. Implementation order

1. Update `modules/braid/fan-control.nix`: options, assertions, warning,
   resolver script.
2. Update `tests/module/fan-control.nix` + `tests/module/fan-control.py`.
3. Update `tests/module/fan-control-hotswap.nix`.
4. Run:
   ```
   nix build --no-link \
     .#checks.x86_64-linux.fan-control \
     .#checks.x86_64-linux.fan-control-hotswap
   ```
   Confirm both pass.
5. Update `manual/guides/fan-control.md` and
   `manual/guides/nixos-configuration.md`.
6. `mdbook build` under `manual/`.

## 6. Verification

**Wiring test (`fan-control`):**
- Generated `hddfancontrol-braid` script contains both glob fallbacks
  (`/sys/devices/platform/braid-test.0/hwmon/hwmon*/device/pwm2` and the
  `/hwmon*/pwm2` variant).
- Script contains the resolver failure message ("braid.fanControl:
  expected exactly one PWM path").
- Script contains `:65:60` on the `-p` argument.
- Existing subtests (drivetemp, Restart=always, no-hddtemp,
  braid-fan-reload oneshot, udev rules) still pass.

**Hotswap test (`fan-control-hotswap`):**
- Oneshot restart chain still works with stubbed daemon (no resolver
  execution path).

**Assertion behavior (manual eval spot-check, optional):**
- Empty `platformDevice` → eval fails with the required-field message.
- `platformDevice = "/sys/devices/platform/f71882fg.656"` → eval fails
  with the identifier-regex message.
- `platformDevice = "f7 88"` → eval fails with the same message.

**Warning behavior:**
- `minFanSpeedPercent = 0` → `nixos-rebuild` prints the warning (use a
  trace test or observe during rebuild).

## Critical files

| File | Action |
|---|---|
| `modules/braid/fan-control.nix` | edit -- swap `pwmPath` for grouped `pwm`, add resolver, add warning |
| `tests/module/fan-control.nix` | edit -- switch test fixture to `pwm.{platformDevice,number}` |
| `tests/module/fan-control.py` | edit -- assert resolver shape |
| `tests/module/fan-control-hotswap.nix` | edit -- switch test fixture to `pwm.{platformDevice,number}` |
| `manual/guides/fan-control.md` | edit -- discovery step, committing-to-Nix recipe, worked example |
| `manual/guides/nixos-configuration.md` | edit -- option table rows, full config example |
| `manual/book/` | rebuild via `mdbook build` (gitignored) |
