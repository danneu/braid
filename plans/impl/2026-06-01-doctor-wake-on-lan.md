# Research: Wake-on-LAN support in `braid doctor`

Date: 2026-06-01
Status: Research / proposal (no code written)

## TL;DR

WoL already exists in braid, but only as **build-time NixOS config**:
`braid.autoSuspend.wolInterface` -> `networking.interfaces.<iface>.wakeOnLan.enable`
(`modules/braid/auto-suspend.nix`). A build-time assertion guarantees the *option*
is set whenever `autoSuspend.enable` is true, and refuses WiFi interfaces.

What is **not** verified anywhere is the **runtime** state of WoL on the NIC.
That is the exact gap a `braid doctor` check fills: confirm the live NIC actually
reports magic-packet wake (`ethtool <iface>` -> `Wake-on: g`). Today nothing in
braid ever runs `ethtool`; the guide tells operators to run it by hand
(`docs/guides/power-management.md` step 3).

The failure this catches is real and serious: `autoSuspend.enable = true` + NIC
showing `Wake-on: d` means the NAS suspends after `idleTime` and is unreachable
until someone physically presses the power button. The build assertion cannot see
this -- it only proves the NixOS option is set, not that the driver/BIOS honor it.

## What already exists (and what it doesn't cover)

- `modules/braid/auto-suspend.nix`
  - `autoSuspend.wolInterface` (nullOr str), asserted non-null when
    `autoSuspend.enable`, asserted not-WiFi (`wl*` prefix).
  - Sets `networking.interfaces.${iface}.wakeOnLan.enable = true`, which NixOS
    realizes as a systemd `.link` file `[Link] WakeOnLan=magic`
    (confirmed by `tests/module/braid-auto-suspend.py:127-133`, which reads
    `/etc/systemd/network/40-eth0.link`).
- `docs/design/decisions/016-auto-suspend.md` ("WoL managed by braid"):
  explicitly states the BIOS-side WoL setting is the user's responsibility and
  "can't be automated from NixOS".
- `docs/guides/power-management.md`: the entire manual troubleshooting chain
  (BIOS ErP/Deep Sleep, `ethtool <iface> | grep Wake-on`, driver swap for
  RTL8125, PCI bridge PME). A doctor check automates step 3 of that chain.

**Gaps the build assertion cannot close** (all surface only at runtime):
1. NIC/driver does not support magic-packet WoL (`Supports Wake-on:` lacks `g`).
2. WoL supported but disabled at runtime (`Wake-on: d`) -- BIOS ErP, driver
   default, or a driver that resets WoL on resume.
3. Interface renamed/removed since the config was written.

## Mechanism: how to read WoL state

- WoL mode is **not** in sysfs. The only interface is the ethtool ioctl/netlink
  `GWOL`. So the check must shell out to `ethtool <iface>` (consistent with how
  braid already shells out to btrfs/cryptsetup/smartctl/upsc via `CommandRunner`).
- **Privilege:** reading WoL requires `CAP_NET_ADMIN`. Verified against the pinned
  kernel:
  - ioctl: `ETHTOOL_GWOL` is absent from the unprivileged allowlist in
    `reference/linux/net/ethtool/ioctl.c:3260-3297`, so it hits
    `default: if (!ns_capable(net->user_ns, CAP_NET_ADMIN)) return -EPERM;`
    (lines 3298-3300).
  - netlink: `ETHTOOL_MSG_WOL_GET` carries `.flags = GENL_UNS_ADMIN_PERM`
    (`reference/linux/net/ethtool/netlink.c:1200-1201`).
  - `braid doctor` already runs as root (`sudo braid doctor`; it runs `smartctl`,
    which also needs root), so the requirement is satisfied. A non-root run would
    get EPERM from ethtool -> non-zero exit (handle as a check failure, not a panic).
