# UPS Integration -- Umbrella Plan

> **This plan is now an umbrella / index.** The detailed milestone work
> lives in three focused follow-on plans. The high-level rationale,
> design summary, and cross-cutting context stay here because they span
> all three sub-plans and do not fit cleanly in any one of them.
>
> Follow-on plans:
>
> - [`plans/wip/ups-v1-safety-core.md`](./ups-v1-safety-core.md) --
>   smallest shippable UPS safety feature set behind `braid.ups.enable`.
> - [`plans/wip/ups-observability-ux.md`](./ups-observability-ux.md) --
>   rich parser, TUI section, doctor checks, fixture / canary / unstable-
>   lane machinery, README UX coverage.
> - [`plans/wip/forced-shutdown-recovery-proof.md`](./forced-shutdown-recovery-proof.md)
>   -- `braid recover` audit and per-mutation VM matrix that proves
>   mid-mutation power loss is survivable. Final gate for flipping ADR
>   020 to `Active`.

## Context

braid needs first-class UPS support. The three follow-on plans together
aim to deliver a state in which enabling `braid.ups.enable = true`
**will** give a home NAS three guarantees: (1) orderly shutdown before
battery exhaustion for ordinary mounted operation, (2) preflight refusal
to start pool-mutating commands while already on battery, (3) live UPS
state visible in `braid ups status` and the TUI. Mid-mutation power loss
will be a supported recovery case handled by the existing journal +
`braid recover` path, proven by VM tests per mutation class in
`plans/wip/forced-shutdown-recovery-proof.md`.

Current state as of this writing: **none of the three guarantees have
shipped yet.** ADR 020 is `Draft`. The guarantees above describe the
target end state of the split plans, not the live contract. The
recovery-proof plan in particular is the load-bearing gate that keeps
ADR 020 in `Draft` until its full matrix passes; readers should not
infer from the guarantee language that mid-mutation recovery is already
proven.

Integrating UPS conditions into the shared alert model is **deferred to a
future ADR**. Decision 014 currently guarantees "alerts stay latched
until `braid ack`" -- that is the right shape for event-driven causes
(disk errors, smartd), but wrong for live-state conditions like on-
battery / low-battery (users expect those to clear when the UPS returns
to OL). Making alerts auto-dismiss for UPS would require splitting
`AlertCause` by persistence semantics (`LatchedUntilAck` vs.
`ActiveWhileConditionHolds`) and updating `merge_into_latch`, `ack`,
`status`, and the tests. That is a core-invariant change that deserves
its own ADR; smuggling it into UPS v1 would conflate two distinct
concerns.

