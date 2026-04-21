# UPS v1 Safety Core

Derived from: `plans/wip/purrfect-honking-rabin.md` (the original monolithic
UPS-integration plan). This file is one of three follow-ons:

- `plans/wip/ups-v1-safety-core.md` (this file) -- smallest shippable safety
  feature set behind `braid.ups.enable`.
- `plans/wip/ups-observability-ux.md` -- rich parser, TUI section, doctor
  checks, fixture / canary / unstable-lane machinery, README UX coverage.
- `plans/wip/forced-shutdown-recovery-proof.md` -- `braid recover` audit and
  the per-mutation VM matrix that proves mid-mutation power loss is
  survivable.

## Scope

Ship the smallest coherent UPS safety feature set behind `braid.ups.enable`:

1. Nix module skeleton wrapping `power.ups` with opinionated defaults.
2. `braid-ups-secrets.service` to provision the upsmon<->upsd credential
   outside the Nix store.
3. `SHUTDOWNCMD = systemctl poweroff` so upsmon's critical-state trigger
   unwinds `braid-online.service` through the systemd-lifecycle teardown
   sequence described in [decision 018](../../docs/decisions/018-systemd-lifecycle.md).
4. Config plumbing into `/etc/braid/config.json` so the CLI can find the UPS
   name at runtime.
5. A minimal `parse_upsc` parser with just enough surface for preflight
   (status flags) plus a daemon-down error mode.
6. A minimal `braid ups status` command sufficient for operator inspection
   and preflight troubleshooting.
7. Preflight refusal of `braid add`, `braid remove`, `braid remove-missing`,
   and `braid replace` when UPS status is `OB` / `LB` / unknown / daemon
   down.
8. One VM proof that `upsmon -> systemctl poweroff -> braid-online ExecStop
   -> clean shutdown` works in ordinary mounted operation.
9. A runtime-budget remediation path if the default `battery.runtime.low`
   (~120s on APC) does not cover the observed shutdown duration.

The end state: `braid.ups.enable = true` gives operators two of the three v1
guarantees -- orderly shutdown before battery exhaustion in ordinary mounted
operation, and preflight refusal of pool-mutating commands on battery. The
third guarantee -- live UPS state visible in the TUI -- lives in
`ups-observability-ux.md`. ADR 020 stays `Draft` until the recovery-proof
plan also passes.

## Non-goals

Explicitly out of scope for this plan. Deferred to the referenced follow-on
plan or a future ADR.

- **Alert-model integration for UPS events.** No new `AlertCause` variants,
  no `NOTIFYCMD` wiring, no `/var/lib/braid/ups-alerts/` latch files.
  Decision 014's "alerts stay latched until `braid ack`" invariant is the
  right shape for event-driven causes but wrong for live-state conditions
  like `OB` / `LB` (users expect those to clear when the UPS returns to
  `OL`). Reconciling that requires splitting `AlertCause` by persistence
  semantics (`LatchedUntilAck` vs. `ActiveWhileConditionHolds`) and updating
  `merge_into_latch`, `ack`, `status`, and the full test matrix -- a
  core-invariant change that deserves its own ADR. Smuggling it into UPS v1
  would conflate two distinct concerns. A future ADR + plan owns this.
- **Rich `parse_upsc` data model.** Deferred to
  `plans/wip/ups-observability-ux.md`. This plan lands only the minimum
  parser surface required for preflight to decide OL vs. OB/LB/unknown and
  for `braid ups status` to render decoded status flags + a daemon-down
  error.
- **TUI UPS section.** Deferred to `plans/wip/ups-observability-ux.md`.
- **`braid doctor` UPS checks.** Deferred to
  `plans/wip/ups-observability-ux.md`.
- **Parser-fixture capture machinery and parser-canary / golden / unstable
  lane obligations at the level maintained for btrfs-progs / cryptsetup /
  util-linux.** Deferred to `plans/wip/ups-observability-ux.md`. This plan
  ships a handful of golden fixtures for the minimal parser but does not
  promote NUT to the full parser-critical maintenance surface yet.
- **Per-mutation power-loss matrix (M11-M14) and the `braid recover`
  audit.** Deferred to `plans/wip/forced-shutdown-recovery-proof.md`. ADR
  020 does **not** flip to `Active` when this plan passes; it flips only
  after the recovery-proof plan also passes.
