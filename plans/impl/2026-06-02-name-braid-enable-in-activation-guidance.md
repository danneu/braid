# Plan: make fan-control + ups activation guidance require `braid.enable = true` (docs + CLI hint)

## Context

`braid.fanControl` is gated on `config = lib.mkIf (cfg.enable && fc.enable)`
(`modules/braid/fan-control.nix:104`, where `cfg = config.braid`,
`fc = cfg.fanControl`). `braid.enable` defaults to `false`
(`modules/braid/options.nix:13`). Both Nix recipes in
`docs/guides/fan-control.md` set `braid.fanControl = { enable = true; ... }`
but never show `braid.enable = true`. A reader who copies a recipe onto a
config that has not separately enabled braid gets nothing -- no service, no
`drivetemp`, no udev rules -- and **no diagnostic**, because the module's
assertions live *inside* the `mkIf` and cannot fire when the gate is closed.

The two well-formed sibling guides already solve this by showing the full
nested block with the master enable visible:

- `docs/guides/monitoring-and-alerts.md:36-49` -- prose ("Monitoring is on by
  default when `braid.enable = true`.") **plus** `braid = { enable = true;
  monitor = {...}; };`
- `docs/guides/power-management.md:38-48` -- `braid = { enable = true;
  autoSuspend = {...}; };`

`docs/guides/ups.md:30-49` has the **same omission** as fan-control. This plan
brings both offenders in line with the house convention.

This is the pivot from the verify-issue finding: the finding's headline fix
(drop a bare `braid.enable = true;` into the recipe) is the wrong shape; the
right shape is to nest the recipe under `braid = { enable = true; <feature> =
{...}; }`, exactly as the two compliant guides do. Nesting makes fan control
read as a sub-feature of an enabled braid module (which it is per
`nixos-configuration.md:42-51`), not a standalone toggle.

## Scope (decided)

- **In:** docs, plus the single user-facing CLI activation hint (same
  single-flag-shorthand root cause -- it *instructs* the operator how to enable):
  - `docs/guides/fan-control.md` -- the fan-control recipes. The guide is the
    *only* fan-control surface needing a fix: grep confirms `braid.fanControl`
    has no command-reference page and no ADR, and its `nixos-configuration.md`
    mentions are already correctly framed under "When `braid.enable = true`".
  - The UPS shorthand recurs across *every* authoritative surface, so the UPS
    correction spans `docs/guides/ups.md`, the `README.md` UPS bullet,
    `docs/commands/ups-status.md`, and the active ADR
    `docs/design/decisions/020-ups-integration.md`.
  - `cli/src/ups.rs` `print_not_enabled` -- the only user-facing CLI string that
    *instructs* setting an enable flag (grep-confirmed singular; doctor's
    "skipped (braid.ups not enabled)" labels and the test/doc-comment mentions
    are not activation instructions). Carries its existing VM test
    `tests/cli/braid-status-ups.py`.
- **Out:** a *module-level* gating diagnostic -- distinct from the CLI hint
  above, which is a user-facing string, not gating logic. The silent no-op is a
  *cross-cutting* pattern -- `fan-control.nix:104`, `monitor.nix:53`,
  `auto-suspend.nix:42`, `ups.nix:59` all gate on `(cfg.enable && <sub>.enable)`,
  and `monitor`/`autoScrub` default their sub-enable to `true`, so a naive
  `sub.enable && !braid.enable` warning would mis-fire for anyone who imports the
  module without enabling braid. A loud module-level fix is a separate,
  decision-doc-backed task, not this pivot. (No `docs/design/decisions/` entry
  currently governs sub-feature gating; that doc would be the home for it.)

## Changes

### 1. `docs/guides/fan-control.md`

**(a) Add a one-line precondition note** under the `## Committing to Nix`
heading (currently line 155), before `### Minimal recipe` (line 157), mirroring
`monitoring-and-alerts.md:36`. ASCII, `--` not em-dash. Reference the existing
in-tree page so `mdbook-linkcheck2` validates it:

> Fan control is a braid sub-feature: it activates only when `braid.enable =
> true` (see [Getting started](getting-started.md)). The recipes below show the
> full `braid` block; merge the non-`braid` lines (`boot.*`,
> `environment.systemPackages`) into your existing config.

**(b) Restructure the Minimal recipe** (current `braid.fanControl = {...}` at
lines 168-176) to nest under `braid`, keeping `boot.*`/`environment.*` as
top-level siblings:

```nix
{ pkgs, ... }:
{
  environment.systemPackages = [ pkgs.lm_sensors ];   # optional: tools for re-running discovery
  boot.kernelModules = [ "coretemp" "nct6775" ];      # your Super I/O driver here
  # boot.kernelParams = [ "acpi_enforce_resources=lax" ];  # only if needed

  braid = {
    enable = true;            # fan control only runs when the braid module is enabled

    fanControl = {
      enable = true;
      pwm = {
        platformDevice = "nct6775.656";
        number = 2;
        minStart = 65;   # from hddfancontrol pwm-test
        maxStop  = 60;   # from hddfancontrol pwm-test
      };
    };
  };
}
```