Decision record:
[`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
(Status: Draft).

Outcome of the split plans: ship UPS v1 (state + shutdown + preflight)
behind `braid.ups.enable`. Flip the ADR to `Active` once the
`forced-shutdown-recovery-proof.md` matrix passes. Alert integration
lives in a separate future plan and ADR.

## Why three plans instead of one

The original monolithic plan mixed three distinct scopes that had
different shipping cadences, different reviewers, and different risks:

1. **Safety core.** Module skeleton, credential file, `SHUTDOWNCMD` =
   `systemctl poweroff`, preflight refusal, a minimal-enough
   `parse_upsc` + `braid ups status` to support preflight. This is the
   smallest shippable safety feature; its risk is mostly systemd +
   runtime-budget wiring.
2. **Observability / UX.** Rich parser, stable JSON, TUI panel,
   `braid doctor` checks, fixture capture, parser canary, unstable
   lane, user-guide coverage. Risk is mostly parser drift and
   UX consistency.
3. **Recovery proof.** Auditing `cli/src/recover.rs` and proving with VM
   tests that the journal recover path survives forced shutdown during
   each mutation class. Risk is recovery correctness, which is the
   load-bearing claim for flipping ADR 020 `Active`.

Splitting lets each scope progress on its own timeline and prevents the
recovery-proof gate from blocking the safety-core ship. It also lets
reviewers focus on one concern at a time.

## Pending ADR refinements (preserved from the original plan)

The full rationale is preserved here because subsequent plans reference
it. Concretely, refine
[`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
before coding starts:

- **Remove the "UPS events become first-class alert causes" section
  entirely.** Replace with a short paragraph noting that alert-model
  integration is deferred to a future ADR, because live-state UPS
  conditions do not fit decision 014's "latched until ack" invariant
  and reconciling them requires a broader alert-model change (splitting
  `AlertCause` by `LatchedUntilAck` vs. `ActiveWhileConditionHolds`).
- **Update guarantee (3) in the Context.** Change from "UPS events
  latched into braid's shared alert model" to "live UPS state visible
  in `braid ups status` and the TUI; live UPS status is used for
  preflight safety and upsmon critical-state shutdown (normally `OB` +
  `LB`)."
- **Drop the `/var/lib/braid/ups-alerts/` latch-file design.** Not used
  in v1 since there is no alert integration at all.
- Keep the "Upsmon credential lifecycle" subsection as written --
  credential generation is still needed because NUT requires
  upsmon<->upsd auth even in standalone mode.
- Keep the "`braid-online` becomes safety-critical under UPS" subsection
  -- `braid doctor` still flags "pool mounted but `braid-online`
  inactive" under UPS, and that is a configuration check, not an alert
  cause.

**Ownership:** this ADR refinement is Pre-M1 in
[`ups-v1-safety-core.md`](./ups-v1-safety-core.md).

Cross-cutting doc refinements for NUT as a pinned parser-critical tool
(principle 10, `docs/decisions/010-toolchain-pinning.md`, the
parser-compatibility table in `AGENTS.md`) are deferred to
[`ups-observability-ux.md`](./ups-observability-ux.md) M3, where they
land in the same change as the golden tests and parser canary that
make the parser-critical classification true.

NUT source is already available at
[`reference/nut/`](../../reference/nut/) (fetched via
`scripts/fetch-references.py`). Consult `reference/nut/clients/upsmon.c`,
`upsc.c`, `upsrw.c`, and `reference/nut/conf/*.sample` when implementing
or debugging.

## Design summary (preserved from the original plan)

### Scope (v1)

Live UPS state is surfaced in `braid ups status` and the TUI. Live UPS
status is consulted directly by preflight when a user runs a mutation,
and by `upsmon`'s own `SHUTDOWNCMD` wiring when `upsmon` declares the
UPS critical (typically `OB` + `LB` together; see
[`reference/nut/clients/upsmon.c:1404`](../../reference/nut/clients/upsmon.c)).
**No `AlertCause` integration, no `NOTIFYCMD`, no monitor-lifecycle
changes.**

### Shutdown path

- `power.ups.upsmon.settings.SHUTDOWNCMD = "${pkgs.systemd}/bin/systemctl
  poweroff"` (overrides nixpkgs' default of `shutdown now` using
  `mkForce` or a plain assignment -- nixpkgs uses `mkDefault`).
- systemd shutdown sequence unwinds `braid-online.service`
  ([decision 018](../../docs/decisions/018-systemd-lifecycle.md)) ->
  btrfs umount -> luks close.

### Credential lifecycle

- `braid-ups-secrets.service` (oneshot): if `/var/lib/braid/upsmon.pass`
  is absent, writes `head -c 24 /dev/urandom | base64` with `0600
  root:root`.
- `before = [ "upsd.service" "upsmon.service" ]` and `requiredBy = [
  "upsd.service" "upsmon.service" ]`.
- `power.ups.users.<name>.passwordFile` and
  `power.ups.upsmon.monitor.<name>.passwordFile` both reference the
  file.
- `/var/lib/braid/` already created by `storage.nix:22` tmpfiles -- no
  new directory needed.

### `braid ups status` shape

- `Commands::Ups(UpsArgs)` with `Status { json: bool }` subcommand.
- Reads `/etc/braid/config.json` for `ups` block; if absent or
  `ups.enable=false`, prints a helpful enable-hint and exits 0.
- Otherwise invokes `upsc <name>`, passes through `parse_upsc`, renders
  curated human summary or `serde_json::to_string_pretty(&UpscOutput)`
  with `--json`.
- Daemon down (`upsc` command fails) renders as a distinct error message
  with exit 1 and `{"error": "daemon_down"}` under `--json`.

The safety-core plan ships a minimal version of this (no `--json`, raw
extras passthrough). The observability plan adds the curated human
summary and stable `--json` shape.

### Preflight on battery

- New `check_ups_not_on_battery(runner, ups_name)` in
  [`cli/src/preflight.rs`](../../cli/src/preflight.rs). Called from
  `add`, `remove`, `remove-missing`, `replace` preflights, before
  journal write.
- Returns `Validation("cannot verify UPS is on utility power -- refusing
  to start <op>. Check 'braid ups status', restore utility power, then
  retry.")` per each command's `Validation` variant. (Fail-closed
  wording stays honest when the real failure is daemon-down or
  malformed status, not just an on-battery condition.)
