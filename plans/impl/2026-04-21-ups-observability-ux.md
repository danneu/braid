# UPS Observability + UX

Derived from: `plans/wip/purrfect-honking-rabin.md` (the original monolithic
UPS-integration plan). This file is one of three follow-ons:

- `plans/wip/ups-v1-safety-core.md` -- smallest shippable safety feature
  set behind `braid.ups.enable`. **Must ship before this plan starts.**
- `plans/wip/ups-observability-ux.md` (this file) -- rich parser, TUI
  section, doctor checks, fixture / canary / unstable-lane machinery,
  README UX coverage.
- `plans/wip/forced-shutdown-recovery-proof.md` -- `braid recover` audit
  and the per-mutation VM matrix. Independent of this plan; either order
  of landing is fine.

## Scope

Turn the minimal UPS surface shipped in `ups-v1-safety-core.md` into the
full operator UX and observability surface promised by ADR 020's
guarantee (3) ("live UPS state visible in `braid ups status` and the
TUI"). Concretely:

1. Promote NUT to the fully-maintained parser-critical toolchain surface
   (fixture capture from a VM, golden tests, parser-canary, unstable
   lane) alongside btrfs-progs / cryptsetup / util-linux.
2. Expand `parse_upsc` from the minimal `{status_flags, extra}` shape to
   the rich typed model: battery, input, device, load, test result,
   realpower nominal, runtime.
3. Polish `braid ups status` with a curated human summary and a stable
   `--json` shape keyed to the rich model.
4. Add the TUI Data-tab UPS section with 5s polling, colored severity,
   and parity with the existing Fans section.
5. Add `braid doctor` checks that catch UPS-adjacent configuration
   faults, particularly "pool mounted but `braid-online.service`
   inactive under UPS."
6. Update cross-cutting toolchain docs (principles, toolchain-pinning
   ADR, AGENTS.md parser-compat table) now that NUT really is a
   maintained parser-critical surface.
7. Update `README.md` with end-user guidance covering `braid.ups.enable`,
   the TUI UPS section, and the "no async UPS alerts in v1" limitation.

The end state: operators have full live visibility into UPS state from
the CLI and TUI, the parser is protected against silent upstream
behavior drift, and the user guide honestly describes both what works
and the v1 alert-integration gap.

## Non-goals

- **Alert-model integration for UPS events.** Still deferred to a
  future ADR as in `ups-v1-safety-core.md`. This plan surfaces UPS state
  synchronously via CLI/TUI, but does not introduce `AlertCause`
  variants, `NOTIFYCMD` wiring, or `/var/lib/braid/ups-alerts/` latch
  files. A user who is not looking at the TUI / `braid ups status` will
  not be notified asynchronously when the UPS goes on battery. The
  README must name this gap explicitly.
- **Per-mutation power-loss matrix.** That is
  `plans/wip/forced-shutdown-recovery-proof.md`'s work.
- **ADR 020 status flip to `Active`.** Still blocked on the
  recovery-proof plan. This plan's completion contributes to the
  "observability" guarantee but is not the final gate.
- **Backwards compatibility with the minimal parser shape in
  `ups-v1-safety-core.md`.** `UpscOutput` is free to change shape here;
  preflight and the minimal `braid ups status` are updated to consume
  the richer model in the same PR that lands the new shape.

## Dependencies

- `plans/wip/ups-v1-safety-core.md` must be merged first. This plan
  builds directly on:
  - the `braid.ups.*` module options and `power.ups.*` wiring in
    `modules/braid/ups.nix`,
  - the minimal `parse_upsc` module and `UpscOutput` skeleton,
  - the `Commands::Ups(UpsArgs)` CLI scaffold and the `config.rs` `Ups`
    field,
  - the wrapped binary already having `upsc` on PATH via
    `modules/braid/wrapper.nix`.
- `docs/decisions/020-ups-integration.md` (`Draft`) -- already refined
  by Plan 1 to remove alert-integration from v1 scope. This plan does
  not re-touch the ADR body; it contributes only the cross-cutting
  docs updates promised there.
- `docs/decisions/010-toolchain-pinning.md` (`Active`) -- updated by
  this plan to add NUT to the pinned parser-critical list.
- `docs/principles.md` principle 10 (Pinned Toolchain) -- updated to
  name NUT.
- `AGENTS.md` -- parser-compatibility section and reference-source
  table updated to name NUT.
- Local NUT reference at `reference/nut/` -- `clients/upsc.c` and
  `clients/upsrw.c` authoritative for output shape and `SET` semantics.
- Existing parser-critical workflow: see the "Parser Compatibility"
  section of `AGENTS.md` for stable-lane and unstable-lane obligations.

## Milestones

Milestones are ordered so that parser and fixture work lands before the
consumers (CLI polish, TUI, doctor) that depend on the rich types. The
cross-cutting parser-critical docs updates (principle 10, ADR 010,
AGENTS.md) are intentionally **folded into M3** rather than standing as a
separate upfront milestone: the docs become true only when the fixture
and canary machinery lands, so they must land in the same change.

### M1 -- Rich `parse_upsc` model

Replace the minimal `UpscOutput` shipped by Plan 1 with the full typed
shape.

- Modify [`cli/src/parse/upsc.rs`](../../cli/src/parse/upsc.rs):

  ```rust
  pub struct UpscOutput {
      pub status_flags: HashSet<UpsStatusFlag>,
      pub battery: BatteryFields,
      pub load_pct: Option<u8>,
      pub realpower_nominal_watts: Option<u32>,
      pub input: InputFields,
      pub test_result: Option<String>,
      pub device: DeviceFields,
      pub extra: BTreeMap<String, String>,
  }
  ```

  With sibling types:
  - `BatteryFields { charge_pct, runtime_secs, voltage, type_, mfr_date,
    runtime_low_secs }` (all `Option`).
  - `InputFields { voltage, transfer_low, transfer_high, sensitivity }`
    (all `Option`).
  - `DeviceFields { model, mfr, serial, type_ }` (all `Option`).

- Modify [`cli/src/parse/types.rs`](../../cli/src/parse/types.rs) or a
  new sibling file to hold the new types.
- Update the preflight call site in
  [`cli/src/preflight.rs`](../../cli/src/preflight.rs) if the consumed
  field access changes. (Preflight should still only look at
  `status_flags`; this is just a shape migration.)
- Update the minimal `braid ups status` path from Plan 1 to consume the
  new shape (rich output in M4 below).

**Verify:** `just test-rust` green against placeholder fixtures. Type
changes compile cleanly. Preflight behavior in `MockRunner` tests is
unchanged.

### M2 -- Fixture capture machinery

Bring NUT into the same fixture-capture discipline used for
btrfs-progs, cryptsetup, and util-linux. The machinery lands here,
not in Plan 1, because the rich parser model is now what the fixtures
need to exercise.

- Create [`tests/capture-ups-fixtures.nix`](../../tests/capture-ups-fixtures.nix).
  - Boots a NUT-enabled VM with the `dummy-ups` driver in `dummy-once`
    mode (per `reference/nut/docs/man/dummy-ups.txt:90,100`) pointing
    at a `.dev` file.
  - Rewrites the dummy-ups state via `upsrw -s` to each target state,
    then captures `upsc <name>` output to a fixture file.
  - Separate VM derivation from `tests/capture-tool-fixtures.nix`
    because the NUT setup diverges (dummy-ups user, SET credential,
    `upsrw` sequencing).
- Fixtures to capture under
  `cli/tests/fixtures/nixos-25.11/upsc/`:
  - `upsc-online.txt`
  - `upsc-onbattery.txt`
  - `upsc-lowbattery.txt`
  - `upsc-replace-battery.txt`
  - `upsc-daemon-down.stderr` (captured from `upsc` invoked while
    `upsd.service` is stopped)
- Mirror capture under `cli/tests/fixtures/nixos-unstable/upsc/`.
- Modify `justfile`:
  - Add `capture-ups-fixtures` recipe (stable).
  - Add `capture-ups-fixtures-unstable` recipe.
  - Wire `capture-ups-fixtures` into `capture-all-fixtures`.
  - Wire `capture-ups-fixtures-unstable` into
    `capture-all-fixtures-unstable`.

**Verify:** Running `just capture-ups-fixtures` and
`just capture-ups-fixtures-unstable` on a clean tree produces the five
fixtures per lane; re-running is idempotent; captured files are
stable across runs (no timestamp / PID noise in the output -- scrub or
document if present).

### M3 -- Golden parser tests, parser canary, NUT parser-critical promotion

Once fixtures exist, land the parser-protection net **and** the
cross-cutting docs that declare NUT parser-critical. These land in the
same change so the contract docs never claim a guarantee before the
machinery exists to enforce it.

Parser-protection net:

- Modify [`cli/tests/golden_nixos_25_11.rs`](../../cli/tests/golden_nixos_25_11.rs)
  -- golden tests for each of the five fixtures using the existing
  `support/golden_common.rs` pattern. Each test asserts the expected
  shape of `UpscOutput` against the fixture.
- Create a sibling `cli/tests/golden_nixos_unstable.rs` entry (or add
  to the existing file, following the pattern of other parsers) so
  `just test-rust-unstable` exercises the unstable fixtures.
- Add a `braid-status-ups` parser-canary VM check under the existing
  parser-canary harness:
  - Boot a dummy-ups-configured NUT VM.
  - Invoke `braid ups status` (leveraging the wrapper's `upsc` PATH).
  - Assert the output parses cleanly and the status-flag set matches
    the expected live-tool output.
  - Run via `just test-parsers`.

Cross-cutting docs (same change):

- Modify [`docs/decisions/010-toolchain-pinning.md`](../../docs/decisions/010-toolchain-pinning.md)
  -- add `nut` (NUT, Network UPS Tools; `pkgs.nut`) to the pinned
  parser-critical list, explain the reason (parsing `upsc` output for
  preflight safety and operator visibility), and describe the
  fixture-refresh obligation on nixpkgs bumps to `nut`.
- Modify [`docs/principles.md`](../../docs/principles.md) -- Principle 10
  (Pinned Toolchain) names NUT alongside btrfs-progs / cryptsetup /
  util-linux / smartmontools.
- Modify [`AGENTS.md`](../../AGENTS.md):
  - Parser-Compatibility section's stable-lane and unstable-lane
    obligations name NUT.
  - Reference-source table's NUT row already exists; verify the row
    points to the source paths used by this plan's fixture work
    (`clients/upsc.c`, `clients/upsrw.c`, `drivers/dummy-ups.c`,
    `conf/*.sample`, `docs/man/*`).
- Confirm the `braid.packages.nut` pin exists (created in Plan 1's M1).

**Verify:**
- `just test-rust` passes all stable golden tests.
- `just capture-all-fixtures-unstable && just test-rust-unstable`
  passes cleanly against an unstable toolchain.
- `just test-parsers` includes `braid-status-ups` and passes.
- Bumping `braid.packages.nut` in `modules/braid/options.nix` to a
  different nixpkgs-pinned version causes the unstable lane to flag
  drift (manually tested or dry-run before merge).
- `AGENTS.md`, principle 10, and ADR 010 land in the same commit as
  the golden / canary machinery; a reviewer reading the contract docs
  finds working backing machinery in the same diff.

### M4 -- Rich `braid ups status` output + stable `--json`

Replace the minimal human output shipped in Plan 1 with the
curated summary, and introduce the stable `--json` flag.

- Modify [`cli/src/ups.rs`](../../cli/src/ups.rs):
  - Human render:
    - Decoded status line (e.g. "Status: OL" / "Status: OB LB" with
      flags color-coded in terminal output when TTY).
    - Battery charge %.
    - Runtime formatted `HH:MM`.
    - Load %.
    - Estimated watts, computed only when both `load_pct` and
      `realpower_nominal_watts` are present; labeled "estimated";
      otherwise omitted.
    - Input voltage with transfer-low / transfer-high context.
    - Device model / manufacturer.
    - Battery manufacture date if present.
    - Last test result if present.
    - Raw extras table is no longer printed -- users who want that can
      run `upsc` directly.
  - `--json` flag: `serde_json::to_string_pretty(&UpscOutput)`.
    `#[derive(Serialize)]` on `UpscOutput` and siblings.
  - Daemon-down error in `--json` mode: `{"error": "daemon_down"}`
    (exit 1).
  - Missing/disabled config in `--json` mode:
    `{"error": "ups_not_enabled"}` (exit 0, unchanged human path still
    prints the enable hint).
- Modify [`cli/src/main.rs`](../../cli/src/main.rs) -- add the
  `#[arg(long)] json: bool` field to the `Status` subcommand.

**Verify:**
- `braid ups status` renders the curated summary against each captured
  fixture.
- `braid ups status --json | jq .` parses cleanly on a live VM.
- Unit tests snapshot the human render for each fixture.
- Unit tests assert the JSON shape against each fixture (serde round
  trip).

### M5 -- TUI UPS section

Add the UPS panel to the Data tab, parallel to the Fans section (not
nested), matching the polling and color patterns already in use.

- Modify [`cli/src/tui/model.rs:13-45,87-94`](../../cli/src/tui/model.rs):

  ```rust
  pub struct UpsSnapshot {
      pub flags: HashSet<UpsStatusFlag>,
      pub battery_charge_pct: Option<u8>,
      pub runtime_secs: Option<u32>,
      pub load_pct: Option<u8>,
      pub watts_estimated: Option<u32>,
      pub daemon: DaemonStatus,
      pub probed_at: Instant,
  }
  ```

  Plus `ups: Option<UpsSnapshot>` and `ups_probe_inflight: bool` on the
  TUI model struct.
- Modify [`cli/src/tui/effect.rs:33-39`](../../cli/src/tui/effect.rs) --
  `Effect::ProbeUps { name }`, `Effect::ScheduleUpsProbe { delay }`.
- Modify [`cli/src/tui/app.rs:43-51,86-93`](../../cli/src/tui/app.rs) --
  `fn ups_probe_effect()` + `Message::UpsProbeFinished(UpsSnapshot)`;
  the `r` key refreshes both pool and UPS; `UPS_PROBE_INTERVAL =
  Duration::from_secs(5)` matching fans.
- Modify [`cli/src/tui/probe.rs`](../../cli/src/tui/probe.rs) --
  `fn probe_ups_for_tui(name) -> UpsSnapshot` invoking `upsc` +
  `parse_upsc` and mapping daemon-down to `DaemonStatus::DaemonDown`.
- Modify [`cli/src/tui/view/mod.rs:93-206,831-843`](../../cli/src/tui/view/mod.rs)
  -- new `ups_section(snapshot)` rendered conditional on
  `ups_config.is_some()`.
  - Color severity:
    - `OL` Green,
    - `OB` Yellow,
    - `LB` Red,
    - `TESTFAIL` / `COMMBAD` Red,
    - daemon-down DarkGray.
  - Matches the Fan `DaemonStatus` pattern at
    `cli/src/tui/view/mod.rs:149-157`.
- Watts displayed only when both `load_pct` and
  `realpower_nominal_watts` are present; labeled "estimated."
- Unknown status tokens preserved via `UpsStatusFlag::Unknown(String)`
  (already in the enum from Plan 1).

**Verify:** `braid tui` shows the UPS section with correct colors.
Driving the state with `upsrw -s 'ups.status=OB LB' -u <testops> -p
<pass> <upsname>@localhost` in a dummy-ups VM causes colors to transition
Yellow -> Red within one 5s poll. (`-s` plus quoted value required so
`OB LB` is one argv token per
`reference/nut/clients/upsrw.c`.)

### M6 -- `braid doctor` checks

Add the UPS-adjacent configuration checks. These are diagnostic /
observability concerns, not alert causes, so they live in `doctor`.

- Modify [`cli/src/doctor.rs:731-740`](../../cli/src/doctor.rs) to
  append:
  - `check_ups_daemon_up(ctx)` -- when `ups.enable`, runs `upsc`;
    `Warn` if daemon down. (Higher severity is tempting, but `Warn`
    matches the existing posture that the operator can fix daemon
    state without braid intervention.)
  - `check_braid_online_active_when_mounted(ctx)` -- when `ups.enable`
    and the pool is mounted, assert `systemctl is-active
    braid-online.service` returns `active`; `Fail` (high severity)
    otherwise. This is the critical one: without `braid-online` active,
    `SHUTDOWNCMD = systemctl poweroff` does not run `braid lock`'s
    `ExecStop` and the Plan 1 safety guarantee silently breaks.
- Both checks guard on config probe: absent UPS block -> both checks
  skip.

**Verify:**
- Unit tests using `MockRunner` exercise each failure branch.
- A targeted VM test simulates `braid-online.service` inactive while
  the pool is mounted under UPS, and asserts the expected `doctor`
  output severity.

### M7 -- README and user-guide coverage

Update [`README.md`](../../README.md) now that the user-facing surface
is real. Style per `AGENTS.md` ("brief, cookbook-like -- short
descriptions with copy-paste examples. Not reference material.").

- Add a UPS section covering:
  - `braid.ups.enable = true` minimal config example.
  - `braid.ups.{name, driver, port}` escape-hatch options.
  - `braid ups status` usage, including `--json`.
  - The TUI Data-tab UPS section -- what the colors mean.
  - Shutdown-at-critical behavior (what happens when LB fires).
  - The "mutations refuse to start on battery" rule -- example error
    message and how to recover.
  - **v1 limitation:** no async notification when the UPS goes on
    battery or loses comms. Users must watch the TUI or run `braid
    ups status` to see UPS state. Alert-model integration is a
    separate future ADR.
- Modify [`docs/index.md`](../../docs/index.md) summary line for
  ADR 020 if anything changed (status still `Draft` per the ADR's own
  status gate; only the summary sentence may need to reflect that
  observability is now real).

**Verify:** README reads cleanly end-to-end. A new user can follow
the `braid.ups.enable` example to a working TUI with colors.

## Critical files

**Module:**
- No module changes beyond Plan 1 expected. If the `cli.nix` config
  emit needs more fields (e.g. driver / port visible to CLI), update
  there.

**CLI (Rust):**
- Modify `cli/src/parse/upsc.rs` (rich model).
- Modify `cli/src/parse/types.rs` or sibling.
- Modify `cli/src/ups.rs` (curated output + `--json`).
- Modify `cli/src/main.rs` (`--json` flag).
- Modify `cli/src/preflight.rs` (consume richer shape if needed; should
  be minimal since preflight only reads `status_flags`).
- Modify `cli/src/doctor.rs` (new UPS checks).

**TUI:**
- Modify `cli/src/tui/model.rs`, `cli/src/tui/effect.rs`,
  `cli/src/tui/app.rs`, `cli/src/tui/probe.rs`,
  `cli/src/tui/view/mod.rs`.

**Tests:**
- Create `tests/capture-ups-fixtures.nix`.
- Create fixture files under
  `cli/tests/fixtures/nixos-25.11/upsc/` and
  `cli/tests/fixtures/nixos-unstable/upsc/`.
- Modify `cli/tests/golden_nixos_25_11.rs` and the unstable sibling.
- Add the `braid-status-ups` parser-canary VM check under the existing
  parser-canary harness.
- Add targeted `braid doctor` VM test (one small module).

**Docs:**
- Modify `docs/decisions/010-toolchain-pinning.md` (NUT added).
- Modify `docs/principles.md` (Principle 10).
- Modify `AGENTS.md` (parser-compat table).
- Modify `docs/index.md` if summary lines need refresh.
- Modify `README.md` (user-guide coverage).

**Build:**
- Modify `justfile` -- `capture-ups-fixtures`,
  `capture-ups-fixtures-unstable`; include in
  `capture-all-fixtures` / `capture-all-fixtures-unstable`.

## Verification

**Unit tests (`just test-rust`):**
- Golden parser tests for each of the five stable fixtures.
- Snapshot tests for the curated human render against each fixture.
- JSON-shape tests for each fixture (serde round trip).
- `braid doctor` check tests in OK / daemon-down / `braid-online`
  inactive scenarios.

**Unstable lane (`just test-rust-unstable`):**
- Golden parser tests for the unstable fixtures.
- Lane is expected to flag upstream drift before it hits the stable
  lane -- consistent with the forecast-lane model in `AGENTS.md`.

**Parser canary (`just test-parsers`):**
- `braid-status-ups` check confirms `braid ups status` still parses
  live NUT output under the current pin.

**VM tests (`just test-vm`):**
- Targeted `braid doctor` VM test for the `braid-online inactive +
  mounted + UPS` fault path.
- No new mutation-matrix VM tests in this plan; those belong to
  `forced-shutdown-recovery-proof.md`.

**Manual smoke on real hardware:**
- Hook the target NAS to a real UPS.
- Confirm `braid tui` shows the UPS section with the right colors
  under OL / OB.
- Unplug UPS input; confirm the section flips Green -> Yellow within
  one poll.
- `braid ups status --json | jq '.status_flags'` prints the expected
  array.

## Risks

1. **Parser drift on nixpkgs bump.** With NUT promoted to
   parser-critical, any `nixpkgs` bump that changes `nut` output
   format becomes a required fixture-refresh event. The obligation
   lives in `AGENTS.md`'s parser-compatibility section after M3.
   Missing the refresh can silently break preflight. Mitigation:
   unstable lane fires early; `just test-rust-unstable` included in
   periodic maintenance.

2. **Minimal-parser migration risk.** Plan 1's preflight callers are
   adjusted in M1 to consume the new `UpscOutput` shape. If preflight
   accidentally grows a dependency on a new typed field that is
   `Option::None` for some real UPS, preflight can start erroring on
   UPS hardware that Plan 1 handled correctly. Mitigation: preflight
   contract remains "only look at `status_flags`"; new-field access is
   confined to `braid ups status` / TUI / doctor.

3. **TUI `UpsSnapshot` fields out of sync with `UpscOutput`.** Two
   snapshot shapes exist (parsed model vs. TUI-facing summary) and can
   drift. Mitigation: the TUI probe is the single converter; TUI tests
   snapshot the converter output.

4. **Dummy-ups driver mode matters for fixture capture.** As in Plan 1:
   use `dummy-once`, not `dummy-loop` (per
   `reference/nut/docs/man/dummy-ups.txt:90,100`). Otherwise `upsrw`
   writes get overwritten before capture and fixtures become
   nondeterministic.

5. **README promises more than the alert model delivers.** The v1
   limitation paragraph must be explicit: no async notification. If the
   README oversells the feature, operators may assume they can walk
   away from a mounted pool under UPS and get pinged on battery -- they
   cannot. Mitigation: the "no async notification" sentence is a
   required part of M7, not optional polish.

6. **Alert-model integration remains the known v1 gap.** Unchanged
   from Plan 1. The follow-up ADR for `AlertCause` persistence semantics
   is a separate plan not yet scheduled.

## Cross-plan status dependency

ADR 020 remains `Draft` when this plan passes. It flips to `Active`
only after `plans/wip/forced-shutdown-recovery-proof.md` also passes.
This plan contributes to guarantee (3) (live UPS state visible in
`braid ups status` and the TUI) but does not close guarantee (1)'s
recovery half on its own.

If ADR 020 should be split so the observability guarantee flips to
`Active` independently of the recovery guarantee, propose that change
in a code review or the recovery-proof plan before acting on it. The
current structure keeps all three guarantees under one contract.
