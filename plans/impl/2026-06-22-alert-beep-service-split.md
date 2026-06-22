# Plan: split `braid-alert` into an ephemeral orchestrator + a hardened beep loop

## Context

A systemd hardening audit (`findings/1-systemd-unit-hardening.md`, Finding 2) flagged
`systemd.services.braid-alert` in `modules/braid/monitor.nix` as a "persistent
unconfined-root beep loop" and recommended applying the monitor's sandbox baseline
(`NoNewPrivileges`, `ProtectSystem`, `ProtectHome`, restricted address families, empty
capability set).

That recommendation is the wrong shape for **this** unit, because `braid-alert` does three
jobs with conflicting requirements in one service:

1. `modprobe pcspkr` -- a real fallback for `nixos-rebuild switch` without reboot
   (`systemd-modules-load.service` runs once at early boot and is not restarted by `switch`;
   asserted by `tests/module/braid-alert.py:42`). Needs `CAP_SYS_MODULE`.
2. The operator's `alertCommand` -- arbitrary code, run as root by design. Documented examples
   are `curl ... ntfy.sh` (needs network) and `/home/user/scripts/...` (needs `/home`). This is
   an escape hatch that **fundamentally cannot be sandboxed**: `ProtectSystem=strict` /
   `ProtectHome=true` / restricted address families each silently break real notifiers, and the
   repo's own tests (`alertCommand = "touch /root/alert-fired"`) prove it.
3. An exponential-backoff `while true; beep; sleep` loop -- the only **persistent** process, and
   the only part with no untrusted input. The beep already drops privileges via
   `setpriv --reuid=nobody --regid=beep --groups=beep` inside the shared `braid-beep-probe` wrapper.

The true root cause is that two **opposite lifecycle and sandbox profiles** share one unit --
already visibly leaking into the code as a conditional
`serviceConfig = if beepEnabled then { Type = "simple"; } else { Type = "oneshot"; RemainAfterExit = true; }`.
The orchestration wants oneshot+RAE; the loop wants `Type=simple`.

**The pivot:** separate them. An ephemeral, intentionally-unconfined `braid-alert.service`
(modprobe + alertCommand) and a persistent, fully-hardened `braid-beep.service` (the loop),
lifecycle-bound. This **collapses the existing `if beepEnabled` conditional into one shape**
(braid-alert becomes unconditionally oneshot+RAE), so it is a net simplification of the
orchestrator *and* lets the persistent surface finally carry the same sandbox the project
already ships on `hddfancontrol-braid` (`modules/braid/fan-control.nix`). It fixes the root
cause instead of papering over it; the alternatives (conditional sandbox, doc-only, run-as-nobody)
each leave the conflation in place -- see "Alternatives considered."

Intended outcome: the days-long persistent root process becomes a tightly-sandboxed loop whose
only capabilities are the two `setpriv` needs; the unconfined surface shrinks to an ephemeral
oneshot that runs operator code once and exits.

## Design