- Check is a no-op when `ups` block is absent from config.

### TUI UPS section

- Lives in the Data tab alongside Fans (parallel, not nested).
  `cli/src/tui/view/mod.rs` adds `ups_section`.
- Polling mirrors fan probe: `Effect::ProbeUps { name }` +
  `Effect::ScheduleUpsProbe { delay }` on the same 5s cadence
  ([`cli/src/tui/app.rs:17,43-51`](../../cli/src/tui/app.rs)).
- Colors: `OL` Green, `OB` Yellow, `LB` Red, `TESTFAIL`/`COMMBAD` Red,
  daemon-down DarkGray (matches Fan `DaemonStatus` pattern at
  `cli/src/tui/view/mod.rs:149-157`).
- `ups.status` parses as a `HashSet<UpsStatusFlag>`; unknown tokens
  preserved via `UpsStatusFlag::Unknown(String)`.
- Watts displayed only when both `ups.load` and
  `ups.realpower.nominal` are present; labeled "estimated"; otherwise
  omitted.

## Crosswalk

Where each section of this original plan now lives.

| Original section / milestone                                            | New home                                                                 |
| ----------------------------------------------------------------------- | ------------------------------------------------------------------------ |
| Context                                                                 | This file (preserved above); individual plans reference it.              |
| Pending ADR refinements -- ADR 020 body (remove alert section, etc.)    | `ups-v1-safety-core.md` Pre-M1                                           |
| Pending ADR refinements -- principle 10, ADR 010, AGENTS.md NUT entries | `ups-observability-ux.md` M3 (folded in with the golden / canary machinery that makes the classification true) |
| Design summary (Scope, Shutdown path, Credential lifecycle, etc.)       | This file (preserved above); individual plans reference it.              |
| M1 -- Module skeleton + `power.ups` wiring + package pin                | `ups-v1-safety-core.md` M1                                               |
| M2 -- `parse_upsc` + fixtures + golden tests (minimal slice)            | `ups-v1-safety-core.md` M2 (minimal parser + hand-written fixtures only) |
| M2 -- `parse_upsc` + fixtures + golden tests (remainder)                | `ups-observability-ux.md` M1 (rich model) + M2 (fixtures) + M3 (golden + canary + parser-critical docs) |
| M3 -- `braid ups status` CLI (minimal slice)                            | `ups-v1-safety-core.md` M3 (flags + daemon-down, no `--json`)            |
| M3 -- `braid ups status` CLI (remainder: curated summary, `--json`)     | `ups-observability-ux.md` M4                                             |
| M4 -- TUI UPS section                                                   | `ups-observability-ux.md` M5                                             |
| M5 -- Alert integration                                                 | Deferred. Separate future ADR + plan; not in any of the three new plans. |
| M6 -- Preflight reject on battery                                       | `ups-v1-safety-core.md` M4 (incl. new mandatory `ups-preflight-on-battery` VM smoke test) |
| M7 -- `braid-ups-secrets.service`                                       | `ups-v1-safety-core.md` M5                                               |
| M8 -- `SHUTDOWNCMD = systemctl poweroff`                                | `ups-v1-safety-core.md` M6                                               |
| M9 -- `braid doctor` checks                                             | `ups-observability-ux.md` M6                                             |
| Pre-M11 -- `braid recover` audit                                        | `forced-shutdown-recovery-proof.md` Pre-M11                              |
| M10a -- Ordinary-mounted-operation LB -> clean poweroff VM test         | `ups-v1-safety-core.md` M7                                               |
| M10b -- Battery threshold remediation                                   | `ups-v1-safety-core.md` M7b                                              |
| M11 -- LB during `braid replace`                                        | `forced-shutdown-recovery-proof.md` M3                                   |
| M12 -- LB during `braid remove`                                         | `forced-shutdown-recovery-proof.md` M4                                   |
| M13 -- LB during `braid remove-missing`                                 | `forced-shutdown-recovery-proof.md` M5                                   |
| M14 -- LB during `braid add` balance                                    | `forced-shutdown-recovery-proof.md` M6                                   |
| ADR 020 status flip to `Active`                                         | `forced-shutdown-recovery-proof.md` M7 (unchanged dependency)            |
| Critical files (module / CLI / TUI / tests / docs / build)              | Split per plan; see each plan's own "Critical files" section.            |
| Verification (unit / parser canary / VM / manual smoke)                 | Split per plan; see each plan's own "Verification" section.              |
| Risks -- runtime budget                                                 | `ups-v1-safety-core.md` Risks (M7b is the remediation path)              |
| Risks -- `braid recover` gap                                            | `forced-shutdown-recovery-proof.md` Risks (Pre-M11 resolves)             |
| Risks -- dummy-ups fixture reliability                                  | `forced-shutdown-recovery-proof.md` Risks + `ups-v1-safety-core.md`      |
| Risks -- `nut` unstable forecast lane                                   | `ups-observability-ux.md` Risks (M2 + M3 own the lane)                   |
| Risks -- alert integration gap                                          | All three plans acknowledge this; README coverage in observability M7.   |