- **Backwards compatibility.** braid is unreleased software
  (see `AGENTS.md`). If a behavior or config shape changes in Plans 2 or 3,
  this plan's shape is updated in place without compatibility shims.

## Dependencies

- `docs/decisions/018-systemd-lifecycle.md` (`Active`) -- `braid-online.service`'s
  `ExecStop = braid lock` is the hinge that makes `SHUTDOWNCMD = systemctl
  poweroff` safe. Read before touching the shutdown wiring.
- `docs/decisions/020-ups-integration.md` (currently `Draft`) -- refined by
  this plan's first milestone to remove alert-integration from v1 scope.
  Stays `Draft` at the end of this plan; flips to `Active` only after
  `plans/wip/forced-shutdown-recovery-proof.md` passes.
- Local NUT reference source at `reference/nut/` (fetched via
  `scripts/fetch-references.py`). Authoritative for behavior, config fields,
  and exit codes:
  - `reference/nut/clients/upsmon.c` -- `SHUTDOWNCMD` invocation,
    `FINALDELAY` default (~line 114), critical-state logic (~line 1404).
  - `reference/nut/clients/upsc.c` -- key/value output shape we parse.
  - `reference/nut/conf/upsd.users.sample` and
    `reference/nut/docs/man/upsd.users.txt` -- authoritative on which
    upsmon fields production needs. Per `upsd.users.txt:78`, the `SET`
    action is required only by `upsrw` clients; the production upsmon
    credential must not carry it.
  - `reference/nut/conf/upsmon.conf.sample.in` -- shape of `MONITOR`,
    `SHUTDOWNCMD`, `FINALDELAY`.

## Milestones

The refinement-of-docs milestone must land before M1 so that the ADR
accurately describes v1 scope before any code lands against it. M1-M6 are
the implementation milestones. M7 is the single VM-test proof that this
plan is complete. M7b is conditional remediation.

### Pre-M1 -- ADR 020 v1-scope refinement