Two units in `modules/braid/monitor.nix`, wired with `BindsTo` -- adapting the `BindsTo` + `After`
precedent ADR 018 documents for the scrub units, but **dropping `After=`** (rationale below: the alarm
must not be ordered behind the orchestrator's `alertCommand`).

### `braid-alert.service` (orchestrator -- intentionally unconfined)

- **Unconditionally** `Type = "oneshot"; RemainAfterExit = true;` (collapses today's `if beepEnabled`
  fork). Latches active so `braid ack` has something to stop.
- Script: `modprobe pcspkr` (beep-enabled only) then the operator's `alertCommand` (if set), emitted from
  a **shared `let` binding** `wrappedAlertCommand = lib.optionalString (cfg.monitor.alertCommand != null)
  "${pkgs.coreutils}/bin/timeout -k 5s ${toString cfg.monitor.alertCommandTimeoutSec}s ${pkgs.runtimeShell} -c ${lib.escapeShellArg cfg.monitor.alertCommand} || true"`.
  The binding is factored (not inlined) so the sibling `braid-alert-advisory.service` reuses the *identical*
  bound command and the two cannot drift -- see its subsection below.
  Interpreter and store paths are explicit, not bare `sh`/`timeout`: today `alertCommand` is interpolated
  raw into the unit `script`, which NixOS renders under `pkgs.runtimeShell` (bash), so a bare `sh -c` would
  switch the interpreter to an unspecified -- possibly absent -- `sh` and silently break any notifier using
  bashisms (`[[ ]]`, arrays). `${pkgs.runtimeShell} -c` + `lib.escapeShellArg` preserves the exact
  interpreter and quoting, and `${pkgs.coreutils}/bin/timeout` keeps store-path purity -- matching the
  in-repo bounded-shell-wrap precedent in `modules/braid/auto-suspend.nix`
  (`${pkgs.coreutils}/bin/timeout -k 2 10 ${pkgs.bash}/bin/bash -c ...`) and the `runtimeShell` use in
  `modules/braid/wrapper.nix`. The bound is a module option `braid.monitor.alertCommandTimeoutSec`
  (`lib.types.ints.positive`, default 60), not a hardcoded constant: a fixed SIGKILL deadline on arbitrary
  operator code is a sharp edge for the rare legitimately-long notifier, the option lets operators tune it,
  and `ints.positive` forbids the `timeout 0` ("disable the timeout") footgun that would re-open the wedge
  this wrap exists to close. No beep loop. The wrap matters because `Type=oneshot`
  has **no start timeout by default** (`reference/systemd/man/systemd.service.xml` `TimeoutStartSec`:
  "the timeout is disabled by default ... when `Type=oneshot` is used"), so an unbounded hung notifier
  (e.g. `curl` to an unreachable host with no `--max-time` -- plausible precisely when a disk alert
  coincides with network trouble) would otherwise wedge braid-alert in `activating` forever and the
  `active (exited)` latch that `braid ack` stops would never form. Setting `TimeoutStartSec=<finite>`
  instead is wrong: it would put braid-alert in `failed`, which `BindsTo` propagates to the beep -- the
  wrap makes the oneshot *succeed* fast instead. The trailing `|| true` keeps a non-zero/timed-out
  notifier from failing the oneshot.
- `wants = lib.optional beepEnabled "braid-beep.service";` to pull in the beep unit on activation.
- **No sandbox.** It runs arbitrary operator code as root and needs `CAP_SYS_MODULE` for the
  modprobe fallback. Document the intent in ADR 018 so the next audit reads it as deliberate.

### `braid-alert-advisory.service` (Warning/exit-3 tier -- intentionally unconfined)

Pre-existing sibling oneshot (`Type = "oneshot"; RemainAfterExit = true;`, `monitor.nix`
`systemd.services.braid-alert-advisory`) that runs **only** the operator `alertCommand` -- no beep, no
`braid-beep`, no `BindsTo` -- for the non-Critical Warning tier (`braid monitor` exit 3, e.g. proactive
ENOSPC risk, routed by `braid-monitor`'s `rc -eq 3` branch). The split plan must touch it too, because it
shares the exact exposure this plan closes:

- **Reuse the shared `wrappedAlertCommand` binding** -- the whole reason the binding is factored. Today the
  advisory runs `${cfg.monitor.alertCommand} || true` **raw and unbounded**, so (by this plan's own
  oneshot-has-no-default-start-timeout reasoning) a hung notifier wedges it in `activating` forever. Its
  blast radius is *wider* than braid-alert's: `braid-monitor` starts it with a **blocking** `systemctl start
  braid-alert-advisory.service` (no `--no-block` -- the `2>/dev/null || true` suppresses stderr but does not
  detach), so a hung advisory notifier wedges the timer-driven `braid-monitor` oneshot itself, and because
  the timer will not re-trigger a unit still stuck `activating`, **all subsequent health monitoring halts.**
  Bounding it via the shared binding dissolves that. Here the wrap's job is "do not wedge the monitor" --
  there is no beep to silence and no `BindsTo` to fail-propagate, so the latch/`|| true` reasoning is
  simpler; the unit keeps its existing trailing `exit 0`.
- **No sandbox**, same rationale as braid-alert: it runs arbitrary operator code as root, so it is
  intentionally unconfined. Only the `wrappedAlertCommand` reuse changes; its lifecycle and the exit-3
  routing are untouched.

### `braid-beep.service` (`lib.mkIf beepEnabled` -- hardened, persistent)

- `Type = "simple";` running the existing exponential-backoff loop (`delay=5`, `max_delay=900`,
  doubling) calling `${braidBeepProbe}/bin/braid-beep-probe`.
- `bindsTo = [ "braid-alert.service" ];` -- **without `after =`**. `BindsTo` alone still cascades
  teardown: when `braid ack` runs `systemctl stop braid-alert.service`, the explicit stop propagates to
  braid-beep (`reference/systemd/man/systemd.unit.xml` `BindsTo`: stops the configuring unit "when a
  listed unit is explicitly stopped"; stop-propagation does not require ordering), so **no Rust change**.
  **Omitting `After=` is deliberate and the core fix for the alertCommand-coupling finding:** with
  `After=braid-alert`, the beep would be ordered behind the orchestrator's *entire* oneshot (modprobe
  **and** the alertCommand), so a slow notifier delays the alarm and a hung one (oneshot never completes)
  withholds it indefinitely. Dropped, braid-beep is pulled in by `Wants=` and starts **in parallel** with
  braid-alert, so the beep fires immediately regardless of alertCommand. The only thing `After=` bought
  (beep waits for modprobe) is already absorbed by the loop's `braid-beep-probe 2>/dev/null || true`: a
  first probe before pcspkr is loaded is a harmless no-op that self-heals on the next 5s iteration (and on
  most boots pcspkr is already loaded -- the orchestrator modprobe is only the switch-without-reboot
  fallback). Deliberate deviation from the scrub units' `BindsTo + After` precedent, which needs the
  ordering because scrub must not start before the pool is online; the alarm has the opposite requirement.
- `serviceConfig.Restart = "always"; serviceConfig.RestartSec = 5;` plus the start-limiter disabled via
  the **type-checked NixOS top-level option `systemd.services.braid-beep.startLimitIntervalSec = 0;`** --
  **required, and the sole self-heal after the split.** Placement is a real trap here: `Restart`/`RestartSec`
  are `[Service]` keys, but `StartLimitIntervalSec` is a `[Unit]` key (`reference/systemd/man/systemd.unit.xml`
  documents it under `[Unit]`; it appears in `systemd.service.xml` only as a cross-reference), so writing it
  into `serviceConfig` would make systemd silently ignore it and leave the default limiter in place. The
  top-level option is chosen precisely to make that misplacement *unreachable* -- it carries no section for
  the author to get wrong (and is int-typed), unlike a raw `unitConfig.StartLimitIntervalSec = 0`, which
  renders byte-identical `[Unit]` output but still requires the author to know the section. `BindsTo` is directional: if the loop
  dies independently (signal, OOM, evdev node blip), braid-alert stays `active (exited)`, leaving a "live"
  alert that is silent. `Restart` self-heals it. The recovery model **changes** here: today the single
  unit is revived by the monitor's per-cycle `systemctl start braid-alert.service`; after the split
  braid-alert is `active (exited)` (RAE), so that per-cycle start no-ops and never re-pulls a dead
  braid-beep -- `Restart=always` is now the *only* thing that brings the beep back. `StartLimitIntervalSec = 0`
  disables the start-rate limiter so the alarm can **never** latch `failed`-and-silent. (Honest scope:
  with `RestartSec = 5` the default 5-starts-per-10s limiter is already practically unreachable --
  restarts are >=5s apart, per `reference/systemd/man/systemd.unit.xml` `StartLimitIntervalSec=` -- so
  this is explicitness + future-proofing against a smaller `RestartSec`, not a reachable bug today.)
  Otherwise mirrors `hddfancontrol-braid` (`fan-control.nix`); the disabled limiter is a deliberate
  third deviation, justified by the never-silent requirement. An explicit/cascaded stop (ack) is not a
  restart trigger, so this does not fight teardown -- guarded by the cascade test below.
- Hardening block, sharing `hddfancontrol-braid`'s baseline (`NoNewPrivileges`, `ProtectSystem=strict`,
  `SystemCallArchitectures=native`, `MemoryDenyWriteExecute`) but **not** a verbatim clone -- it adds a
  broader `Protect*`/`Restrict*` set and omits the fan controller's realtime `CPUScheduling*`. The two
  deviations that need *justifying* (the rest is cheap additive hardening):
  - `CapabilityBoundingSet = [ "CAP_SETUID" "CAP_SETGID" ];` -- **not empty.** Verified against
    vendored `reference/util-linux/sys-utils/setpriv.c`: the wrapper calls `setresuid`/`setresgid`/
    `setgroups`, which need exactly these two caps. Empty caps would break the beep;
    `CAP_SYS_MODULE` is absent because modprobe lives in the orchestrator.
  - **No `PrivateDevices`** (verified against `reference/systemd/.../namespace.c`: its devnode
    whitelist excludes `/dev/input/*`; enabling it hides the PC Speaker evdev node and kills the beep).
    **No `ReadWritePaths`** (the loop writes nothing). `ProtectSystem = "strict"` with no RW paths is fine.
  - `NoNewPrivileges = true;` is compatible -- it blocks privilege *gain*, not the setpriv *drop*.
  - Include the cheap rest: `ProtectHome`, `PrivateTmp`, `PrivateNetwork` (loop makes no network
    calls), `ProtectKernelModules`, `ProtectKernelTunables`, `ProtectControlGroups`, `ProtectClock`,
    `RestrictNamespaces`, `RestrictRealtime`, `LockPersonality`, `MemoryDenyWriteExecute`,
    `SystemCallArchitectures = "native"`.

### Invariants to preserve (do not touch)

- `braidBeepProbe` derivation and the `environment.etc."braid/notifier-config.json"` contract stay
  byte-for-byte unchanged. `braid doctor` runs the wrapper as root from its own context and relies
  on the setpriv inside it -- the sandbox lives on the **unit**, never folded into the wrapper.
- `cli/src/ack.rs` `stop_beeper()` keeps stopping `braid-alert.service` by name; braid-beep is torn
  down by the `BindsTo` cascade on that explicit stop -- which holds **without** `After=` (stop
  propagation does not require ordering), so no Rust change. Guarded by a new cascade test (see below).
- The trailing `|| true` on the (timeout-wrapped) `alertCommand` is **load-bearing**: it keeps
  braid-alert's oneshot from ever entering `failed`, which `BindsTo` would otherwise propagate to
  braid-beep and silence the alarm. A failing or timed-out notifier must not take the beep down with it.
- Both `braid-alert.service` and `braid-alert-advisory.service` emit the operator command **only** through
  the shared `wrappedAlertCommand` binding -- never an inline `${cfg.monitor.alertCommand}`. Re-inlining
  either re-opens the hung-notifier wedge for that tier (and, for the advisory, can hang `braid-monitor`'s
  blocking start). The advisory's trailing `exit 0` stays.
- `braid-alert.service` name is unchanged, so `smartdAlertScript`, `monitor.nix`'s
  `systemctl start braid-alert.service`, and ADR 018's diagram references all keep working.

## Files to modify

- `modules/braid/monitor.nix` -- split `systemd.services.braid-alert`; add
  `systemd.services.braid-beep`; collapse the `if beepEnabled` serviceConfig fork; add the
  `braid.monitor.alertCommandTimeoutSec` option (`lib.types.ints.positive`, default 60); add a shared
  `wrappedAlertCommand` `let`-binding (`${pkgs.coreutils}/bin/timeout -k 5s <that>s ${pkgs.runtimeShell} -c ${lib.escapeShellArg ...} || true`)
  and use it in **both** `braid-alert` and the existing `braid-alert-advisory` service (replacing each
  unit's raw inline `${cfg.monitor.alertCommand} || true`); disable the beep unit's start-limiter via the
  top-level `startLimitIntervalSec = 0` option (a `[Unit]` key -- never `serviceConfig`); wire `wants` +
  `bindsTo` (**no `after`**). Reuse the existing `braidBeepProbe` top-level binding for both the beep unit
  and the etc file.
- `tests/module/braid-alert.py` -- retarget structural asserts (see Test plan).
- `tests/module/braid-alert-slow-command.nix` + `.py` (**new**) and its `flake.nix` `checks`
  registration -- regression test that a hung `alertCommand` does not delay or silence the beep
  (see Test plan).
- `tests/module/braid-alert-no-beep.py` -- light touch (see Test plan).
- `tests/module/monitor-lifecycle.nix` + `monitor-lifecycle.py` -- flip the node to beep-enabled
  and assert `braid-beep.service` through the real monitor->ack chain (see Test plan).
- `docs/design/decisions/018-systemd-lifecycle.md` -- additive (see Docs).
- `docs/commands/monitor.md`, `docs/commands/ack.md`, `docs/guides/monitoring-and-alerts.md` --
  re-attribute the beeper from `braid-alert.service` to `braid-beep.service` (see Docs).
- Reference only (copy directives, do not edit): `modules/braid/fan-control.nix`.

## Test plan (TDD: write/adjust failing tests first, confirm the right failure, then implement)

- **`braid-alert.py` -- rewrite structural asserts.** They currently read braid-alert's `ExecStart`
  and assert modprobe + the backoff loop (`delay=5`, `max_delay=900`, `delay * 2`, `$max_delay`) +
  the `braid-beep-probe` wrapper all live there (lines 38-66). After the split: keep the
  modprobe/pcspkr assertion on `braid-alert.service`; move the loop + `braid-beep-probe` + wrapper-body
  (`setpriv`/`reuid=nobody`/`regid=beep`) assertions onto `braid-beep.service`.
- **`braid-alert.py` -- add new subtests** (its node is beep-enabled, so `braid-beep.service` exists):
  - *Hardening landed (guards the directives from silent removal):*
    `systemctl show -p NoNewPrivileges -p ProtectSystem -p CapabilityBoundingSet -p PrivateDevices
    -p StartLimitIntervalUSec -p SystemCallFilter braid-beep.service` and assert NNP=yes,
    ProtectSystem=strict, cap set = CAP_SETUID+CAP_SETGID, PrivateDevices=no, StartLimitIntervalUSec=0
    (the never-give-up directive landed -- catches the limiter option being reverted or otherwise not
    rendering, in which case `show` reports the nonzero default), and
    SystemCallFilter **empty** (no syscall filter -- this is
    the load-bearing precondition for the replay subtest's scoping; if someone later adds one that
    intercepts `setgroups`, this trips). Assert braid-alert.service does **not** carry the sandbox.
  - *Privilege drop works under the sandbox (guards the directives from being too tight):* the loop
    swallows probe failures (`braid-beep-probe 2>/dev/null || true`, `monitor.nix:111`), so braid-beep
    stays active even if the trimmed cap set is insufficient for the `setpriv` drop -- a property-only
    assertion cannot catch that, and the existing `setpriv ... id` check (braid-alert.py:71) runs
    **unsandboxed** (full root caps). Replay the drop under the unit's *actual* caps: read
    `systemctl show -p CapabilityBoundingSet --value braid-beep.service`, then
    `systemd-run --pipe --wait -p CapabilityBoundingSet='<those caps>' -p NoNewPrivileges=yes
    setpriv --reuid=nobody --regid=beep --groups=beep -- id -gn` and assert the output is `beep`
    (exit 0). Reading the caps from the unit keeps the test drift-free; if setpriv's pre-drop cap
    handling needs more than `CAP_SETUID`/`CAP_SETGID`, this fails loudly instead of silencing the beep.
    *Scope (stated, not assumed):* this replay reconstructs only `CapabilityBoundingSet` +
    `NoNewPrivileges` because those are the **only** directives in the hardening set that can intercept
    the `setresuid`/`setresgid`/`setgroups` drop. The rest (`ProtectSystem`, `ProtectHome`,
    `PrivateTmp`, the `Protect*`/`Restrict*` family, `MemoryDenyWriteExecute`) are filesystem/namespace
    protections setpriv does not exercise, and **no `SystemCallFilter=` is added** -- a precondition the
    Hardening-landed subtest now asserts, so the narrow scope is an enforced invariant rather than an
    apparent gap.
  - *Lifecycle cascade (behavioral):* `systemctl start braid-alert.service`, then
    `wait_until_succeeds("systemctl is-active braid-beep.service")` -- **not** an immediate `is-active`,
    because braid-beep is pulled by `Wants=` and (with `After=` dropped) starts in parallel, so it can
    still be `activating` when the start command returns. Then `systemctl stop braid-alert.service` and
    `wait_until_succeeds("! systemctl is-active braid-beep.service")` (the `BindsTo` cascade is async
    too). Proves the `Wants` pull-in + `BindsTo` teardown contract.
  - *Restart self-heal (behavioral -- guards the `Restart=always` claim):* start braid-alert, capture
    braid-beep's `MainPID`, `systemctl kill --signal=KILL --kill-whom=main braid-beep.service`, wait
    until braid-beep is active again with a **different** `MainPID` (proves restart), then
    `systemctl stop braid-alert.service` and assert braid-beep is inactive (proves a cascaded stop
    still wins over `Restart`). Without this, a reverted/broken `Restart` passes every other subtest
    while a dying loop silently goes quiet.
- **`braid-alert-slow-command.{nix,py}` (new) -- the beep is not hostage to `alertCommand`.** Node:
  beep-enabled + `alertCommand` that hangs (`sleep infinity`) + `braid.monitor.alertCommandTimeoutSec = 3`
  (small, so the latch-forms assertion resolves in ~3s instead of the 60s default). The point the other
  tests miss: a slow/hung notifier must neither delay nor silence the alarm. **Start braid-alert
  non-blocking** -- `systemctl start --no-block braid-alert.service` -- because a *blocking* `systemctl
  start` on a `Type=oneshot` does not return until ExecStart finishes (~`alertCommandTimeoutSec` + the 5s
  kill grace), which would wedge the test thread for the whole bound and let the parallel-beep property be
  observed only after the orchestrator already exited. Then assert
  `wait_until_succeeds("systemctl is-active braid-beep.service")` *first* (the beep fires while braid-alert
  is still in its hung `alertCommand` -- proves `After=` is dropped and the beep starts in parallel), and
  *after* that, `wait_until_succeeds(..., timeout = 30)` that braid-alert reaches `active`/exited (proves
  the `timeout` wrap forms the latch instead of wedging in `activating`). The explicit **`timeout = 30` is
  load-bearing for the option, not decoration:** with `alertCommandTimeoutSec = 3` the latch forms in ~3-4s
  (the `sleep infinity` dies on the `timeout` SIGTERM), so a 30s bound clears the real case by >7x while
  sitting ~2x under the 60s default -- a regression that accepts the option but silently ignores it (falling
  back to 60s, or dropping the wrap so the hung command never returns at all) overshoots 30s and trips,
  whereas the framework's default 900s `wait_until_succeeds` would let it pass slow-but-green. It is a
  *coarse upper bound*, not a fragile exact-3s timing assertion (which would be structure-sensitive and is
  rightly avoided). Without this bullet, reintroducing `After=` or dropping the `timeout` wrap regresses
  silently. Register in `flake.nix` `checks`.
- **Advisory tier (`braid-alert-advisory.service`) -- covered by construction + existing end-to-end.**
  Because both units consume the single `wrappedAlertCommand` binding, the `braid-alert-slow-command` test
  above exercises the *identical* bounded construction the advisory uses -- so no separate advisory-hang
  test is needed; the bound cannot be present on one unit and absent on the other without editing the shared
  binding. Put that coverage argument on the record, and pin it with one cheap structural assert in the
  slow-command `.py` (its node has `alertCommand` set): `systemctl cat braid-alert-advisory.service`
  contains the same `timeout -k 5s` wrap, so a future re-inline of the advisory's raw command trips. The
  exit-3 routing + advisory lifecycle stay covered end-to-end by the existing
  `tests/cli/braid-monitor-enospc.nix` (beep=false, `alertCommand = "touch /root/alert-fired"`); its fast
  notifier completes well within the 60s bound, so that test passes unchanged -- re-run to confirm.
- **`braid-alert-no-beep.py` -- verify, expect pass unchanged.** With `beep=false`, `braid-beep.service`
  is absent (`mkIf beepEnabled`) and `braid-alert.service` is oneshot+RAE running only `alertCommand`.
  Its existing asserts (no modprobe/pcspkr/setpriv/beep; alertCommand as root; latches active;
  stoppable) all still hold. Add a `machine.fail("systemctl cat braid-beep.service")` to pin the
  unit's absence.
- **`alert-state-lock.py`, `smartd-hook.py`, `scrub-alert.{nix,py}` -- expect pass unchanged,
  re-run to confirm.** `alert-state-lock`/`smartd-hook` only check `braid-alert.service`
  active/inactive and the hook's literal unit name. `scrub-alert` needs no edit but is a live
  regression guard for *this* change: its `fail` node is beep-enabled (default) with
  `alertCommand = "touch /root/alert-fired"` (`scrub-alert.nix:43`) and drives the Critical
  scrub-failure path end-to-end -- `braid-alert.service` goes active, `/root/alert-fired` appears,
  `braid ack` tears it all down -- and its "monitor latches ScrubFailed at Critical (beeper, not
  advisory)" subtest asserts the Critical latch routes to `braid-alert.service` and **fails**
  `braid-alert-advisory.service` (`scrub-alert.py`, the `fail:` subtests). Every assertion is
  behavioral (`is-active`/`Result`/`test -f`/JSON-latch `grep`) and none reads
  `braid-alert.service`'s `ExecStart` or otherwise couples to *where* the beep loop lives, so the
  split passes it unchanged (`is-active` reports `active` for both today's `Type=simple` loop and
  the post-split `active (exited)` oneshot). A mis-wired Critical-vs-advisory routing or a broken
  `wrappedAlertCommand` (the `touch` never fires, so `/root/alert-fired` never appears) trips it
  loudly -- so it guards the Critical-tier routing **and** the bounded `alertCommand` *running a
  normal command to completion* (the complement of `braid-alert-slow-command`'s *bounding a hung
  one*). It also runs through the new `Wants` pull-in and the `BindsTo` ack-teardown, but does not
  assert `braid-beep` directly -- that machinery is guarded by `braid-alert.py`'s cascade/restart
  subtests, not here.
- **`monitor-lifecycle` -- flip to beep-enabled and assert the beeper end-to-end.** The node sets
  `monitor.beep = false` (`monitor-lifecycle.nix:41`), so `braid-beep.service` would be **absent** --
  a `braid-beep` is-active check there would fail for the wrong reason. Change the node to beep-enabled
  (drop the `monitor.beep = false` override; default is true) and, in the `.py`, assert
  `braid-beep.service` is **active** after the degraded monitor trigger via
  `wait_until_succeeds("systemctl is-active braid-beep.service")` (it is `Wants`-pulled, so an immediate
  check races -- alongside the existing `braid-alert` active check) and **inactive** after `braid ack`
  via `wait_until_succeeds("! systemctl is-active braid-beep.service")`. The beep=false plumbing stays
  covered by `braid-alert-no-beep.py`, so no integration coverage is lost.
- **`braid-smartd-alert.{nix,py}`, `braid-ack-cleanup-pending.{nix,py}` -- expect pass unchanged,
  re-run to confirm.** Both are registered `nixosTest` VM checks (`tests/cli/`, `flake.nix`), **not**
  Rust tests: each asserts `braid ack` emits the literal `warning: systemctl stop
  braid-alert.service` on stderr from `stop_beeper()` when the unit is absent
  (`braid-smartd-alert.py:76`, `braid-ack-cleanup-pending.py:71-72`). They pass unchanged because
  `ack.rs` is untouched (the warning still fires) and the `braid-alert.service` *name* is preserved
  (the asserted string stays byte-identical), and because braid-alert.service is **not installed on
  their nodes**, the split's oneshot/`Wants`/`BindsTo` lifecycle never reaches them. (Moved here from
  the old Rust bullet, which mis-routed them through `just test-rust`.)
- **Rust (`just test-rust` = `cargo test` only):** no `ack.rs` change, so its `stop_beeper` unit
  tests pass unchanged under `cargo test` (Verification step 1). `just test-rust` builds **no**
  `nixosTest`, so the two warning-message VM checks above are confirmed in the VM lane, **not** by
  `just test-rust` -- correcting a prior false-green where running `test-rust` looked like it covered
  them.

## Docs / ADR

- `docs/design/decisions/018-systemd-lifecycle.md`: extend the `### braid-alert.service` section to
  describe the orchestrator/beep split and the **unconfined-because-alertCommand** rationale; add the
  `braid-beep.service` box to the units diagram (`braid-monitor.service -> braid-alert.service ->
  braid-beep.service`, edge labelled `BindsTo` only -- **no `After=`**); document why braid-beep
  deviates from the scrub units' `BindsTo + After` precedent (the `After=` is dropped so the alarm is
  never ordered behind -- and thus never delayed or withheld by -- the orchestrator's `alertCommand`;
  the loop's `|| true` absorbs the modprobe race) and the bounded `timeout` wrap on `alertCommand` (a
  `Type=oneshot` has no default start timeout, so an unbounded notifier would otherwise wedge the
  orchestrator in `activating` and the `active (exited)` latch would never form; the bound is the
  `braid.monitor.alertCommandTimeoutSec` option, default 60s). Cover the exit-3 Warning branch too --
  `braid-monitor -> braid-alert-advisory.service` (oneshot, no beep, no `BindsTo`), which shares the same
  `wrappedAlertCommand` bound, so the bounded-notifier guarantee holds for **both** alert tiers; reflect it
  in the units diagram and the exit-code table. Note also that
  `braid-beep.service` needs **no explicit sleep/shutdown edges beyond the normal service defaults**:
  `DefaultDependencies=yes` already gives every normal service `Conflicts=`/`Before=shutdown.target`
  (per `reference/systemd/man/systemd.service.xml` automatic-dependencies, and mirrored by the existing
  `braid-scrub` comment in `modules/braid/storage.nix`), so those are inherited, not omitted. Unlike the
  scrub units -- which add explicit `sleep.target` ordering because they own the pool/LUKS resource --
  braid-beep owns no such resource, so it declares no ordering of its own.
- **User-facing docs re-attribute the beeper** -- they currently call `braid-alert.service` "the
  beeper", which becomes wrong once the loop moves to `braid-beep.service`:
  - `docs/commands/monitor.md` -- the alert-pipeline diagram (`braid-alert.service (beeper + alertCommand)`,
    lines ~60-62) and the "held by `braid-alert.service` itself ... the backoff beep loop when beep is
    enabled, or a `RemainAfterExit` oneshot when it's off" paragraph (line ~69): split into
    `braid-alert.service` (latched root orchestrator -- modprobe + alertCommand, now *always*
    RemainAfterExit) and `braid-beep.service` (the persistent backoff beep loop); note `braid ack`'s
    `systemctl stop braid-alert.service` silences the beep **via the `BindsTo` cascade** (line ~73).
  - `docs/commands/ack.md` -- "Stops `braid-alert.service` (the beeper)" (line ~51) becomes "stops
    `braid-alert.service`, which cascades (`BindsTo`) to stop the `braid-beep.service` beep loop."
  - `docs/guides/monitoring-and-alerts.md` -- the "How the pieces fit together" diagram (lines ~150-168:
    add the `braid-beep.service` box under `braid-alert.service`) and the `journalctl -u
    braid-alert.service` beep-log tip (line ~141: the beep loop now logs under `braid-beep.service`).
- alertCommand still "runs as root" -- that text stays accurate; add a sentence to the
  `monitoring-and-alerts.md` alertCommand section noting it now runs under a bounded `timeout`
  (default 60s, tunable via the new `braid.monitor.alertCommandTimeoutSec` option) on **both** the Critical
  (`braid-alert.service`) and Warning (`braid-alert-advisory.service`) paths, so a hung notifier can no
  longer stall the alert latch -- nor wedge the timer-driven `braid-monitor` on the Warning path -- and
  document that option alongside `alertCommand` in the monitor option reference.
- ASCII-only echo/string check still passes (no new user-facing strings).
- `README.md` checked (per the AGENTS.md README-sync rule): it describes the beeper only generically
  ("beep the PC speaker until acknowledged"; the docs-TOC "beeper" row) and never names
  `braid-alert.service`, so **no change** -- recorded here so the omission reads as considered, not missed.

## Verification

1. `just test-rust` -- ack/cmd unit tests unaffected.
2. VM checks (the authoritative pass; `systemd-analyze security` is unavailable on the macOS host):
   `braid-alert`, `braid-alert-no-beep`, `braid-alert-slow-command`, `alert-state-lock`,
   `monitor-lifecycle`, `smartd-hook`, `scrub-alert`, `braid-monitor-enospc`, `braid-smartd-alert`,
   `braid-ack-cleanup-pending`. The last four run no changed assertions but each touches the modified
   unit, so each belongs in the authoritative pass, not merely "re-run if convenient": `scrub-alert`
   exercises the Critical-tier routing (`braid-alert.service`, not the advisory) plus the bounded
   `alertCommand` running to completion; `braid-monitor-enospc` the exit-3 advisory lifecycle through
   the shared `wrappedAlertCommand`; `braid-smartd-alert` and `braid-ack-cleanup-pending` the
   `stop_beeper` `warning: systemctl stop braid-alert.service` stderr string (preserved because the
   unit name does not change). All are registered `nixosTest` checks in `flake.nix`; **none is
   runnable via `just test-rust`** -- step 1 covers only the `cargo test` lane.
3. Confirm the new `braid-alert.py` subtests fail before the module change and pass after.
4. `just docs-build` -- linkcheck for the ADR 018 edit.

## Alternatives considered (rejected)

- **Conditional sandbox when `alertCommand == null`** -- backwards: it disables hardening for exactly
  the security-conscious operators who set an ntfy/email `alertCommand`, and adds a third conditional
  shape to the unit.
- **Doc-only floor** (keep one unit, drop the unsupportable directives, ADR-note it as intentionally
  unconfined) -- the honest fallback, but it leaves the loop unsandboxed and the dual-lifecycle `if`
  intact; inconsistent with a project that hardens a fan controller. This is the floor if the
  `BindsTo` wiring is ever judged too risky -- not the target.
- **Run the loop as `User=nobody`** (no setpriv in the unit) -- a trap: the shared `braid-beep-probe`
  wrapper must keep `setpriv` for `braid doctor`'s root-context invocation, and `setpriv --groups=beep`
  calls `setgroups` (needs `CAP_SETGID`) even when already `nobody:beep` -- forcing `AmbientCapabilities`
  back onto the "unprivileged" unit or forking the wrapper (breaking the single-source-of-truth
  invariant). Messier than the split, not cleaner.
- **Decoupling the beep from `alertCommand` -- three rejected shapes** (the chosen fix is drop `After=`
  + `timeout`-wrap the notifier, keep `BindsTo`):
  - *`timeout`-wrap but keep `After=braid-alert`* -- bounds the orchestrator, but the beep is still
    ordered behind the wrapped oneshot, so a slow notifier delays the first beep by up to the wrap N.
    Dropping `After=` makes the beep fire immediately at no extra cost, so keeping it is strictly worse.
  - *Full decouple: drop `BindsTo` too and make `ack.rs` `stop_beeper()` explicitly stop
    `braid-beep.service`* -- unnecessary: `BindsTo` **without** `After=` already propagates ack's explicit
    `systemctl stop braid-alert` to the beep (`reference/systemd/man/systemd.unit.xml` `BindsTo`), so the
    declarative cascade survives and the Rust change buys nothing. (Still available as the
    defense-in-depth note below.)
  - *`TimeoutStartSec=<finite>` on braid-alert instead of wrapping* -- wrong direction: it drives the
    orchestrator to `failed`, which `BindsTo` propagates to the beep (silencing it) and which is not the
    `active (exited)` latch ack expects. The wrap makes the oneshot *succeed* quickly instead.

## Notes / optional

- **Skip `DeviceAllow=char-input rw`.** It is viable (the PC Speaker evdev is char-major-13 "input"),
  but it allow-lists *all* evdev nodes, and the real authorization is already the udev rule
  (`GROUP="beep" MODE="0620"` on `ATTRS{name}=="PC Speaker"`) plus the `nobody:beep` drop. An
  unasserted `DeviceAllow` line rots; omit unless someone wants to assert it.
- **Optional `ack.rs` defense-in-depth:** explicitly `systemctl stop braid-beep.service` in
  `stop_beeper()` to match `braid-online`'s explicit-`BoundBy`-teardown style in ADR 018, removing
  ack's implicit dependency on the unit topology. Costs a Rust change + updates to the `stop_beeper`
  unit tests in `ack.rs`. The cascade test makes this optional; default is to rely on the
  `BindsTo`-without-`After=` stop-propagation, which the cascade test exercises directly.

## Implementation notes

- Preserved the existing `braid-pcspkr-load.service` split instead of moving `modprobe pcspkr` back into `braid-alert.service`; the current repo already isolates module loading in a hardened rerunnable oneshot, so `braid-alert.service` only waits for that loader and pulls `braid-beep.service` in parallel.
- Retargeted the pre-existing `tests/module/braid-alert-hardened.py` VM check to `braid-beep.service`; that check was added after the plan was written and now guards the hardened persistent loop.
- Updated `docs/design/decisions/033-systemd-unit-hardening.md` alongside ADR 018 because the split changes the authoritative hardening profile table and removes the old conditional light alert profile.