## Existing functions / utilities to reuse (preserved from the original plan)

Used across multiple plans; keep this list here so sub-plans can
reference it without duplicating.

- `parse::*` pattern -- mirror `parse_smartctl` shape at
  [`cli/src/parse/smartctl.rs:76`](../../cli/src/parse/smartctl.rs).
- `require_mutation_preflight` + `check_*` helpers --
  [`cli/src/preflight.rs:15-321`](../../cli/src/preflight.rs).
- `DaemonStatus` rendering for Fans section --
  [`cli/src/tui/view/mod.rs:149-157`](../../cli/src/tui/view/mod.rs).
- `Effect::ProbeFan` + `FAN_PROBE_INTERVAL` --
  [`cli/src/tui/app.rs:17,43-51`](../../cli/src/tui/app.rs).
- Shutdown VM test pattern --
  [`tests/module/systemd-lifecycle.py:420-462`](../../tests/module/systemd-lifecycle.py).
- Monitor lifecycle VM test pattern --
  [`tests/module/monitor-lifecycle.{nix,py}`](../../tests/module/monitor-lifecycle.nix).
- `braid-online.service` definition --
  [`modules/braid/storage.nix:84-101`](../../modules/braid/storage.nix).

## Cross-plan status dependency

[`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
remains `Draft` until the
[`forced-shutdown-recovery-proof.md`](./forced-shutdown-recovery-proof.md)
matrix passes. Shipping the safety-core plan does not flip the ADR. The
observability plan does not flip the ADR either.

If you think ADR 020 should be restructured so guarantees (1)
observability or (3) preflight can flip to `Active` independently of
the mid-mutation recovery guarantee, raise that in a code review or
directly in the recovery-proof plan before acting on it. The original
ADR presents all three guarantees as one contract; splitting the ADR
is a documentation-architecture decision, not a scheduling shortcut.