**(c) Restructure the worked-example "Final Nix config"** (current
`braid.fanControl = {...}` at lines 351-359) the same way:

```nix
boot.kernelModules = [ "coretemp" "f71882fg" "jc42" ];
boot.kernelParams  = [ "acpi_enforce_resources=lax" ];

braid = {
  enable = true;

  fanControl = {
    enable = true;
    pwm = {
      platformDevice = "f71882fg.656";
      number = 2;
      minStart = 65;
      maxStop  = 60;
    };
  };
};
```

No other code block in this guide is a braid config recipe (the lines 49-55
block is discovery tooling, lines 210-220 / 234-246 are shell), so they are
left untouched.

### 2. `docs/guides/ups.md`

**(a) Rewrite the activation contract at line 8** -- currently "Enabling
`braid.ups.enable = true` turns on three behaviors:" -- so it names both gates,
since `ups.nix:59` gates on `(cfg.enable && ups.enable)`. This sentence sits at
the top of the guide and serves as the established precondition (the convention
analog to `monitoring-and-alerts.md:36`); the recipes below then show the same
nesting. New form, e.g.:

> Enabling UPS support (`braid.enable = true` plus `braid.ups.enable = true`)
> turns on three behaviors:

**(b) Restructure the Minimal config** (lines 30-35):

```nix
# configuration.nix
{
  braid = {
    enable = true;
    ups.enable = true;
  };
}
```

**(c) Restructure the driver/port override** (lines 42-49):

```nix
braid = {
  enable = true;

  ups = {
    enable = true;
    name = "myups";
    driver = "apcsmart";
    port = "/dev/ttyS0";
  };
};
```

**(d) Rewrite the two downstream activation sentences** so neither attributes
activation to `braid.ups.enable = true` alone (grep confirms lines 8, 90, 145
are the only prose claims; line 33 is the recipe, handled by (b)):

- Line 90 (TUI panel): "`braid tui`'s Data tab gains a UPS row when
  `braid.ups.enable = true`." -> "... when UPS support is enabled."
- Line 145 (doctor): "`braid doctor` adds two UPS-adjacent checks when
  `braid.ups.enable = true`:" -> "... when UPS support is enabled:"

Referring back to the line-8 contract (rather than repeating both flags in every
sentence) keeps the prose clean and matches the establish-once convention in
`monitoring-and-alerts.md`. After this, no prose sentence presents
`braid.ups.enable = true` as the sole activation gate.

### 3. `README.md`

**Rewrite the UPS feature bullet at line 47** -- currently "with
`braid.ups.enable = true`, NUT drives orderly poweroff ..." -- so it does not
present `braid.ups.enable = true` as the sole gate. README is the brief cookbook
(per AGENTS.md, not reference material), so use the terse flag-agnostic form
that matches the ups.md downstream convention rather than naming both flags:

> - **UPS safety** -- with UPS support enabled, NUT drives orderly poweroff on
>   low battery, mutating commands refuse to start while on battery, and `braid
>   ups status` / the TUI show live UPS state

Line 48's "(when enabled)" dashboard bullet names no flag and is already
correct; line 92's `enable = true;` is inside the `braid = { ... }` config
example and is fine. Line 47 is the only stale shorthand in README.

### 4. `docs/commands/ups-status.md`

**Rewrite the requirement statement at line 8** -- currently "Requires
`braid.ups.enable = true`." -- to name the full contract, since this is where
the command's activation requirement is introduced (single occurrence,
grep-confirmed):

> Requires UPS support enabled (`braid.enable = true` and
> `braid.ups.enable = true`). With UPS disabled the command prints an enable
> hint and exits 0 (not an error).

### 5. `docs/design/decisions/020-ups-integration.md` (status: Active)

This ADR is live architecture authority and its config contract is now
inaccurate; editing it to match the real gate is consistent with AGENTS.md
("any change to behavior or invariants must update those docs"). Two edits:

**(a) Nest the "Proposed config surface" example** (lines 100-106) so the config
shape carries both flags -- this is the one place the ADR establishes config
shape, so it gets the full two-flag contract:

```nix
braid = {
  enable = true;

  ups = {
    enable = true;
    name = "ups";               # identifier used by upsd and upsc
    driver = "usbhid-ups";      # USB default; covers the vast majority of UPSes
    port = "auto";              # usbhid-ups's standard "find the device" value
  };
};
```

**(b) Reword the four prose conditions** that present `braid.ups.enable = true`
as a standalone gate to "UPS support is enabled" (the config-surface example
above already carries the full contract -- same establish-once convention as the
guide). Grep-confirmed these are the only prose occurrences:

- Line 19: "enabling `braid.ups.enable = true` gives a home NAS three specific
  guarantees" -> "enabling UPS support gives a home NAS three specific
  guarantees"
- Line 71: "When `braid.ups.enable = true`, `braid add` ..." -> "When UPS
  support is enabled, `braid add` ..."
- Line 85: "Under `braid.ups.enable = true`, this silent-degradation path is
  unsafe" -> "When UPS support is enabled, this silent-degradation path is
  unsafe"
