# UPS Integration Implementation Plan

## Context

braid needs first-class UPS support so enabling `braid.ups.enable = true` gives a home NAS three guarantees: (1) orderly shutdown before battery exhaustion for ordinary mounted operation, (2) preflight refusal to start pool-mutating commands while already on battery, (3) live UPS state visible in `braid ups status` and the TUI. Mid-mutation power loss is a supported recovery case handled by the existing journal + `braid recover` path, proven by VM tests per mutation class.

Integrating UPS conditions into the shared alert model is **deferred to a future ADR**. Decision 014 currently guarantees "alerts stay latched until `braid ack`" -- that's the right shape for event-driven causes (disk errors, smartd), but wrong for live-state conditions like on-battery / low-battery (users expect those to clear when the UPS returns to OL). Making alerts auto-dismiss for UPS would require splitting `AlertCause` by persistence semantics (`LatchedUntilAck` vs. `ActiveWhileConditionHolds`) and updating `merge_into_latch`, `ack`, `status`, and the tests. That's a core-invariant change that deserves its own ADR; smuggling it into UPS v1 would conflate two distinct concerns.

Decision record: [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md) (Status: Draft).

Outcome of this plan: ship UPS v1 (state + shutdown + preflight) behind `braid.ups.enable`. Flip the ADR to Active once M10-M14 VM tests pass. Alert integration lives in a separate future plan and ADR.

## Pending ADR refinements (do before Milestone 1)

Refine [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md) before coding starts:

- **Remove the "UPS events become first-class alert causes" section entirely.** Replace with a short paragraph noting that alert-model integration is deferred to a future ADR, because live-state UPS conditions don't fit decision 014's "latched until ack" invariant and reconciling them requires a broader alert-model change (splitting `AlertCause` by `LatchedUntilAck` vs. `ActiveWhileConditionHolds`). Splitting that work out of UPS v1 keeps both changes honest.
- **Update guarantee (3) in the Context.** Change from "UPS events latched into braid's shared alert model" to "live UPS state visible in `braid ups status` and the TUI; live UPS status is used for preflight safety and upsmon critical-state shutdown (normally `OB` + `LB`)."
- **Drop the `/var/lib/braid/ups-alerts/` latch-file design.** Not used in v1 since there's no alert integration at all.
- Keep the "Upsmon credential lifecycle" subsection as written (credential generation is still needed because NUT requires upsmon<->upsd auth even in standalone mode).
- Keep the "`braid-online` becomes safety-critical under UPS" subsection -- `braid doctor` still flags "pool mounted but `braid-online` inactive" under UPS, and that's a configuration check, not an alert cause.

Also in the same PR, refine the cross-cutting docs the ADR already calls for:

- [`docs/decisions/010-toolchain-pinning.md`](../../docs/decisions/010-toolchain-pinning.md) -- add `nut` (NUT, Network UPS Tools; `pkgs.nut`) to the pinned parser-critical list.
- [`docs/principles.md`](../../docs/principles.md) -- principle 10 (Pinned Toolchain) names NUT alongside btrfs-progs/cryptsetup/util-linux.
- [`AGENTS.md`](../../AGENTS.md) -- parser-compatibility section names NUT; reference-source table gains a row for `reference/nut/` with pointers to upstream `upsc`, `upsmon`, `upsrw`, and `dummy-ups` source paths.
- NUT source is already available at [`reference/nut/`](../../reference/nut/) (fetched via `scripts/fetch-references.py`). Consult `reference/nut/clients/upsmon.c`, `upsc.c`, `upsrw.c`, and `reference/nut/conf/*.sample` when implementing or debugging.

## Design summary

### Scope (v1)

Live UPS state is surfaced in `braid ups status` and the TUI. Live UPS status is consulted directly by preflight when a user runs a mutation, and by `upsmon`'s own `SHUTDOWNCMD` wiring when `upsmon` declares the UPS critical (typically `OB` + `LB` together; see [`reference/nut/clients/upsmon.c:1404`](../../reference/nut/clients/upsmon.c)). **No `AlertCause` integration, no `NOTIFYCMD`, no monitor-lifecycle changes.**

### Shutdown path