Refine [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
to match v1's narrower scope before coding starts. Concretely:

- **Remove the "UPS events become first-class alert causes" section in its
  entirety.** Replace with a short paragraph: alert-model integration is
  deferred to a future ADR because live-state UPS conditions do not fit
  decision 014's "latched until ack" invariant; reconciling them requires a
  broader alert-model change (splitting `AlertCause` by `LatchedUntilAck`
  vs. `ActiveWhileConditionHolds`).
- **Update guarantee (3) in the ADR Context.** Change from "UPS events
  latched into braid's shared alert model" to "live UPS state visible in
  `braid ups status` and the TUI; live UPS status is used for preflight
  safety and upsmon critical-state shutdown (normally `OB` + `LB` together,
  per `reference/nut/clients/upsmon.c:1404`)."
- **Drop the `/var/lib/braid/ups-alerts/` latch-file design.** Not used in
  v1 since there is no alert integration at all.
- **Keep the "Upsmon credential lifecycle" subsection as written** --
  credential generation is still needed because NUT requires
  upsmon<->upsd auth even in standalone mode.
- **Keep the "`braid-online` becomes safety-critical under UPS"
  subsection** -- under UPS, "pool mounted but `braid-online` inactive" is
  a configuration fault (the `ExecStop = braid lock` hook does not run),
  not an alert cause. Flagging this lives in `braid doctor` (see
  `plans/wip/ups-observability-ux.md`).
- **Keep Status: `Draft`.** The ADR flips to `Active` only after the
  recovery-proof plan passes its full matrix. See the cross-plan status
  dependency at the bottom of this file.

Cross-cutting doc refinements for NUT becoming a pinned toolchain member
(principle 10, `docs/decisions/010-toolchain-pinning.md`, the
parser-compatibility table in `AGENTS.md`) are **deferred to
`plans/wip/ups-observability-ux.md`**, where NUT is promoted to the full
parser-critical surface with fixture-refresh obligations. Doing those
updates earlier than the fixture machinery exists would oversell the
current guarantee.

**Verify:** `docs/decisions/020-ups-integration.md` diff shows the
above three structural changes; Status remains `Draft`; `docs/index.md`
summary line for 020 still accurate.

### M1 -- Module skeleton, `power.ups` wiring, package pin

- Create [`modules/braid/ups.nix`](../../modules/braid/ups.nix).
  - `braid.ups.{enable, name, driver, port}` options (`name` default
    `"ups"`, `driver` default `"usbhid-ups"`, `port` default `"auto"`).
  - Wire `power.ups.*`:
    - `enable = true` (gated on `cfg.ups.enable`).
    - `mode = "standalone"`.
    - `ups.<name>.{driver, port}`.
    - `upsmon.monitor.<name>.{system = "<name>@localhost", powerValue = 1,
      type = "primary", passwordFile}`.
  - **Production upsmon user is minimal.** Exactly one upsmon user with
    `upsmon = "primary"`, no `actions`, no `instcmds`. Per
    `reference/nut/docs/man/upsd.users.txt:78`, the `SET` action is only
    required by `upsrw` clients; the production credential must not carry
    it. Test-only users with `actions = [ "SET" ]` are a
    `forced-shutdown-recovery-proof.md` concern and are provisioned in
    `tests/module/lib/ups-fixture.nix`, not in this production module.
  - `power.ups.package = cfg.packages.nut`.
  - Assertion: `cfg.ups.enable -> cfg.ups.name != ""`.
- Modify [`modules/braid/default.nix:1`](../../modules/braid/default.nix) --
  add `./ups.nix` to imports.
- Modify [`modules/braid/options.nix:16-18`](../../modules/braid/options.nix)
  -- add `nut = lib.mkPackageOption pkgs "nut" {};`.
- Modify [`modules/braid/cli.nix:13-31`](../../modules/braid/cli.nix) -- when
  `cfg.ups.enable`, emit `ups = { enable = cfg.ups.enable; name =
  cfg.ups.name; }` into `/etc/braid/config.json`.
- Modify [`cli/src/config.rs:13-54`](../../cli/src/config.rs) -- add
  `Ups { enable: bool, name: String }` as an optional field on `Config`,
  mirroring `FanControl`.
- Modify [`modules/braid/wrapper.nix:11`](../../modules/braid/wrapper.nix)
  -- add `cfg.packages.nut` to `toolPackages` so `upsc` resolves in the
  wrapped binary (needed by M3 and M4).

**Verify:** `braid.ups.enable = true` in a smoke VM boots `upsd.service`
and `upsmon.service` without error; `upsc ups` returns the dummy UPS's
fields. The production upsmon user entry in the rendered `upsd.users` has
no `actions` list.

### M2 -- Minimal `parse_upsc` for preflight + status

Implement only the parser surface that M3 and M4 need. The rich model
(battery mfr date, input voltage, test-result decoding, etc.) is explicitly
Plan 2's responsibility; adding it here couples the safety core to fixture
machinery that does not yet exist.

- Create [`cli/src/parse/upsc.rs`](../../cli/src/parse/upsc.rs) with
  `pub fn parse_upsc(raw: &RawCommandOutput) -> UpscOutput`.
  - Minimal `UpscOutput`:
    ```rust
    pub struct UpscOutput {
        pub status_flags: HashSet<UpsStatusFlag>,
        pub extra: BTreeMap<String, String>, // everything else, untouched
    }
    ```
  - Extending `UpscOutput` with typed `battery`, `input`, `load_pct`,
    `realpower_nominal_watts`, `test_result`, `device` fields is Plan 2's
    work. Plan 2 can freely change the shape of `UpscOutput` -- no
    backwards-compat contract ties us here.
  - `UpsStatusFlag` enum: `Ol`, `Ob`, `Lb`, `Rb`, `Hb`, `Chrg`, `Dischrg`,
    `Cal`, `Bypass`, `Off`, `Over`, `Trim`, `Boost`, `Fsd`,
    `Unknown(String)`. (Full enum lands here even though preflight only
    consults `Ob` / `Lb`; keeping the enum complete means Plan 2 does not
    need to re-land the full variant list.)
- Modify [`cli/src/parse/mod.rs`](../../cli/src/parse/mod.rs) -- `pub mod
  upsc; pub use upsc::parse_upsc;`.
- Modify [`cli/src/parse/types.rs`](../../cli/src/parse/types.rs) or
  sibling as needed.
- Modify [`cli/src/cmd.rs`](../../cli/src/cmd.rs) -- add
  `CmdRequest::UpscQuery { name: String }`.
- Land three minimal golden fixtures hand-written from the
  `reference/nut/clients/upsc.c` output shape (not a VM-captured corpus):
  `upsc-online.txt`, `upsc-onbattery-low.txt`, `upsc-daemon-down.stderr`.
  Unit tests assert status-flag parsing for each.
- **Do not** create `tests/capture-ups-fixtures.nix`, wire `just
  capture-ups-fixtures` into `just capture-all-fixtures`, or add unstable
  lane coverage in this plan. Those are Plan 2's responsibility.

**Verify:** `just test-rust` green; the three golden fixtures parse; unit
tests exercise preflight-relevant status combinations (`OL`, `OB`,
`OB LB`, empty / missing, daemon-down).

### M3 -- Minimal `braid ups status`

`braid ups status` exists here to give operators a way to **inspect the
same state preflight inspects**. Anything richer (curated human summary
with watts / runtime / battery mfr date / test result, stable `--json`
shape, decoded input voltage context) is Plan 2's responsibility.

- Create [`cli/src/ups.rs`](../../cli/src/ups.rs) -- `cmd_ups_status(args,
  paths, runner)`.
  - Reads `config.rs` for `ups` block; missing or `enable = false` ->
    helpful enable-hint message, exit 0.
  - Runs `UpscQuery { name }`; subprocess failure -> `"ups daemon not
    running -- check 'systemctl status upsd.service'"` to stderr, exit 1.
  - Success -> `parse_upsc` -> human render: one line "Status: OL" (or
    whichever decoded flags are set), plus the `extra` key/value table
    printed verbatim for now.
  - No `--json` flag in this plan. Stable JSON shape is deferred to Plan 2
    (where the rich model exists to serialize).
- Modify [`cli/src/main.rs:23-64`](../../cli/src/main.rs) -- add
  `Commands::Ups(UpsArgs)` with `Status` subcommand (no `--json` yet).

**Verify:** `braid ups status` works against a dummy-ups VM and shows
decoded status flags plus the raw key/value passthrough; absence of UPS
config prints the enable hint and exits 0; `upsd` stopped prints
daemon-down and exits 1.

### M4 -- Preflight reject on battery / unknown

This is the primary safety feature: narrow the surface that mid-mutation
recovery (Plan 3) must cover by rejecting the avoidable case up front.

- Modify [`cli/src/preflight.rs:15-321`](../../cli/src/preflight.rs) -- new
  `fn check_ups_not_on_battery(runner, ups_name: Option<&str>) ->
  PreflightResult`.
  - When `ups_name` is `None` (no `ups` block in config), the check is a
    no-op.
  - When `ups_name` is `Some`, invokes `upsc <name>` via `UpscQuery` +
    `parse_upsc`. The check returns error if:
    - status flags contain `Ob` or `Lb`, OR
    - the `upsc` subprocess fails (daemon down), OR
    - the parsed status flag set is empty / missing (treat unknown state as
      on-battery; fail-closed).
  - Returns a single `Validation`-shaped error with the text: `"cannot
    verify UPS is on utility power -- refusing to start <op>. Check 'braid
    ups status', restore utility power, then retry."` One message covers
    `OB`, `LB`, `OFF`, and comms-failure so the wording stays honest when
    the cause is uncertain.
- Modify [`cli/src/add.rs:320`](../../cli/src/add.rs),
  [`cli/src/remove.rs`](../../cli/src/remove.rs),
  [`cli/src/remove_missing.rs`](../../cli/src/remove_missing.rs),
  [`cli/src/replace.rs`](../../cli/src/replace.rs) -- wire `ups_name` from
  `Config` through `require_mutation_preflight` (or equivalent entry point
  per-command). The check must run **before the journal write** in every
  case.
- Each command maps the preflight error to its own `Validation` variant
  per existing pattern.

**Verify:**
- Unit tests with `MockRunner` exercising OB, LB, OL, daemon-down,
  empty-flags, and no-UPS-config across all four mutation entry points
  (`add`, `remove`, `remove-missing`, `replace`). Unit tests carry the
  per-command breadth.
- **Mandatory VM smoke test** (new file:
  `tests/module/ups-preflight-on-battery.{nix,py}`): boot with
  `braid.ups.enable = true` and dummy-ups in `OB`; run one mutation
  (pick `braid add <spare>` as the representative; data volume can be
  minimal because preflight runs before the journal write); assert exit
  is the `Validation` variant with the expected error text. Block
  comment per `AGENTS.md` test convention: Intent = "preflight refusal
  on battery blocks mutation starts end-to-end, not just in unit
  tests"; Why = "one of the two shipped safety guarantees; must be
  proven against real `upsc` output and real NUT wiring, not only
  through `MockRunner`"; Scenario = "operator runs `braid add` while
  the UPS is already on battery."
- The per-mutation interruption matrix (mutations that were already
  running when LB fires) lives in `forced-shutdown-recovery-proof.md`;
  breadth across the four mutations under preflight refusal stays in
  unit tests here.

### M5 -- `braid-ups-secrets.service`

NUT requires upsmon<->upsd authentication even in single-host standalone
mode. The credential must live outside the Nix store so `nix-store --query`
never reveals it.

- Add to [`modules/braid/ups.nix`](../../modules/braid/ups.nix):

  ```nix
  systemd.services.braid-ups-secrets = {
    description = "Generate upsmon password file for braid-managed NUT";
    before = [ "upsd.service" "upsmon.service" ];
    requiredBy = [ "upsd.service" "upsmon.service" ];
    path = [ pkgs.coreutils ];  # head/base64/chmod/chown from the pinned closure
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

- `power.ups.users.<name>.passwordFile = "/var/lib/braid/upsmon.pass"` and
  `power.ups.upsmon.monitor.<name>.passwordFile =
  "/var/lib/braid/upsmon.pass"`.
- `/var/lib/braid/` already exists via `storage.nix:22` tmpfiles -- no new
  directory rule needed.
- `before` + `requiredBy` (not `wants`) so that `upsd` and `upsmon` hard-fail
  to start if secret creation fails, rather than racing it.

**Verify:** Fresh boot yields `/var/lib/braid/upsmon.pass` with mode `0600`
and owner `root:root`; file survives rebuild; `nix-store --query
--references /etc/nut/upsd.users` and `... upsmon.conf` do not contain the
token.

### M6 -- `SHUTDOWNCMD = systemctl poweroff`

nixpkgs' `power.ups` module defaults `SHUTDOWNCMD` to `shutdown now`. We
override to `systemctl poweroff` so the shutdown runs through systemd's
standard stop sequence, which unwinds `braid-online.service` (see decision
018) -> btrfs umount -> luks close.

- Add to [`modules/braid/ups.nix`](../../modules/braid/ups.nix):

  ```nix
  power.ups.upsmon.settings.SHUTDOWNCMD =
    "${pkgs.systemd}/bin/systemctl poweroff";
  ```

  nixpkgs uses `mkDefault`, so a plain assignment wins.

**Verify:** `systemctl cat upsmon.service` on the smoke VM shows the
override. Full end-to-end confirmation is M7's job.

### M7 -- VM proof: ordinary mounted operation LB -> clean poweroff

The one shippable proof that the safety core works: LB fires while the
pool is mounted and idle, upsmon runs `SHUTDOWNCMD`, systemd unwinds
`braid-online.service`'s `ExecStop`, btrfs unmounts cleanly, LUKS closes
cleanly, host powers off.

- Create [`tests/module/ups-lb-clean-shutdown.nix`](../../tests/module/ups-lb-clean-shutdown.nix)
  and the sibling `.py`.
- Block comment per `AGENTS.md` test convention with:
  - **Intent** -- verify that upsmon's critical-state trigger runs
    `systemctl poweroff`, which unwinds `braid-online.service`'s
    `ExecStop = braid lock`, which cleanly unmounts btrfs and closes LUKS.
  - **Why** -- this is v1 guarantee (1): orderly shutdown before battery
    exhaustion in ordinary mounted operation. Without this proof, the
    safety-core shipping claim is hollow.
  - **Scenario** -- real-world outage lasting long enough to drain the UPS
    past the low-battery threshold while the NAS is idle with the pool
    mounted.
- Test body:
  1. Spawn VM with `braid.ups.enable = true` and dummy-ups configured.
  2. Unlock the pool; write a canary file into the mount.
  3. Drive the dummy UPS to critical via
     `upsrw -s 'ups.status=OB LB' -u <testops-user> -p <pass>
     <upsname>@localhost`. The `-s` flag plus the quoted value are required
     so `OB LB` is parsed as one multi-flag status, not two argv tokens
     (see `reference/nut/clients/upsrw.c`). The test uses a **test-only
     credential** with `actions = [ "SET" ]` provisioned by a minimal
     harness in this plan's test file; production upsmon credentials never
     carry `SET`.
  4. Wait for the machine to shut down.
  5. Boot again; re-unlock; assert:
     - canary file is present and intact;
     - `btrfs device stats <mount>` reports zero errors;
     - `journalctl -b -1` contains "Stopped Braid storage pool online"
       (proves `braid-online.service`'s `ExecStop` ran before power-off).
- Follows the pattern at
  [`tests/module/systemd-lifecycle.py:420-462`](../../tests/module/systemd-lifecycle.py).

**Note on fixture placement:** This plan embeds a minimal test-only upsmon
`SET` user inline in `tests/module/ups-lb-clean-shutdown.nix` rather than
creating the shared harness. The shared
`tests/module/lib/ups-fixture.nix` is created by
`plans/wip/forced-shutdown-recovery-proof.md` when the full mutation
matrix lands; refactoring this single test onto the shared harness is
part of that plan.

**Verify:** `just test-vm ups-lb-clean-shutdown` passes.

### M7b -- Battery-threshold remediation (only if M7 fails)

If and only if M7 times out because the host does not finish shutting
down before the dummy UPS "dies," apply this remediation. Do not
apply preemptively.

The runtime budget between "UPS goes critical" and "host actually powers
off" is the sum of:

- (a) NUT `FINALDELAY` (default ~5s per
  `reference/nut/clients/upsmon.c:114`) -- the sleep before `upsmon`
  invokes `SHUTDOWNCMD`.
- (b) `systemctl poweroff` sequencing.
- (c) `braid-online.service` `ExecStop` (bounded by `TimeoutStopSec =
  5min`, set in decision 018's unit definition).

Default `battery.runtime.low` on APC hardware is ~120 seconds. If a
loaded pool takes longer than that to tear down, M7 hangs until the
simulated battery runs out.

Remediation (do in this order):

1. From the failing M7 run, measure the observed clean-shutdown duration
   -- the wall-clock time from "upsrw set OB LB" to "host actually powered
   off."
2. Raise `battery.runtime.low` to that observed value plus a 30s margin
   via
   `power.ups.ups.<name>.directives = [ "override.battery.runtime.low=<measured+margin>" ]`.
3. Document the chosen value in ADR 020's resolved-open-questions section
   (Open Question 3).
4. Re-run M7.

Lowering `FINALDELAY` buys at most ~5s by default and cannot rescue a
genuinely insufficient runtime budget. Do not use it as the primary
remediation.

**Verify:** M7 passes on the remediated VM.

## Critical files

Grouped by layer. Relative to repo root.

**Module:**
- Create `modules/braid/ups.nix`
- Modify `modules/braid/default.nix` (imports)
- Modify `modules/braid/options.nix` (package pin)
- Modify `modules/braid/cli.nix` (config.json `ups` block)
- Modify `modules/braid/wrapper.nix` (tool PATH so `upsc` resolves inside
  the wrapped binary)

**CLI (Rust):**
- Create `cli/src/parse/upsc.rs` (minimal shape: `status_flags` +
  `extra`; Plan 2 expands)
- Create `cli/src/ups.rs` (minimal human output; no `--json` yet)
- Modify `cli/src/parse/mod.rs`, `cli/src/parse/types.rs`, `cli/src/cmd.rs`
  (add `UpscQuery`)
- Modify `cli/src/preflight.rs` (new `check_ups_not_on_battery`)
- Modify `cli/src/add.rs`, `cli/src/remove.rs`,
  `cli/src/remove_missing.rs`, `cli/src/replace.rs` (call new preflight)
- Modify `cli/src/main.rs` (new `Ups(UpsArgs)` command)
- Modify `cli/src/config.rs` (new `Ups` field on `Config`)

**Tests:**
- Create `tests/module/ups-lb-clean-shutdown.{nix,py}` (M7).
- Create `tests/module/ups-preflight-on-battery.{nix,py}` (M4 mandatory
  smoke test).
- Hand-written fixtures under `cli/tests/fixtures/` for minimal golden
  parser tests (`upsc-online.txt`, `upsc-onbattery-low.txt`,
  `upsc-daemon-down.stderr`) -- captured-from-VM fixture corpus is Plan
  2's work.

**Docs:**
- Modify `docs/decisions/020-ups-integration.md` -- Pre-M1 refinement.
  Status remains `Draft`.

**Build:**
- No changes to `justfile` in this plan. `just capture-ups-fixtures` is
  Plan 2.

## Verification

**Unit tests (`just test-rust`):**
- Minimal golden-parser tests for `parse_upsc` against the three
  hand-written fixtures.
- `check_ups_not_on_battery` with `MockRunner` in OL / OB / LB /
  daemon-down / empty / no-UPS-config states.
- Plumbing tests that each of `add`, `remove`, `remove-missing`, `replace`
  invokes the new check before journal write.

**VM tests (`just test-vm`):**
- `ups-lb-clean-shutdown` (M7) -- mandatory.
- `ups-preflight-on-battery` (M4) -- mandatory. One mutation
  (representative: `braid add`) against real `upsc` output and real NUT
  wiring, proving preflight refusal is not unit-test-only. Per-command
  breadth stays in unit tests.
- The per-mutation interruption matrix (mutations already running when
  LB fires) is Plan 3's responsibility, not a deferred item here.

**Manual smoke on real hardware:**
- Hook a target NAS to a real UPS with `braid.ups.enable = true`.
- `braid ups status` returns decoded status flags + raw extras from a
  live UPS.
- `braid add <spare-disk>` while on battery returns a `Validation` error;
  after restoring AC, retry succeeds.
- Let the UPS drain to low battery; confirm the NAS shuts down cleanly
  (canary file intact on next boot).

## Risks

1. **`braid-online.service` silent-degradation.** The wrapper's warn-and-
   continue behavior on `systemctl start braid-online.service` failure is
   safe without UPS, but **unsafe under UPS**: without `braid-online`
   active, its `ExecStop` does not run and LUKS close is not guaranteed to
   complete before power dies. This plan relies on `braid-online`
   activating correctly. Detecting "pool mounted but `braid-online`
   inactive under UPS" is a `braid doctor` check that lives in
   `plans/wip/ups-observability-ux.md`. Until that check ships, the risk
   is latent but mitigated by decision 018's existing defense-in-depth
   (`ConditionPathIsMountPoint` on the unit).

2. **Runtime budget: `battery.runtime.low` vs. `FINALDELAY` + systemd
   shutdown + `TimeoutStopSec = 5min`.** Default ~120s on APC may not
   cover the observed total. M7b remediates by raising the threshold; see
   ADR 020 Open Question 3.

3. **Alert-model integration is a known gap in v1.** Users who aren't
   watching `braid ups status` won't see on-battery or comms-loss
   conditions asynchronously. Documented in the ADR (see Pre-M1); the
   full user-guide coverage lands in
   `plans/wip/ups-observability-ux.md`. The follow-up ADR for
   `AlertCause` persistence semantics is a separate plan not yet
   scheduled.

4. **Dummy-ups driver mode matters for the test.** M7 drives the UPS via
   `upsrw`, so the fixture must use `dummy-ups` in `dummy-once` mode with
   a `.dev` file (per `reference/nut/docs/man/dummy-ups.txt:90,100`):
   `dummy-once` loads the file into memory once and preserves `upsrw`
   writes; `dummy-loop` re-reads the file and would overwrite in-memory
   `upsrw` changes before `upsmon` reacts. The same constraint applies
   in Plan 3.

5. **Recovery gap latent until Plan 3 runs.** ADR 020's "mid-mutation
   power loss is a supported recovery case" claim is only honest after
   `plans/wip/forced-shutdown-recovery-proof.md` audits
   `cli/src/recover.rs` and proves the per-mutation matrix. This plan's
   shipping does **not** make that claim true. See cross-plan status
   dependency below.

## Cross-plan status dependency

**ADR 020 (`docs/decisions/020-ups-integration.md`) stays `Draft` when
this plan passes.** It flips to `Active` only after
`plans/wip/forced-shutdown-recovery-proof.md` passes its full matrix
(M11-M14 in the original monolithic plan). Splitting the plans does not
split the ADR: ADR 020 still covers the complete UPS-integration shape
and its status gate is still "full recovery-proof obligation satisfied."

If you are tempted to split ADR 020 into an "Active" v1-safety-core
sub-ADR and a separate recovery-proof ADR, propose that change
explicitly in a code review or the
`forced-shutdown-recovery-proof.md` plan before acting on it. The
current structure intentionally treats the three guarantees as one
contract whose ADR only activates when all parts hold.