- Line 87: "... whenever `braid.ups.enable = true`. ... fires only when
  `systemd_lifecycle = true` and `braid.ups.enable = true`." -> "... whenever
  UPS support is enabled. ... fires only when `systemd_lifecycle = true` and UPS
  support is enabled."

(Out of scope: the line-25 "before this decision flips to Active" remark vs. the
`status: Active` frontmatter is a pre-existing inconsistency unrelated to the
activation-gate wording; left untouched.)

### 6. `cli/src/ups.rs` + `tests/cli/braid-status-ups.py` (CLI not-enabled hint)

`braid ups status` against a config with UPS off prints a human hint
(`print_not_enabled`, `cli/src/ups.rs:162-174`) that currently instructs only
the single flag -- which can steer an operator straight back into the silent
no-op. This is the only user-facing CLI string that *instructs* enabling
(grep-confirmed); the JSON path emits just the `ups_not_enabled` sentinel (no
hint text) and is untouched.

**(a) Rewrite the hint at `ups.rs:168-171`** to name both flags (ASCII, `--` not
em-dash, per CLI output style), preserving the existing tail:

> UPS support is not enabled. Set `braid.enable = true` and
> `braid.ups.enable = true` in your NixOS configuration and rebuild to enable
> preflight safety and low-battery shutdown.

**(b) Strengthen the VM test assertion** at `tests/cli/braid-status-ups.py:235`.
The new hint still *contains* `braid.ups.enable = true`, so the existing
substring check would pass without protecting the fix. Add a `braid.enable =
true` substring check so the test locks the two-flag hint:

```python
assert "braid.enable = true" in out_no_ups and "braid.ups.enable = true" in out_no_ups, (
    f"expected two-flag enable hint (braid.enable + braid.ups.enable) on stdout, got: {out_no_ups!r}"
)
```

Also update the inline comment (lines 222-225) to note *both* flag substrings are
stable. (`braid.enable = true` is not a substring of `braid.ups.enable = true` --
the `.ups.` segment breaks contiguity -- so the new check is meaningful.) No Rust
unit test snapshots the human string (the ups.rs tests cover only the JSON
`ups_not_enabled` path), so none regress.

## Files

- `docs/guides/fan-control.md` -- 1 precondition note + 2 recipe restructures.
- `docs/guides/ups.md` -- 1 activation-contract rewrite (line 8) + 2 recipe
  restructures (lines 30-35, 42-49) + 2 downstream-sentence rewrites (lines
  90, 145).
- `README.md` -- 1 feature-bullet rewrite (line 47).
- `docs/commands/ups-status.md` -- 1 requirement-statement rewrite (line 8).
- `docs/design/decisions/020-ups-integration.md` -- nest the config-surface
  example (lines 100-106) + 4 prose-condition rewrites (lines 19, 71, 85, 87).
- `cli/src/ups.rs` -- rewrite the `print_not_enabled` human hint (lines 168-171)
  to name both flags.
- `tests/cli/braid-status-ups.py` -- strengthen the not-enabled assertion (line
  235) to require both flag substrings; update the inline comment (lines
  222-225).

## Convention reference (match exactly)

- `docs/guides/monitoring-and-alerts.md:36-49`
- `docs/guides/power-management.md:38-48`

## Verification

1. `mdbook build docs` -- must pass. `mdbook-linkcheck2` (configured in
   `docs/book.toml`) validates the new `getting-started.md` cross-link; a broken
   link fails the build.
2. Manual diff review: confirm each restructured recipe is valid Nix (balanced
   braces, `braid.enable = true;` present, `boot.*`/`environment.*` left at top
   level) and that the nesting matches the monitoring/power-management blocks.
3. Confirm no remaining `braid.fanControl = {` or `braid.ups` recipe in either
   guide omits the enclosing `braid.enable = true` (grep the two files).
4. `rg -n 'braid\.ups\.enable = true' docs/ README.md` -- the only acceptable
   remaining matches are the two full-contract statements that also name
   `braid.enable = true`: the ups.md line-8 activation contract and the
   ups-status.md requirement line. Everywhere else -- the restructured recipe
   blocks (now `braid = { enable = true; ups = {...}; }`), README, and ADR 020
   -- must show zero matches. No standalone single-flag activation or condition
   claim may survive in any docs file.
5. CLI hint: confirm `cli/src/ups.rs` `print_not_enabled` names both
   `braid.enable = true` and `braid.ups.enable = true`, then run
   `just test-vm braid-status-ups` (exercises the human not-enabled path and the
   strengthened two-flag assertion). Run `just test-rust` as a cheap
   confirmation that the JSON-path ups unit tests/snapshots are unaffected.

## Suggested commit

`fix: name braid.enable alongside braid.ups.enable in activation guidance`

(`fix:` rather than `docs:` -- the user-facing `print_not_enabled` hint change is
the behaviorally significant part. Body should note the full UPS sweep -- guide
recipes + prose, README bullet, `ups-status.md` requirement line, ADR 020's
config-surface nesting + prose rewrites -- plus the fan-control recipe
restructures and the CLI hint + test. Split into a `docs:` commit and a `fix:`
commit if you prefer separating the CLI behavior change from the docs.)