- **Output to parse** (stable, and `RealRunner` already forces `LC_ALL=C` so the
  English labels are guaranteed -- `cli/src/cmd.rs:1308`):
  ```
  Supports Wake-on: pumbg
  Wake-on: g
  ```
  Decision table:
  | Observed | Meaning | Proposed status |
  | --- | --- | --- |
  | `Wake-on:` contains `g` | active, magic-packet wake armed | Ok |
  | `Wake-on:` lacks `g` (e.g. `d`) but `Supports Wake-on:` has `g` | configured in NixOS, NIC reports it off (BIOS ErP/Deep Sleep, driver reset, not rebuilt) | **Fail** (configured-but-off) |
  | `Supports Wake-on:` lacks `g` | NIC/driver cannot do magic-packet WoL at all | **Fail** (unsupported) |
  | ethtool non-zero / not found | interface gone, not root, or spawn failure | **Fail** (cannot verify the wake path) |
  | ethtool exit 0 but no parseable `Wake-on:` line | output drift / unexpected format | **Fail** ("could not parse ethtool output" -- fail closed; never silently read as ok/disabled/unsupported) |
  - Prefer text parsing of the `Wake-on:` line over `ethtool --json`: the text
    form is rock-stable, locale-pinned under `LC_ALL=C`, and is what the repo's
    own docs/tests already key on. (`--json` WoL coverage varies by ethtool
    version; not worth the version risk for a one-line field.)

## Implementation sketch (follows the `ups` / `fan_control` precedents exactly)

### 1. Plumb the interface name into config.json
The CLI has no idea which interface to check today -- `wolInterface` lives only in
the Nix module. Mirror how `ups`/`fan_control` are emitted.

`modules/braid/cli.nix` (add to the `//` chain):
```nix
// lib.optionalAttrs cfg.autoSuspend.enable {
  auto_suspend = {
    wol_interface = cfg.autoSuspend.wolInterface;  # assertion guarantees non-null here
  };
}
```

`cli/src/config.rs`:
```rust
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoSuspend { pub wol_interface: String }
// add `auto_suspend: Option<AutoSuspend>` to Config + RawConfig (+ accessor),
// exactly like `ups`. deny_unknown_fields per the nested-struct convention.
```

### 2. New `CmdRequest` + argv
`cli/src/cmd.rs`:
```rust
EthtoolShow { interface: String },
// to_argv: CmdArgs { program: "ethtool".to_owned(), args: vec![interface.clone()] }
```

### 3. Package ethtool as a pinned, overrideable tool (FOUR sites, not one)
`RealRunner` resolves programs via PATH (`Command::new(&cmd.program)`), and both
braid wrappers **prepend** their tool path (`braid-wrapper.sh:2`
`export PATH="@toolPath@:$PATH"`; the flake wrapper uses `makeWrapper --prefix
PATH`). ADR-010 requires every new parsed tool to be a pinned, overrideable
`braid.packages.*` entry and documents *two* wrapping sites. `braid.packages.ethtool`
is the only path; it must be wired into all four places below, or
`nix run .#braid doctor` (and the override-based VM test in Testing) will miss it:
1. `modules/braid/options.nix` -- add
   `ethtool = lib.mkPackageOption pkgs "ethtool" { };` under `braid.packages`.
2. `modules/braid/wrapper.nix` -- add `cfg.packages.ethtool` to `toolPackages`
   (module wrapper, for deployed NixOS systems where the option may be overridden).
3. `flake.nix` top-level `toolPath` (currently `flake.nix:43-50` -- lists
   cryptsetup/btrfs-progs/util-linux/systemd/smartmontools/nut, **no ethtool**) --
   add `pkgs.ethtool` (for `nix run` and the fixed flake checks/tests).
4. `flake.nix` `nixosModules.default` pinned defaults (currently
   `flake.nix:1005-1011`, **no ethtool**) -- add
   `ethtool = lib.mkDefault braidPkgs.ethtool;`.

### 4. The check (mirror `check_ups_daemon_up`)
`cli/src/doctor.rs` -- gate on config + autoSuspend, then a **pure** summarizer
fed by a thin impure probe (the `summarize_declared_disks` pattern):
```rust
fn check_wake_on_lan<R: CommandRunner>(ctx) -> CheckResult {
    let name = "wake_on_lan";
    let Some(config) = ctx.config.as_ref() else { return skip(name, "config not available") };
    let Some(auto) = config.auto_suspend() else {
        return skip(name, "skipped (braid.autoSuspend not enabled)");
    };
    // run EthtoolShow { interface }, then hand stdout + exit to a pure
    // summarize_wol(iface, stdout, exit) -> CheckResult (unit-tested).
}
```
Register it in `run_doctor` after the UPS checks; add the human label
`"wake_on_lan" => "wake-on-lan"` (11 chars, fits the `<14` column).

## Testing (this is the load-bearing constraint)

**Live ethtool cannot be exercised in a VM.** `tests/module/braid-auto-suspend.py:130`
already records why: "ethtool can't be tested in QEMU -- virtual NICs don't
support real WoL." Plan around that:

1. **Unit tests (primary, behavioral, structure-insensitive).** `MockRunner`
   returns golden `ethtool <iface>` text for each branch:
   - `Wake-on: g` -> Ok
   - `Wake-on: d`, `Supports Wake-on: pumbg` -> Fail (configured-but-off)
   - `Supports Wake-on: d` (no `g`) -> Fail (unsupported)
   - non-zero exit / spawn error -> Fail
   - exit 0 but no parseable `Wake-on:` line (line missing, or value unrecognized)
     -> Fail ("could not parse ethtool output"). Fail-closed: parser/output drift
     must never be silently read as disabled/unsupported/ok, matching the
     `braid idle` Unknown->Busy convention in ADR-016 and the AGENTS.md
     "fail-closed policy from the downstream failure mode" heuristic.
   - autoSuspend absent in config -> Skip
   Keep `summarize_wol` pure so these assert on a string in/CheckResult out, same
   shape as the existing `summarize_declared_disks` / `summarize_smart_selftest`
   tests.
2. **VM test (integration, limited).** Two things are testable without a real NIC:
   - config.json carries `auto_suspend.wol_interface` when `autoSuspend.enable`
     (extend `braid-auto-suspend.py`, which already reads the generated config).
   - doctor's row behaves when ethtool is **stubbed via a package override**:
     set `braid.packages.ethtool` to a fake package whose `bin/ethtool` prints
     canned `Wake-on: g` / `d`, run `braid doctor --json`, assert the
     `wake_on_lan` status. A `/tmp/ethtool` on ambient PATH does **not** work:
     both wrappers prepend their pinned `toolPath` (`braid-wrapper.sh:2`), so the
     pinned ethtool always shadows an ambient stub. Overriding
     `braid.packages.ethtool` injects the fake into the wrapper's own toolPath,
     which is the supported override seam (ADR-010 "operational escape hatch").
   - The Skip path (autoSuspend disabled) needs no NIC at all.
3. **Golden fixture.** If a fixture is added, it is **stable-only / hand-authored**
   (like `smartctl-selftest-*.json`), because live capture is impossible in a VM.
   See `cli/tests/fixtures/nixos-26.05/README.md`.