- `power.ups.upsmon.settings.SHUTDOWNCMD = "${pkgs.systemd}/bin/systemctl poweroff"` (overrides nixpkgs' default of `shutdown now` using `mkForce` or a plain assignment -- nixpkgs uses `mkDefault`).
- systemd shutdown sequence unwinds `braid-online.service` ([decision 018](../../docs/decisions/018-systemd-lifecycle.md)) -> btrfs umount -> luks close.

### Credential lifecycle

- `braid-ups-secrets.service` (oneshot): if `/var/lib/braid/upsmon.pass` is absent, writes `head -c 24 /dev/urandom | base64` with `0600 root:root`.
- `before = [ "upsd.service" "upsmon.service" ]` and `requiredBy = [ "upsd.service" "upsmon.service" ]`.
- `power.ups.users.<name>.passwordFile` and `power.ups.upsmon.monitor.<name>.passwordFile` both reference the file.
- `/var/lib/braid/` already created by `storage.nix:22` tmpfiles -- no new directory needed.

### `braid ups status` shape

- `Commands::Ups(UpsArgs)` with `Status { json: bool }` subcommand.
- Reads `/etc/braid/config.json` for `ups` block; if absent or `ups.enable=false`, prints a helpful enable-hint and exits 0.
- Otherwise invokes `upsc <name>`, passes through `parse_upsc`, renders curated human summary or `serde_json::to_string_pretty(&UpscOutput)` with `--json`.
- Daemon down (upsc command fails) renders as a distinct error message with exit 1 and `{"error": "daemon_down"}` under `--json`.

### Preflight on battery

- New `check_ups_not_on_battery(runner, ups_name)` in [`cli/src/preflight.rs`](../../cli/src/preflight.rs). Called from `add`, `remove`, `remove-missing`, `replace` preflights, before journal write.
- Returns `Validation("cannot verify UPS is on utility power -- refusing to start <op>. Check 'braid ups status', restore utility power, then retry.")` per each command's `Validation` variant. (Fail-closed wording stays honest when the real failure is daemon-down or malformed status, not just an on-battery condition.)
- Check is a no-op when `ups` block is absent from config.

### TUI UPS section

- Lives in the Data tab alongside Fans (parallel, not nested). `cli/src/tui/view/mod.rs` adds `ups_section`.
- Polling mirrors fan probe: `Effect::ProbeUps { name }` + `Effect::ScheduleUpsProbe { delay }` on the same 5s cadence ([`cli/src/tui/app.rs:17,43-51`](../../cli/src/tui/app.rs)).
- Colors: `OL` Green, `OB` Yellow, `LB` Red, `TESTFAIL`/`COMMBAD` Red, daemon-down DarkGray (matches Fan `DaemonStatus` pattern at `cli/src/tui/view/mod.rs:149-157`).
- `ups.status` parses as a `HashSet<UpsStatusFlag>`; unknown tokens preserved via `UpsStatusFlag::Unknown(String)`.
- Watts displayed only when both `ups.load` and `ups.realpower.nominal` present; labeled "estimated"; otherwise omitted.

## Milestones

M1-M9 are impl; M10a-M14 are the VM-test proof obligations that flip the ADR to Active.

### M1 -- Module skeleton + `power.ups` wiring + package pin

- Create [`modules/braid/ups.nix`](../../modules/braid/ups.nix).
  - `braid.ups.{enable, name, driver, port}` options (`name` default `"ups"`, `driver` default `"usbhid-ups"`, `port` default `"auto"`).
  - Wire `power.ups.*`: `enable = true`, `mode = "standalone"`, `ups.<name>.{driver, port}`, `upsmon.monitor.<name>.{system = "<name>@localhost", powerValue = 1, type = "primary", passwordFile}`. Production creates exactly one upsmon user with `upsmon = "primary"`, no `actions`, no `instcmds` (per [`reference/nut/docs/man/upsd.users.txt:78`](../../reference/nut/docs/man/upsd.users.txt), the `SET` action is only needed by `upsrw` clients; the shipped credential should not carry it). `actions = [ "SET" ]` users are a test-only concern provisioned in `tests/module/lib/ups-fixture.nix`, not in production.
  - `power.ups.package = cfg.packages.nut`.
  - Assertion: `cfg.ups.enable -> cfg.ups.name != ""`.
- Modify [`modules/braid/default.nix:1`](../../modules/braid/default.nix) -- add `./ups.nix` to imports.
- Modify [`modules/braid/options.nix:16-18`](../../modules/braid/options.nix) -- add `nut = lib.mkPackageOption pkgs "nut" {};`.
- Modify [`modules/braid/cli.nix:13-31`](../../modules/braid/cli.nix) -- emit `ups = { enable = cfg.ups.enable; name = cfg.ups.name; }` into config.json when `cfg.ups.enable`.
- Modify [`cli/src/config.rs:13-54`](../../cli/src/config.rs) -- add `Ups { enable: bool, name: String }` as an optional field on `Config`, mirroring `FanControl`.
- **Verify**: `braid.ups.enable = true` in a VM boots `upsd.service` and `upsmon.service` without error; `upsc ups` returns the dummy UPS's fields.

### M2 -- `parse_upsc` + fixtures + golden tests

- Create [`cli/src/parse/upsc.rs`](../../cli/src/parse/upsc.rs) with `pub fn parse_upsc(raw: &RawCommandOutput) -> UpscOutput`.
  - `UpscOutput { status_flags: HashSet<UpsStatusFlag>, battery: BatteryFields, load_pct: Option<u8>, realpower_nominal_watts: Option<u32>, input: InputFields, test_result: Option<String>, device: DeviceFields, extra: BTreeMap<String, String> }`.
  - `UpsStatusFlag` enum: `Ol`, `Ob`, `Lb`, `Rb`, `Hb`, `Chrg`, `Dischrg`, `Cal`, `Bypass`, `Off`, `Over`, `Trim`, `Boost`, `Fsd`, `Unknown(String)`.
- Modify [`cli/src/parse/mod.rs`](../../cli/src/parse/mod.rs) -- `pub mod upsc; pub use upsc::parse_upsc;`.
- Modify [`cli/src/parse/types.rs`](../../cli/src/parse/types.rs) or sibling -- add the new types.
- Modify [`cli/src/cmd.rs`](../../cli/src/cmd.rs) -- add `CmdRequest::UpscQuery { name: String }`.
- Create `tests/capture-ups-fixtures.nix` -- boots NUT with `dummy-ups` (pointing at a `.seq` file), rewrites it to each target state, captures `upsc ups` output.
  - Fixtures: `upsc-online.txt`, `upsc-onbattery.txt`, `upsc-lowbattery.txt`, `upsc-replace-battery.txt`, `upsc-daemon-down.stderr`.
  - Separate VM derivation from `capture-tool-fixtures.nix` -- different setup, cleaner separation.
  - Add matching `just capture-ups-fixtures` recipe and call it from `just capture-all-fixtures`.
- Modify [`cli/tests/golden_nixos_25_11.rs`](../../cli/tests/golden_nixos_25_11.rs) -- golden tests for each fixture using the existing `support/golden_common.rs` pattern.
- **Verify**: unit tests pass; `just test-rust` green; fixtures commit-clean.

### M3 -- `braid ups status` CLI

- Create [`cli/src/ups.rs`](../../cli/src/ups.rs) -- `cmd_ups_status(args, paths, runner) -> Result<(), UpsError>`.
  - Reads `config.rs` for `ups` block; missing -> hint message, exit 0.
  - Runs `UpscQuery { name }`; subprocess failure -> "ups daemon not running" message + exit 1 (`{"error": "daemon_down"}` in JSON mode).
  - Success -> `parse_upsc` -> render human or JSON.
  - Human render: decoded status line, battery charge %, runtime formatted `HH:MM`, load %, estimated watts if computable, input voltage with transfer-low/high context, device model, battery mfr date, last test result.
- Modify [`cli/src/main.rs:23-64`](../../cli/src/main.rs) -- add `Ups(UpsArgs)` variant with `Status { #[arg(long)] json: bool }`.
- Modify [`modules/braid/wrapper.nix:11`](../../modules/braid/wrapper.nix) -- add `cfg.packages.nut` to `toolPackages` so `upsc` resolves in the wrapped binary.
- **Verify**: `braid ups status` works against a dummy UPS VM; `braid ups status --json` parses with `jq`; absence of UPS config prints the hint.

### M4 -- TUI UPS section

- Modify [`cli/src/tui/model.rs:13-45,87-94`](../../cli/src/tui/model.rs) -- add `UpsSnapshot { flags, battery_charge_pct, runtime_secs, load_pct, watts_estimated, daemon: DaemonStatus, probed_at }`, `ups: Option<UpsSnapshot>`, `ups_probe_inflight: bool`.
- Modify [`cli/src/tui/effect.rs:33-39`](../../cli/src/tui/effect.rs) -- `Effect::ProbeUps { name }`, `Effect::ScheduleUpsProbe { delay }`.
- Modify [`cli/src/tui/app.rs:43-51,86-93`](../../cli/src/tui/app.rs) -- `fn ups_probe_effect()` + `Message::UpsProbeFinished(UpsSnapshot)`; `r` key refreshes both pool and ups; `UPS_PROBE_INTERVAL = Duration::from_secs(5)` matching fans.
- Modify [`cli/src/tui/probe.rs`](../../cli/src/tui/probe.rs) -- `fn probe_ups_for_tui(name) -> UpsSnapshot` invoking `upsc` + `parse_upsc`.
- Modify [`cli/src/tui/view/mod.rs:93-206,831-843`](../../cli/src/tui/view/mod.rs) -- new `ups_section(snapshot)` rendered conditional on `ups_config.is_some()`.
- **Verify**: `braid tui` shows UPS section; colors switch as `upsrw -s 'ups.status=OB LB' -u <user> -p <pass> <upsname>@localhost` changes state in a dummy VM.

### M5 -- Alert integration (DEFERRED to future ADR + plan)

Not in UPS v1. UPS state is surfaced via `braid ups status` / TUI / preflight only. A separate future ADR covers splitting `AlertCause` by persistence semantics and integrating UPS conditions into the shared alert model. This number is kept so later milestones stay aligned with prior plan versions.

### M6 -- Preflight reject on battery

- Modify [`cli/src/preflight.rs:15-321`](../../cli/src/preflight.rs) -- `fn check_ups_not_on_battery(runner, ups_name: Option<&str>) -> PreflightResult`. When `ups_name` is `Some`, invokes `upsc` + `parse_upsc`; returns error if `OB` or `LB` in status flags. `OFF`/comms failure is a different failure mode -- treat as "cannot determine; refuse."
- Modify [`cli/src/add.rs:320`](../../cli/src/add.rs), [`cli/src/remove.rs`](../../cli/src/remove.rs), [`cli/src/remove_missing.rs`](../../cli/src/remove_missing.rs), [`cli/src/replace.rs`](../../cli/src/replace.rs) -- wire `ups_name` from config through `require_mutation_preflight` (or equivalent sibling entry point).
- Error message (covers OB, LB, OFF, and comms-failure with one message so wording stays honest when the cause is uncertain): `"cannot verify UPS is on utility power -- refusing to start <op>. Check 'braid ups status', restore utility power, then retry."` Mapped to each command's `Validation` variant.
- **Verify**: unit test with mocked runner; VM test that `braid add` exits `Validation` while `dummy-ups` is in `OB`.

### M7 -- `braid-ups-secrets.service`

- Add to [`modules/braid/ups.nix`](../../modules/braid/ups.nix):
  ```nix
  systemd.services.braid-ups-secrets = {
    description = "Generate upsmon password file for braid-managed NUT";
    before = [ "upsd.service" "upsmon.service" ];
    requiredBy = [ "upsd.service" "upsmon.service" ];
    path = [ pkgs.coreutils ];  # ensure head/base64/chmod/chown resolve from the pinned closure
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
      ExecStart = pkgs.writeShellScript "braid-ups-secrets" ''
        set -euo pipefail
        if [ ! -s /var/lib/braid/upsmon.pass ]; then
          umask 077
          head -c 24 /dev/urandom | base64 > /var/lib/braid/upsmon.pass
          chmod 0600 /var/lib/braid/upsmon.pass
          chown root:root /var/lib/braid/upsmon.pass
        fi
      '';
    };
  };
  ```
- `power.ups.users.<name>.passwordFile = "/var/lib/braid/upsmon.pass"` and `power.ups.upsmon.monitor.<name>.passwordFile = "/var/lib/braid/upsmon.pass"`.
- **Verify**: fresh boot yields `/var/lib/braid/upsmon.pass` with `0600 root:root`; file survives rebuild; not in `nix-store --query` output.

### M8 -- `SHUTDOWNCMD = systemctl poweroff`

- Add to [`modules/braid/ups.nix`](../../modules/braid/ups.nix): `power.ups.upsmon.settings.SHUTDOWNCMD = "${pkgs.systemd}/bin/systemctl poweroff";` (nixpkgs uses `mkDefault`, so plain assignment wins).
- **Verify**: `systemctl cat upsmon.service` shows the override; confirmed via M10a VM test.

### M9 -- `braid doctor` checks

- Modify [`cli/src/doctor.rs:731-740`](../../cli/src/doctor.rs) -- append:
  - `check_ups_online(ctx)` -- when `ups.enable`, runs `upsc`; `Warn` if daemon down.
  - `check_braid_online_active_when_mounted(ctx)` -- when `ups.enable` and pool is mounted, assert `systemctl is-active braid-online.service` is `active`; `Fail` (high severity) otherwise.
- Both guarded by config probe; absent UPS block -> checks skip.
- **Verify**: unit tests using `MockRunner`; VM test that simulates `braid-online.service` inactive-while-mounted returns the expected doctor output.

### Pre-M11 -- `braid recover` audit

Before M11-M14 VM tests run, audit [`cli/src/recover.rs`](../../cli/src/recover.rs) and the journal-replay paths for each mutation class. Concretely: for `replace`, `remove`, `remove-missing`, and `add`, identify what happens when the previous boot was interrupted mid-mutation. If any class has a gap (e.g. `btrfs replace` in progress on reboot, or `btrfs device remove` interrupted), land the remediation in this audit milestone, not in the VM test PR. Gaps here are the single biggest risk to flipping the ADR Active.

Output of this milestone: either "no gap, proceed" or a concrete list of recovery code paths to add.

### M10a -- VM test: ordinary-mounted-operation LB -> clean poweroff

- Create [`tests/module/ups-lb-clean-shutdown.nix`](../../tests/module/ups-lb-clean-shutdown.nix) and `.py`.
- Test: spawn VM with `braid.ups.enable = true`, dummy-ups configured; unlock pool, write a canary file; `upsrw -s 'ups.status=OB LB' -u <user> -p <pass> <upsname>@localhost` (the `-s` flag and quoted value are required so `OB LB` is parsed as a single multi-flag status, not two argv tokens); wait for machine to shut down.
- Boot again, re-unlock, assert: canary file present, no btrfs device stats errors, `journalctl -b -1` contains "Stopped Braid storage pool online" (proves ExecStop ran).
- Follows the pattern at [`tests/module/systemd-lifecycle.py:420-462`](../../tests/module/systemd-lifecycle.py).
- Block-comment with Intent / Why / Scenario per AGENTS.md test convention.

### M10b -- Battery threshold remediation (only if M10a fails)

The runtime budget between "UPS goes critical" and "host actually powers off" is the sum of (a) NUT `FINALDELAY` (default 5s per [`reference/nut/clients/upsmon.c:114`](../../reference/nut/clients/upsmon.c), the sleep before `upsmon` invokes `SHUTDOWNCMD`), (b) `systemctl poweroff` sequencing, and (c) `braid-online.service` `ExecStop` (bounded by `TimeoutStopSec = 5min`). If M10a times out because the default `battery.runtime.low` (~120s on APC) doesn't cover that sum, measure the observed total clean-shutdown duration from the M10a run and raise `battery.runtime.low` to that value plus a margin (e.g. observed + 30s) via `power.ups.ups.<name>.directives = [ "override.battery.runtime.low=<measured+margin>" ]`. Lowering `FINALDELAY` buys at most ~5s by default and does not cover a genuinely insufficient budget; raise the threshold instead. Document the chosen value in the ADR's resolved-open-questions section.

### M11-M14 -- VM tests per mutation class

Create [`tests/module/lib/ups-fixture.nix`](../../tests/module/lib/ups-fixture.nix) as a shared dummy-ups harness. Each test module imports it and parameterizes the mutation. The fixture provisions a second upsd user (e.g. `testops`) with `actions = [ "SET" ]` so `upsrw` can drive state changes; the production upsmon user stays minimal. Tests use the test-only credential when calling `upsrw` (never the upsmon credential).

- **M11**: `tests/module/ups-lb-during-replace.{nix,py}` -- start `braid replace old new` with enough data for the replace to take ~30s; mid-flight `upsrw -s 'ups.status=OB LB' -u <user> -p <pass> <upsname>@localhost`; reboot; assert `braid recover` restores the pool cleanly and replace either completed or is resumed.
- **M12**: `tests/module/ups-lb-during-remove.{nix,py}` -- same shape for `braid remove`.
- **M13**: `tests/module/ups-lb-during-remove-missing.{nix,py}` -- same for `braid remove-missing`; interrupt during the conditional `maybe_restore_raid1` soft balance.
- **M14**: `tests/module/ups-lb-during-balanced-add.{nix,py}` -- same for `braid add` during the `pool_balance_raid1` phase.

Each test asserts: reboot succeeds, `braid recover` runs to completion without errors, pool mounts, no btrfs device stats errors, no orphaned LUKS mappers, no stuck journal / pending-op.

When all four pass, flip [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md) to `Status: Active`.

## Critical files

Grouped by layer. Relative to repo root.

**Module:**
- Create `modules/braid/ups.nix`
- Modify `modules/braid/default.nix` (imports)
- Modify `modules/braid/options.nix` (package pin)
- Modify `modules/braid/cli.nix` (config.json ups block)
- Modify `modules/braid/wrapper.nix` (tool PATH)

**CLI (Rust):**
- Create `cli/src/parse/upsc.rs`, `cli/src/ups.rs`
- Modify `cli/src/parse/mod.rs`, `cli/src/parse/types.rs`, `cli/src/cmd.rs`
- Modify `cli/src/preflight.rs`, `cli/src/doctor.rs`
- Modify `cli/src/add.rs`, `cli/src/remove.rs`, `cli/src/remove_missing.rs`, `cli/src/replace.rs`
- Modify `cli/src/main.rs`, `cli/src/config.rs`
- Potentially modify `cli/src/recover.rs` (pending audit outcome)

**TUI:**
- Modify `cli/src/tui/{model.rs,effect.rs,app.rs,probe.rs,view/mod.rs}`

**Tests:**
- Create `tests/capture-ups-fixtures.nix`
- Modify `cli/tests/golden_nixos_25_11.rs`
- Create `tests/module/ups-lb-clean-shutdown.{nix,py}`
- Create `tests/module/ups-lb-during-{replace,remove,remove-missing,balanced-add}.{nix,py}`
- Create `tests/module/lib/ups-fixture.nix`

**Docs:**
- Modify `docs/decisions/020-ups-integration.md` (remove alert-integration section; Status bump to Active after M14)
- Modify `docs/decisions/010-toolchain-pinning.md` (NUT added)
- Modify `docs/principles.md` (principle 10)
- Modify `AGENTS.md` (parser-compatibility table; NUT reference-source row already present)
- Modify `docs/index.md` -- refresh decision 020's summary line when Status flips to Active; add line for any follow-up ADR covering alert-cause persistence semantics
- Modify `README.md` -- end-user guide coverage: enabling `braid.ups.enable`, `braid ups status` usage, shutdown-at-critical behavior, "mutations refuse to start on battery" rule, and the v1 alert-integration limitation (no async notification yet -- users must check TUI or `braid ups status` for UPS state)

**Build:**
- Modify `justfile` -- add `capture-ups-fixtures` recipe; include in `capture-all-fixtures`

## Existing functions / utilities to reuse

- `parse::*` pattern -- mirror `parse_smartctl` shape at [`cli/src/parse/smartctl.rs:76`](../../cli/src/parse/smartctl.rs).
- `require_mutation_preflight` + `check_*` helpers -- [`cli/src/preflight.rs:15-321`](../../cli/src/preflight.rs).
- `DaemonStatus` rendering for Fans section -- [`cli/src/tui/view/mod.rs:149-157`](../../cli/src/tui/view/mod.rs).
- `Effect::ProbeFan` + `FAN_PROBE_INTERVAL` -- [`cli/src/tui/app.rs:17,43-51`](../../cli/src/tui/app.rs).
- Shutdown VM test pattern -- [`tests/module/systemd-lifecycle.py:420-462`](../../tests/module/systemd-lifecycle.py).
- Monitor lifecycle VM test pattern -- [`tests/module/monitor-lifecycle.{nix,py}`](../../tests/module/monitor-lifecycle.nix).
- `braid-online.service` definition -- [`modules/braid/storage.nix:84-101`](../../modules/braid/storage.nix).

## Verification

**Unit tests (`just test-rust`):**
- Golden-parser tests for `parse_upsc` against each fixture.
- `check_ups_not_on_battery` preflight with `MockRunner` in OB/LB/OL states.
- `doctor.rs` checks with mocked systemctl + `upsc` responses.

**Parser canary (`just test-parsers`):**
- Add a `braid-status-ups` VM check that boots a dummy-ups-configured NUT and confirms `upsc` output parses cleanly.

**VM tests (`just test-vm`):**
- `ups-lb-clean-shutdown` (M10a).
- `ups-lb-during-replace` / `-remove` / `-remove-missing` / `-balanced-add` (M11-M14).

**Manual smoke on real hardware:**
- Hook target NAS to a real UPS with `braid.ups.enable = true`.
- `braid ups status` returns expected fields (online state, battery charge, runtime, load).
- Unplug UPS input; confirm `braid ups status` flips to on-battery within one refresh.
- In the TUI Data tab, confirm the UPS section shows the right color severity (green/yellow/red) as state changes.
- `braid add <spare-disk>` while on battery returns a `Validation` error; after restoring AC, retry succeeds.
- Let the UPS drain to low battery; confirm the NAS shuts down cleanly (pool unmounted, LUKS closed, canary file intact on next boot).

## Risks

1. **Runtime budget: `battery.runtime.low` vs. `FINALDELAY` + systemd shutdown + `TimeoutStopSec = 5min`.** Default LB threshold (~120s on APC) may not cover NUT's `FINALDELAY` (default 5s) + systemd teardown + `braid-online.service` stop bound. M10a measures the observed total clean-shutdown duration; M10b remediates by raising `battery.runtime.low` to that value plus a margin. (Lowering `FINALDELAY` buys at most ~5s and cannot rescue a genuinely insufficient budget.) ADR Open Question 3.

2. **`braid recover` gap for mid-mutation power loss.** Unknown until audited. Pre-M11 milestone resolves. If gaps exist, they surface as failing M11-M14 tests or as new recover code to write. The ADR's "mid-mutation power loss is a supported recovery case" claim is only honest after this audit + any remediation.

3. **Dummy-ups fixture reliability.** Tests drive state via `upsrw`, so the fixture uses `dummy-once` with a `.dev` file (per [`reference/nut/docs/man/dummy-ups.txt:90,100`](../../reference/nut/docs/man/dummy-ups.txt)): `dummy-once` loads the file into memory once and preserves `upsrw` writes, while `dummy-loop` would re-read the file and overwrite in-memory `upsrw` changes before `upsmon` reacts. Document this in `tests/module/lib/ups-fixture.nix`.

4. **`braid.packages.nut` nixos-unstable forecast lane.** After M2 lands stable fixtures, add unstable capture to `just capture-all-fixtures-unstable` and unstable golden tests. Matches existing parser obligations under [decision 010](../../docs/decisions/010-toolchain-pinning.md).

5. **Alert integration is a known gap in v1.** Users who aren't watching `braid ups status` / TUI won't see on-battery or comms-loss conditions asynchronously. Document this limitation in the ADR and the user guide, and plan the follow-up ADR for `AlertCause` persistence semantics.