**Parser-contract authority (must be updated -- ADR-010 mandates classification).**
ethtool becomes a new parsed, pinned tool, and ADR-010:33 ("New runtime
dependencies must be classified into one of these two groups when added")
makes updating the contract authority a required task, not optional cleanup:
- Add an ethtool row to the ADR-010 pinned-tools table
  (`docs/design/decisions/010-toolchain-pinning.md`): Pinned / overrideable,
  reason "`Wake-on:` line parsed by the doctor `wake_on_lan` check".
- Add ethtool to the Principle 10 tool list (`docs/design/principles.md:57-59`,
  which currently enumerates btrfs-progs/cryptsetup/util-linux/NUT/smartmontools).
- Add ethtool to AGENTS.md "Parser Compatibility" and the parser-critical
  tool-version list, **explicitly documenting it as hand-fixtured / no-live-capture**
  (virtio NICs don't emit WoL), exactly like the smartctl-selftest fixtures.
- Add `braid.packages.ethtool` to the option table in
  `docs/guides/nixos-configuration.md` (~lines 68-70).
- Extend the `tool-versions` VM test with **two distinct assertions**, matching
  how the test already separates concerns: `tool-versions.nix:34-42` puts pinned
  tools on the VM *system* PATH, and `tool-versions.py` does direct
  `--version`/`command -v` provenance against that PATH (lines 11-48), with a
  *separate* wrapper empty-PATH subtest for upsc only (lines 72-94).
  1. **Direct provenance + version** (identical to the existing tools): add
     `pkgs.ethtool` to `environment.systemPackages` and
     `ethtool = pkgs.ethtool.version` to the `expected-versions` map in
     `tool-versions.nix`; add `ethtool` to the `command -v` provenance loop and an
     `ethtool --version` assertion in `tool-versions.py`. This is what catches an
     ambient ethtool shadowing the pinned one on the system PATH (the test's
     stated purpose). It does *not* require the wrapper.
  2. **Wrapper spawn proof** (mirror `assert_wrapper_finds_upsc`): run the braid
     wrapper with `PATH=/nonexistent` against a config that sets
     `auto_suspend.wol_interface` (so doctor actually invokes ethtool), and assert
     the `wake_on_lan` check produced a real parsed result (`ok`/`warn`/`fail`),
     **not** a spawn-failure message -- proving `cfg.packages.ethtool` is wired
     into the wrapper toolPath. The wrapper subtest cannot expose
     `ethtool --version`, which is exactly why version lives in (1), not here.

ethtool cannot join the live-capture lanes (`just test-parsers`, fixture capture)
for the same virtio reason; it is a hand-authored, stable-only parser. The
`Wake-on:` line is about as stable as tool output gets, so ongoing risk is low.

## Decisions (settled)

1. **Severity: `Fail`** whenever `autoSuspend` is enabled and WoL is not provably
   armed -- the single rule covers configured-but-off (`Wake-on: d`), unsupported
   (`Supports Wake-on:` lacks `g`), and unverifiable (ethtool non-zero / spawn
   failure, or exit 0 with unparseable output). The `wolInterface` requirement
   exists precisely because suspend without working WoL strands the NAS; a runtime
   failure defeats the exact guarantee the build assertion provides. Mirrors
   `braid_online_active` (Fail when a configured automatic safety path is broken),
   and makes `braid doctor` exit non-zero for all these states.
   (Considered and overridden: a running `Wake-on: g` does not *prove*
   wake-from-S3 works -- BIOS ErP / PCI-bridge PME are out of band -- so `Ok` is
   necessary-not-sufficient. That residual risk stays in the troubleshooting
   guide; it does not justify softening any failure state to `Warn`.)
2. ~~ethtool packaging~~ **Resolved (review):** not a choice. ADR-010 requires a
   pinned, overrideable `braid.packages.ethtool` wired into all four sites in
   step 3 above; bare `pkgs.ethtool` is not an option.

## Pivot worth considering: a pre-suspend WoL gate (more robust for the core risk)

The doctor check is on-demand: it only helps if the operator runs it. The actual
risk -- suspending into an unreachable state, *especially after a driver resets
WoL on resume* -- is better closed by a gate that **blocks suspend** when WoL is
not armed. braid already has the exact pattern: `braid idle` is wired into
autosuspend as an `ExternalCommand` check (`auto-suspend.nix` `checks.BraidPool`).
A sibling `WolActive` check (or a `systemd` pre-sleep hook) that returns "activity"
when `ethtool <iface>` lacks `g` would keep the NAS awake rather than letting it
strand. That is the higher-value change for reliability; the doctor check remains
valuable as the diagnostic ("doctor told me WoL is off, and why").

Recommendation: ship the doctor check (it's what was asked, low blast radius, and
gives the operator a single command that automates power-management.md step 3),
and separately consider the suspend gate as the durable safety net.

## Docs to update (per AGENTS.md "keep both in sync")

- `docs/commands/doctor.md`: add a `wake_on_lan` row to the check table + a line
  in "What it checks".
- `docs/guides/power-management.md`: replace/augment the manual step-3 `ethtool`
  instructions with "run `sudo braid doctor`".
- `README.md`: if the doctor check list is mirrored there.
- **Parser-contract authority (required -- see Testing, not optional):**
  `docs/design/decisions/010-toolchain-pinning.md` (pinned-tools table),
  `docs/design/principles.md` (Principle 10 list), AGENTS.md "Parser
  Compatibility", and the `braid.packages.ethtool` row in
  `docs/guides/nixos-configuration.md`.
- `docs/design/decisions/016-auto-suspend.md`: one-line note that doctor now
  verifies runtime WoL state.

## Files touched (estimate)

- `cli/src/config.rs` (+ `AutoSuspend` struct, accessor)
- `cli/src/cmd.rs` (+ `EthtoolShow` variant + argv arm)
- `cli/src/doctor.rs` (+ check, + pure summarizer, + label, + register, + unit tests)
- `modules/braid/cli.nix` (+ `auto_suspend` block)
- `modules/braid/options.nix` + `modules/braid/wrapper.nix` (ethtool option + toolPackages)
- `flake.nix` -- top-level `toolPath` (~43-50) **and** `nixosModules.default`
  pinned defaults (~1005-1011); both currently omit ethtool
- `tests/module/braid-auto-suspend.py` (config.json assertion; package-override stub test)
- `tests/cli/tool-versions.nix` + `tests/cli/tool-versions.py` (ethtool provenance/version,
  via the wrapper sub-test pattern)
- docs as above (incl. ADR-010, principles.md, AGENTS.md, nixos-configuration.md)

## Follow Up

- Add a pre-suspend WoL gate in `modules/braid/auto-suspend.nix` or an equivalent sleep hook so autosuspend blocks when `ethtool <iface>` does not report `Wake-on: g`.
