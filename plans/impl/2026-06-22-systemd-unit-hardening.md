# Plan: systemd unit hardening via a shared base profile

## Context

A security audit (`findings/1-systemd-unit-hardening.md`) found that four
braid-defined systemd units run as **fully unconfined root** with zero
`serviceConfig` sandboxing: `braid-monitor`, `braid-alert`,
`braid-ups-secrets`, and `braid-seal-mountpoint`. The only hardened unit,
`hddfancontrol-braid` (`modules/braid/fan-control.nix`), carries a
hand-rolled 7-directive set -- proving the project already wants the pattern
but has applied it once, inconsistently.

`braid-monitor` is the sharpest case: it fires every 5 minutes
(`OnUnitActiveSec`, `modules/braid/monitor.nix`), is the only *periodic* root
unit, and parses external tool output (`btrfs device stats` /
`btrfs filesystem show`) -- a documented parser-drift surface (AGENTS.md
"Parser compatibility"). A parser bug executing as unconfined root every 5
minutes unattended is the worst-case exposure.

Verification established two load-bearing facts and one trap:

1. **Monitor keeps `CAP_SYS_ADMIN` only for device-mapper status.** The btrfs
   ioctls it uses need no capabilities: `BTRFS_IOC_GET_DEV_STATS` gates
   `CAP_SYS_ADMIN` *only* with the `RESET` flag
   (`reference/linux/fs/btrfs/ioctl.c:3235`), and `FS_INFO`/`DEV_INFO` have no
   capability gate (`reference/linux/fs/btrfs/ioctl.c:2769,2823`). But
   `probe_pool_alerts` also runs `cryptsetup status` to classify live mapper
   backing, and the VM gate showed that device-mapper status fails without
   `CAP_SYS_ADMIN`. So monitor keeps that single capability.

2. **The trap:** `braid monitor` acquires `/run/braid-pool.lock` in dispatch
   *before any work* (`cli/src/main.rs` `lock_policy` -> `LockPolicy::MonitorSilent`,
   opened `O_RDWR|O_CREAT` at `cli/src/pool_lock.rs` `open_lock_file`).
   `ProtectSystem=strict` makes `/run` read-only, so the finding's proposed
   `ReadWritePaths=[/var/lib/braid]` would make every monitor cycle fail with
   `EROFS` -> exit 2 -> the wrapper logs but exits 0 -> **monitoring silently
   stops while the timer looks healthy.** The sandbox must also grant
   `/run/braid-pool.lock`.

This plan hardens all four units behind a single shared base, retrofits
`hddfancontrol-braid` onto it, and codifies the posture in a new ADR so the
audit does not recur. It matches braid's charter ("reach for the ideal,
robust, simple, most correct solution regardless of refactor cost") and
Principle 1 (Resilient by default).

## Decisions (confirmed with user)

- **Scope: comprehensive.** The four write-path units (`braid-monitor`,
  `braid-alert`, `braid-ups-secrets`, `braid-seal-mountpoint`) + a shared
  `hardening.nix` base + retrofit `hddfancontrol-braid` + a new ADR. Implement
  monitor-first inside this scope, each unit guarded by a VM test as it lands.
- **Scope extension (reviewer round 3 + audit Finding 6): the two dispatch
  shells.** `braid-scrub-resume-trigger` and `braid-fan-reload` were first
  filed as "not sandboxable," but Finding 6 itself classes them as cheaply
  hardenable, and `braid-scrub-resume-trigger` parses `btrfs scrub status` as
  root -- the exact parser-drift-as-root risk that justifies hardening monitor.
  Harden both rather than codify a false exception in the ADR. This grows the
  original four-unit scope by two; see the per-unit deltas and the corrected ADR
  "genuinely unhardenable" list. (Flagged as a scope change for sign-off.)
- **`braid-alert`: conditional profile.** `braid-alert` runs the
  operator-supplied `alertCommand` (arbitrary code) -- the alert path is the
  most safety-critical path and must never be silently sabotaged. So:
  - **Default (`alertCommand == null`):** the full strong base. The persistent
    beep loop is braid's own code and writes nothing, so it can be tightly
    confined.
  - **Custom command set (`alertCommand != null`):** a light process-family
    profile only, leaving filesystem/home/network open so custom notifiers
    (logfiles, webhooks, `touch /root/...`) keep working.

## Design

### 1. New shared base: `modules/braid/hardening.nix`

Mirror the `wrapper.nix` helper pattern (a plain function returning a value,
imported in each module's `let` block -- see `modules/braid/monitor.nix`
`braidWrapped = import ./wrapper.nix { inherit cfg pkgs lib; }`). `constants.nix`
holds only static scalars and is the wrong home.

```nix
# modules/braid/hardening.nix
# Shared systemd exec-sandbox baseline for braid units. The maximal set of
# directives safe for EVERY consuming unit; per-unit deltas (ReadWritePaths,
# CapabilityBoundingSet, address families, PrivateDevices) are applied at the
# call site via `base // { ... }`. See ADR 032.
{ }:
{
  base = {
    NoNewPrivileges = true;
    ProtectSystem = "strict";        # consumers MUST add their ReadWritePaths
    ProtectHome = true;
    PrivateTmp = true;
    ProtectControlGroups = true;
    ProtectKernelModules = true;
    ProtectKernelLogs = true;
    RestrictNamespaces = true;
    LockPersonality = true;
    MemoryDenyWriteExecute = true;
    SystemCallArchitectures = "native";
    RestrictSUIDSGID = true;
  };
}
```

Deliberately **not** in `base` (they are per-unit because they conflict
between consumers):
- `CapabilityBoundingSet` -- differs per unit (empty / one cap / omitted).
- `RestrictAddressFamilies` / `PrivateNetwork` -- monitor needs AF_UNIX; ups
  and seal want `PrivateNetwork`; alert must stay open.
- `PrivateDevices` -- a per-unit, dependency-proven opt-in, **never a default**
  pushed onto non-device-looking services. `alert`'s beep needs `/dev/input`,
  `hddfancontrol-braid` reads `/dev/disk/by-id` + validates block devices, and
  `braid-monitor` relies on `cryptsetup status` seeing real mapper backing
  devices (all three **omit** it); `ups-secrets` keeps it (it preserves
  `/dev/urandom`); seal adds it only after a VM test proves it opens no `/dev`
  node.
- `RestrictRealtime` -- would break `hddfancontrol-braid`'s `CPUSchedulingPolicy=rr`.
- `ProtectKernelTunables` -- test-gated for monitor (see Risks).

Consumed in each module as:
```nix
inherit (import ./hardening.nix { }) base;   # in the let block
# ...
serviceConfig = base // { Type = "oneshot"; ReadWritePaths = [ ... ]; ... };
```

### 2. Per-unit deltas

| Unit | File | ReadWritePaths | CapabilityBoundingSet | Net / Devices | Notes |
|------|------|----------------|-----------------------|---------------|-------|
| `braid-monitor` | monitor.nix | `/var/lib/braid`, `/run/braid-pool.lock` | `[CAP_SYS_ADMIN]` | `RestrictAddressFamilies=[AF_UNIX]`; **omit** `PrivateDevices` | add `After=systemd-tmpfiles-setup.service` |
| `braid-ups-secrets` | ups.nix | `/var/lib/braid` | `""` (empty) | `PrivateNetwork=true`; `PrivateDevices=true` (keeps /dev/urandom) | drop redundant `chown root:root` line |
| `braid-seal-mountpoint` | storage.nix | `cfg.mountPoint` | `[CAP_LINUX_IMMUTABLE]` | `PrivateNetwork=true`; `PrivateDevices=true`* | already has `After=systemd-tmpfiles-setup.service` |
| `braid-alert` (default) | monitor.nix | (none -- writes nothing) | omit (setpriv needs SETUID/SETGID) | **no** `PrivateDevices` (beep needs /dev/input) | strong = `base`; `modprobe` moves to `braid-pcspkr-load` |
| `braid-alert` (alertCommand set) | monitor.nix | -- | -- | -- | light profile (below); `modprobe` moves to `braid-pcspkr-load` |
| `braid-pcspkr-load` (new, beep only) | monitor.nix | (none) | `[CAP_SYS_MODULE]` | `PrivateNetwork=true` | `base // { ProtectKernelModules=false; CapabilityBoundingSet=[CAP_SYS_MODULE]; }`; re-runnable oneshot (no `RemainAfterExit`); see below |
| `hddfancontrol-braid` | fan-control.nix | (none -- writes /sys, exempt) | -- | **OMIT** `PrivateDevices` -- reads `/dev/disk/by-id`, validates block devices | `base // { CPUSchedulingPolicy="rr"; CPUSchedulingPriority=49; Restart="always"; RestartSec=5; }` |
| `braid-scrub-resume-trigger` | storage.nix | (none -- reads only) | `""` (empty)** | `RestrictAddressFamilies=[AF_UNIX]` | parses `btrfs scrub status` (no lock, no writes), then `systemctl start braid-scrub` |
| `braid-fan-reload` | fan-control.nix | (none -- dispatch only) | `""` (empty) | `RestrictAddressFamilies=[AF_UNIX]` | `sleep 5` + `systemctl restart hddfancontrol-braid` |

`*` = per-unit `PrivateDevices` opt-in, added only once a VM test proves the
unit opens no `/dev` node. Not a default (see the `PrivateDevices` note above).
`hddfancontrol-braid` is the proven counter-example: hddfancontrol 2.1.1
`src/cl.rs` (`to_drive_paths`) does `read_dir("/dev/disk/by-id")` for `-d ata`
and `src/device/drive.rs` validates each path with `is_block_device()`, so
`PrivateDevices`'s minimal `/dev` would break drive discovery.

`**` = empty caps, verified against source. `btrfs scrub status` *does* issue the
`CAP_SYS_ADMIN`-gated `BTRFS_IOC_SCRUB_PROGRESS` ioctl (linux v6.x,
`fs/btrfs/ioctl.c` `btrfs_ioctl_scrub_progress`), but its `EPERM` is **swallowed**:
btrfs-progs v6.19.1 `cmds/scrub.c` (`is_scrub_running_in_kernel`) returns 1 only on
`!ret` and otherwise falls through to `return 0`, so an `-EPERM` is indistinguishable
from "no scrub running." That return feeds only the display-only `in_progress`
annotation; `cmd_scrub_status` returns `!!err`, where `err` is set **only** by the
ungated path -- `get_fs_info` (`btrfs_ioctl_fs_info`/`btrfs_ioctl_dev_info`, no
`capable()` gate) and `get_df` (`btrfs_ioctl_space_info`, no `capable()` gate). So
`btrfs scrub status --raw` **exits 0 with empty caps**, and the resume-relevant
`Status:` words come from the persisted status file, not the gated ioctl:
`_print_scrub_ss` prints `in_progress ? "running" : canceled ? "aborted" : finished
? "finished" : "interrupted"`, where `canceled`/`finished` are read from
`/var/lib/btrfs/scrub.status.<uuid>` via `scrub_read_file` (no caps). Missing caps
can only push `in_progress` toward 0, so the only possible misread is Running ->
Interrupted -> Yes (the *safe* over-resume direction), and at pool-online trigger
time no scrub is running anyway. So this unit drops all caps. It still writes nothing
(`scrub_needs_resume.rs` only reads + parses) and takes no pool lock
(`LockPolicy::None`, `main.rs`), so `ProtectSystem=strict` needs no `ReadWritePaths`
(the status-file read is satisfied by read-only mounts) and it never hits monitor's
EROFS trap.

**`braid-alert` conditional construction** (`monitor.nix`, replacing the
current `serviceConfig` if/else):
```nix
let
  # lightAlert: the operator escape hatch (alertCommand != null). CONTRACT --
  # it confines ambient authority the notifier should never need, but NEVER
  # constrains how the operator's own command runs. It is a hand-curated subset
  # of `base`, NOT derived, so this list is the recorded contract (ADR 032):
  #   Dropped from base (would break/fault an arbitrary notifier):
  #     ProtectSystem, ProtectHome, PrivateTmp, RestrictNamespaces (file writes,
  #     containers), MemoryDenyWriteExecute (JIT/interpreters), and
  #     SystemCallArchitectures=native (faults a 32-bit/non-native-ABI binary).
  #   Kept (ambient hardening that does not fault normal operator binaries):
  #     the directives below. CapabilityBoundingSet is also omitted (Finding 2:
  #     no empty set / SystemCallFilter on operator-supplied code).
  # A future directive added to `base` does NOT auto-propagate here -- if it is
  # ambient-only, add it below too.
  lightAlert = {
    NoNewPrivileges = true;
    ProtectKernelModules = true;
    ProtectControlGroups = true;
    ProtectKernelLogs = true;
    LockPersonality = true;            # locks personality; does not block 32-bit exec
    RestrictSUIDSGID = true;
  };
  alertType = if beepEnabled
    then { Type = "simple"; }
    else { Type = "oneshot"; RemainAfterExit = true; };
in
serviceConfig = alertType
  // (if cfg.monitor.alertCommand == null then base else lightAlert);
```
The strong branch (`base`) **omits** `CapabilityBoundingSet`: the beep loop's
`setpriv --reuid=nobody --regid=beep` drops root -> nobody, which requires
`CAP_SETUID`/`CAP_SETGID`; an empty bounding set would make `setpriv` fail and
the beep silently never fire (the script swallows its stderr with `2>/dev/null
|| true`). All other base directives are safe for the beep loop (`setpriv`/`beep`
are plain C; `/dev/input` is exempt under `ProtectSystem=strict` and not hidden
without `PrivateDevices`).

**New `braid-pcspkr-load.service` (created only when `beepEnabled`).** The
runtime `modprobe pcspkr` cannot stay in the alert script: `ProtectKernelModules=true`
(in both alert branches) blocks module loading, and module-load privilege must
not live in the long-lived beep loop. But it cannot simply be deleted either --
`braid-alert.py` pins it as the **no-reboot load path** (`boot.kernelModules`
only loads pcspkr at boot, so enabling beep via `nixos-rebuild switch` without a
reboot would otherwise leave audible alerting silently inert). So move it to a
dedicated minimal loader:

```nix
systemd.services.braid-pcspkr-load = lib.mkIf beepEnabled {
  description = "Load pcspkr for braid audible alerts";
  wantedBy = [ "multi-user.target" ];          # eager boot load
  serviceConfig = base // {
    Type = "oneshot";
    # No RemainAfterExit: a re-runnable oneshot. braid-alert pulls it via
    # Wants=/After= on EVERY start, so an unloaded pcspkr is reloaded at
    # alert time -- matching the original in-script `modprobe`, which ran on
    # each alert start (monitor.nix, top of the alert `script`). With
    # RemainAfterExit the unit would be active-after-exit, so the dependency
    # would count as satisfied and never re-run.
    ProtectKernelModules = false;              # it must load a module
    CapabilityBoundingSet = [ "CAP_SYS_MODULE" ];
    PrivateNetwork = true;
    ExecStart = "${pkgs.kmod}/bin/modprobe pcspkr";
  };
};
```

`braid-alert.service` gains `after = [ "braid-pcspkr-load.service" ]` +
`wants = [ "braid-pcspkr-load.service" ]` (when `beepEnabled`). Because the
loader is a re-runnable oneshot (no `RemainAfterExit`), this dependency re-runs
`modprobe` on *every* alert start -- the binding "pcspkr present at beep time"
guarantee, equivalent to the old in-script `modprobe` it replaces (which also
ran on each start). `boot.kernelModules = [ "pcspkr" ]` plus the loader's
`wantedBy = multi-user.target` give the eager boot load; the loader is a
harmless no-op when the module is already loaded and reloads it if it was
removed -- enabling beep via `nixos-rebuild switch` without a reboot, or a
manual `rmmod`.

**Dispatch-shell units (`braid-scrub-resume-trigger`, `braid-fan-reload`).**
Both are thin root shells the first draft mislabeled "not sandboxable." Neither
opens device-mapper, mounts, or writes braid state, so both take the base:
- `braid-fan-reload` (`fan-control.nix`) is `ExecStartPre=sleep 5` +
  `ExecStart=systemctl restart hddfancontrol-braid.service`. No caps, no writes:
  `base // { CapabilityBoundingSet = ""; RestrictAddressFamilies = [ "AF_UNIX" ]; }`
  (AF_UNIX for the `systemctl` call to PID1, same reasoning as monitor; the
  socket connect is authorized by peer uid, not caps, and is not an FS write).
- `braid-scrub-resume-trigger` (`storage.nix`) runs `braid scrub-needs-resume`
  then `systemctl start --no-block braid-scrub.service`. `scrub-needs-resume`
  only reads + parses `btrfs scrub status` (`scrub_needs_resume.rs`) and is
  `LockPolicy::None` -- no `/run/braid-pool.lock`, so no EROFS trap and no
  `ReadWritePaths`. `btrfs scrub status` *does* issue the `CAP_SYS_ADMIN`-gated
  scrub-progress ioctl, but swallows its `EPERM` and exits 0 without caps (see
  `**`), so empty caps are correct and match `braid-fan-reload`:
  `base // { CapabilityBoundingSet = ""; RestrictAddressFamilies = [ "AF_UNIX" ]; }`.

Confining the trigger's *own* parse is the point: it hands off to the genuinely
unhardenable `braid-scrub.service`, but the parser-drift-as-root exposure lives
in the trigger, not the handoff -- so the audit's "marginal payoff because they
hand off to unhardened units" undersells it. (`braid-fan-reload`'s payoff is
genuinely marginal -- it parses nothing -- but hardening it is near-free and
makes the ADR's "genuinely unhardenable" list actually true.)

### 3. The `/run/braid-pool.lock` fix

Add to the always-present `systemd.tmpfiles.rules` in `storage.nix` (alongside
the `/var/lib/braid` and mountpoint rules):
```nix
"f /run/braid-pool.lock 0600 root root -"
```
This pre-creates the lock file so monitor's `ReadWritePaths` entry can bind-mount
it read-write under `ProtectSystem=strict`. Add `After=systemd-tmpfiles-setup.service`
to `braid-monitor.service` (it currently has no `After`) so the file exists when
the namespace is built. braid never unlinks the lock (`open_lock_file` only
`O_CREAT`s), so the bind mount stays valid for the boot session; flock contends
correctly across the private mount namespace (same inode), preserving Principle
12. Only monitor needs this -- seal's service form takes no lock, ups/alert never
touch it.

### 4. Cleanups the hardening unlocks

- **`braid-alert`:** remove the runtime `${pkgs.kmod}/bin/modprobe pcspkr` line
  from the alert script and relocate it to `braid-pcspkr-load.service` (above) --
  `ProtectKernelModules=true` would block it in-loop, but it cannot be dropped
  outright (it is the tested no-reboot load path). Not a pure deletion.
- **`braid-ups-secrets`:** delete the redundant `chown root:root` line
  (`ups.nix`). The unit runs as root and creates the file as root, so the chown
  is a no-op; removing it lets `CapabilityBoundingSet=""` be exact.

### 5. ADR + doc updates

- **New `docs/design/decisions/032-systemd-unit-hardening.md`** (status:
  `Active`). Records: the shared base + per-unit delta model; the conditional
  `braid-alert` profile and *why* (operator escape hatch, resilient-by-default);
  the verified empty-cap reasoning (cite the kernel ioctl facts); the
  `setpriv`/`CAP_SETUID` exception; the `braid-pcspkr-load.service` split and
  why module-load privilege lives there, not in the alert loop (and why it is a
  re-runnable oneshot -- no `RemainAfterExit` -- so `braid-alert`'s `Wants=`
  reloads pcspkr on every start); the
  `/run/braid-pool.lock` pre-creation requirement; `PrivateDevices` as a
  dependency-proven per-unit opt-in with the **`hddfancontrol-braid` exception**
  (it reads `/dev/disk/by-id` and validates block devices, so `PrivateDevices`
  is incompatible); the `lightAlert` contract (exactly which `base` directives it
  drops -- the FS/home/tmp/namespace/W^X family plus `SystemCallArchitectures`,
  all of which break or fault an arbitrary operator notifier -- and which ambient
  directives it keeps; noting the source audit's own example *kept*
  `SystemCallArchitectures`, dropped here on reflection because it is the one such
  directive that faults the operator's own non-native-ABI binary); the
  cross-namespace `flock` premise (monitor holds `/run/braid-pool.lock` via a
  `ReadWritePaths` bind mount in a private mount namespace; bind mounts share the
  host inode and `/run` is not re-created the way `PrivateTmp` re-creates `/tmp`,
  so the lock still contends with host-namespace mutators -- preserving Principle
  12, and now behaviorally tested); the test-gated directives; and -- lifting the
  audit's
  Finding 6 -- the corrected sandboxability split. The genuinely **not**
  -sandboxable set is `braid-unlock`/`braid-auto-unlock`/`braid-online`/
  `braid-scrub` (they need `CAP_SYS_ADMIN` + device-mapper + mount propagation)
  plus the process-less `braid-pool` target. `braid-scrub-resume-trigger` and
  `braid-fan-reload` are moved OUT of that set and hardened -- upgrading the
  audit's "optional, low priority" on the parser-as-root consistency argument --
  both with empty caps. The ADR records *why* the trigger needs none: `btrfs
  scrub status` issues the `CAP_SYS_ADMIN`-gated scrub-progress ioctl but
  swallows its `EPERM` (`is_scrub_running_in_kernel` returns 0 on any ioctl
  error; `cmd_scrub_status` never propagates it to the `!!err` exit code), so
  the command exits 0 without caps and the resume decision is read from the
  persisted status file -- pre-empting a future auditor who spots the gated
  ioctl and re-raises the cap. This section stops future audits from re-raising
  either the now-hardened units or the truly-unhardenable ones.
- **ADR citation style.** ADR 032 cites upstream kernel/tool facts in the
  project's shape-based form per `docs/dev/reference-source.md#citing-reference-code`:
  `pkg <version>, <path> (fn name)` plus a short paraphrase, or a fenced excerpt
  tagged `c`/`text` (never `rust`, which becomes a failing doctest) -- **no line
  numbers** (they drift on `just fetch-references` and `reference/` is gitignored).
  E.g. `linux <ver>, fs/btrfs/ioctl.c (btrfs_ioctl_get_dev_stats)` -- "gates
  `CAP_SYS_ADMIN` only when the `RESET` flag is set". The `reference/...:NNNN`
  refs in this plan's Context are implementer breadcrumbs and must not be copied
  verbatim into the tracked ADR.
- **`docs/design/decisions/018-systemd-lifecycle.md`:** add a `## See` link to
  ADR 032 (follow `docs/dev/doc-citations.md#decision-doc-references`; validated
  by `scripts/docs/check-see-paths.py`).
- **`AGENTS.md` "Read before you touch":** add a line pointing systemd-unit
  hardening / sandbox directives at ADR 032.
- Reference Principle 1 (Resilient by default) in the ADR header, matching ADR
  018's style.

## Test plan (TDD: assertions first -> red -> implement -> green)

Reuse existing fixtures and the established `systemctl show -p` / `systemctl cat`
assertion patterns (`tests/module/auto-scrub.py`, `immutable-mountpoint.py`).
Behavioral assertions are the real guard; directive-presence assertions are
cheap regression tripwires for security invariants.

1. **`tests/module/monitor-lifecycle.py` (extend).** Subtests 5/7 already start
   `braid-monitor.service` and assert the alert chain fires
   (`test -f /root/alert-fired`); once monitor is hardened these become the
   **behavioral guard for the `/run` fix** (a missing `/run/braid-pool.lock`
   RW path -> exit 2 -> no alert -> failure). Add directive-presence assertions
   for `braid-monitor`: `ProtectSystem=strict`,
   `CapabilityBoundingSet=CAP_SYS_ADMIN`, `ReadWritePaths` contains both
   `/var/lib/braid` and `/run/braid-pool.lock`, `NoNewPrivileges=yes`.
   (This config has `alertCommand` set + `beep=false`,
   so it also confirms the **light** alert branch keeps `/root` writable.)
2. **`tests/module/braid-alert.py` (extend + fix).** `braid-alert.nix` sets
   `alertCommand` -> light branch. (a) Assert the kept light directives are
   present and the escape-hatch-dropped ones are **absent** --
   `ProtectSystem`/`ProtectHome`/`PrivateTmp`/`RestrictNamespaces` AND
   `SystemCallArchitectures` (guard: a custom command must get neither
   FS/namespace confinement nor a non-native-ABI fault; this is the executable
   form of the `lightAlert` contract). (b) **Rework the existing modprobe assertion**
   (`assert "modprobe" in script`, ~line 42): the alert script must no longer
   contain `modprobe`; instead assert `braid-pcspkr-load.service` exists, carries
   `CapabilityBoundingSet=CAP_SYS_MODULE`, and `braid-alert.service` is
   `After`/`Wants` it. (c) Behavioral no-reboot guard (replacing what the old
   modprobe-in-script assertion stood for), exercised through the *alert-start*
   path so it catches a non-re-runnable loader: `systemctl stop
   braid-alert.service` -> `rmmod pcspkr` (assert it is gone) -> `systemctl start
   braid-alert.service` -> `lsmod | grep pcspkr` proves the alert's
   `Wants=`/`After=` re-ran the loader before the beep loop. Going through
   `braid-alert` (not `systemctl restart braid-pcspkr-load.service` directly) is
   the point: a direct loader restart would pass even with `RemainAfterExit` set,
   masking the regression. (`braid-alert.nix` is the light+beep branch, so the
   loader exists and pcspkr is loadable -- `boot.kernelModules` proves it at
   boot.) Keep the existing system-level priv-drop check (~line 71).
3. **New `tests/module/braid-alert-hardened.{nix,py}`** (beep enabled,
   `alertCommand = null`) -> strong branch. Register in `flake.nix` `checks` next
   to the other `braid-alert*` entries. Assert:
   - `braid-alert.service` reaches active and base directives are present
     (`ProtectSystem=strict`, `ProtectHome=yes`).
   - **Invariant guard:** `systemctl show braid-alert.service -p CapabilityBoundingSet --value`
     still contains `cap_setuid` and `cap_setgid` -- an empty/restricted set would
     silently break the beep's privilege drop.
   - **Behavioral priv-drop under the sandbox** -- the service-active check is
     *not* enough, because the beep swallows `setpriv` failure with `2>/dev/null
     || true`. Run `setpriv --reuid=nobody --regid=beep --groups=beep -- id`
     inside a transient unit mirroring the strong-alert directives
     (`systemd-run --pipe --wait -p ProtectSystem=strict -p NoNewPrivileges=yes
     ...`) and assert the output shows `uid=...(nobody)` and `gid=...(beep)`.
     Comment that the `-p` list mirrors `hardening.nix` `base` and must track it.
4. **`tests/module/immutable-mountpoint.py` (extend).** The boot-seal assertion
   already exercises the hardened seal unit; add presence assertions
   (`CapabilityBoundingSet=CAP_LINUX_IMMUTABLE`, `ProtectSystem=strict`,
   `ReadWritePaths` = mountpoint) and confirm `+i` still sets under the sandbox.
5. **`tests/module/ups-credential-lifecycle.py` (extend).** Confirm
   `upsmon.pass` is still generated under the sandbox (behavioral) and add
   presence assertions (`CapabilityBoundingSet` empty, `ProtectSystem=strict`,
   `PrivateNetwork=yes`).
6. **`tests/module/fan-control.py` (extend).** It exists. Add a
   directive-presence check: base directives present (`ProtectSystem=strict`,
   `NoNewPrivileges=yes`, ...), `rr` scheduling intact (`CPUSchedulingPolicy=rr`
   not blocked, since `RestrictRealtime` is excluded from base), and
   **`PrivateDevices` absent** (`systemctl show hddfancontrol-braid.service -p
   PrivateDevices --value` = `no`) so a future tidy-up cannot add it and silently
   break `-d ata` drive discovery. **Behavioral sandbox-start guard:** every
   directive-presence read above passes even if the unit never executes --
   `fan-control.py` runs against the fake `braid-test.0` device, so the resolver
   finds 0 PWM paths and the daemon never reaches `active`, and `systemctl show`
   returns unit config regardless. Add a journal assertion that the generated
   script actually ran *inside the namespace*:
   `wait_until_succeeds("journalctl -b -u hddfancontrol-braid.service | grep -q
   'expected exactly one PWM path'")`. That diagnostic (`fan-control.nix`,
   resolver `script`) is emitted only if bash executed under the sandbox; a
   sandbox-setup failure (exit 226/NAMESPACE from a base directive that breaks
   startup) produces no such line and fails the assertion -- the one behavioral
   signal available without PWM hardware. The existing fan behavior assertions
   (plus `fan-control-hotswap.py`) remain the live-fire guard.
7. **`tests/module/monitor-lifecycle.py` (extend) -- cross-namespace lock
   contention.** The one genuinely new safety premise: monitor holds
   `/run/braid-pool.lock` via a `ReadWritePaths` bind mount inside a private
   mount namespace, while `braid add`/`remove` hold the host path; Principle 12
   rests on "same inode -> flock contends." Prove it: start a transient unit
   mirroring monitor's sandbox that holds the lock (`systemd-run
   --unit=braid-flock-probe -p ProtectSystem=strict -p
   ReadWritePaths=/run/braid-pool.lock --service-type=exec /bin/sh -c 'flock
   /run/braid-pool.lock -c "sleep 30"'`), then assert a host-namespace acquire
   FAILS (`wait_until_fails("flock -n /run/braid-pool.lock -c true")`), then stop
   the probe and assert the host CAN acquire. A bind mount not sharing the host
   inode would let both sides hold the lock -> concurrent pool mutation -> the
   assertion catches it. (ADR 032 also records the kernel guarantee as the
   documented fallback, but the behavioral test is the braid-TDD-preferred form.)
8. **`tests/module/fan-control.py` (extend) -- `braid-fan-reload`.** The existing
   `braid-fan-reload oneshot exists with debounce` subtest gains directive
   presence (`ProtectSystem=strict`, `CapabilityBoundingSet` empty,
   `RestrictAddressFamilies=AF_UNIX`). `fan-control-hotswap.py` already drives a
   real udev event -> debounced restart, now exercised under the sandbox (its
   live-fire guard doubles as the behavioral start proof).
9. **`tests/module/auto-scrub.py` (extend) -- `braid-scrub-resume-trigger`.**
   Directive presence (`ProtectSystem=strict`, `CapabilityBoundingSet` empty,
   `RestrictAddressFamilies=AF_UNIX`) + a behavioral guard that the trigger
   reaches its scrub-status decision **under the sandbox** (it succeeds and
   correctly starts or skips `braid-scrub`), proving `ProtectSystem=strict` does
   not break the trigger's read path -- the analog of monitor subtest 7. This is a
   genuine sandbox-read regression guard (a base directive that broke the parse
   would fail it); it does **not** distinguish the cap level, because `btrfs scrub
   status` swallows the scrub-progress ioctl's `EPERM` and exits 0 with either
   empty caps or `CAP_SYS_ADMIN` (see the `**` footnote) -- empty caps are the
   verified-correct setting, not something this test could falsify.
   `auto-scrub.py`/`scrub-lifecycle.py` already exercise the trigger, so the
   behavioral fixture exists.

Follow the `// Intent / Why it exists / Scenario` preamble convention
(AGENTS.md; `docs/dev/testing.md`) on any new test.

## Files to modify

- **New:** `modules/braid/hardening.nix`; `docs/design/decisions/032-systemd-unit-hardening.md`;
  `tests/module/braid-alert-hardened.{nix,py}`.
- **Edit:** `modules/braid/monitor.nix` (monitor + conditional alert + new
  `braid-pcspkr-load.service` + relocate `modprobe` + harden
  `braid-scrub-failed.service`), `modules/braid/ups.nix`
  (ups-secrets + drop chown), `modules/braid/storage.nix` (seal + tmpfiles lock
  rule + harden `braid-scrub-resume-trigger`), `modules/braid/fan-control.nix`
  (retrofit onto base, no `PrivateDevices`; harden `braid-fan-reload`),
  `docs/design/decisions/018-systemd-lifecycle.md` (`## See`), `AGENTS.md`,
  `flake.nix` (register `braid-alert-hardened`), and the test `.py` files above
  (`monitor-lifecycle`, `braid-alert`, `immutable-mountpoint`,
  `ups-credential-lifecycle`, `fan-control`, `auto-scrub`, `scrub-alert`).

## Verification

- `nix flake check` / `nix-instantiate --parse` equivalent via the module eval:
  build each VM check with `just test-vm <name>` for `monitor-lifecycle`,
  `braid-alert`, `braid-alert-no-beep`, `braid-alert-hardened`,
  `immutable-mountpoint`, `ups-credential-lifecycle`, `fan-control`,
  `fan-control-hotswap`, `auto-scrub`, `scrub-alert` (VM tests run on macOS via
  `nix.linux-builder`, `aarch64-darwin`). `braid-alert-no-beep` is not edited but
  must be run: it behaviorally guards the `beep=false` arm of the new conditional
  alert composition (`alertType // profile`, where that arm is the
  `Type=oneshot`/`RemainAfterExit` latch). `fan-control` guards the
  `hddfancontrol-braid` retrofit (base present, `rr` intact, `PrivateDevices`
  absent) and `braid-fan-reload`; `fan-control-hotswap` is the live-fire restart
  guard under the sandbox; `auto-scrub` guards the hardened
  `braid-scrub-resume-trigger` (empty caps, reaches its decision under the
  sandbox).
- `just test-rust` (no Rust behavior changes expected, but the lock path and
  `cmd_monitor` are touched indirectly -- confirm green).
- Doc/repo gates: `just docs-build` (linkcheck for the new ADR + `## See`),
  `just check-docs-see-paths`, `just check-output-ascii` (nix `echo` lines),
  `just check-docs-frontmatter` (ADR 032 frontmatter).
- End-to-end sanity in a VM: unlock pool -> `systemctl start braid-monitor.service`
  -> verify it reaches the stats check (not exit 2) -> degrade -> alert fires ->
  `braid ack` clears it -> lock. This is exactly what `monitor-lifecycle.py`
  automates.

## Risks & test-gated directives

- **`PrivateDevices` is a per-unit, dependency-proven opt-in, never a default.**
  `hddfancontrol-braid` **must not** get it (proven: hddfancontrol 2.1.1 reads
  `/dev/disk/by-id` and validates block devices for `-d ata`); `fan-control.py`
  asserts its absence. `braid-monitor` also omits it: the VM gate showed
  `cryptsetup status` reports healthy mapper backing as `(null)` when the real
  `/dev` tree is hidden. `ups-secrets` keeps it (`/dev/urandom` is whitelisted).
  seal adds it because the VM test confirms the inode ioctl opens no `/dev` node.
  `alert` never gets it (beep needs `/dev/input`).
- **`ProtectKernelTunables`** is intentionally omitted from `base`; the btrfs
  ioctls go through the fs, not `/proc/sys`, so it is likely safe for monitor but
  is the one directive worth a separate gated test before adding.
- **`hddfancontrol-braid` retrofit:** it currently works with 7 inline
  directives; the base adds 6 more (all safe for a sysfs-writing daemon). The
  only conflict to avoid is `RestrictRealtime` (excluded from base). Validate
  with a VM test before trusting the retrofit.
- **Dispatch shells take empty caps (verified, including the deceptive case).**
  Both `braid-fan-reload` (pure `sleep` + `systemctl restart`) and
  `braid-scrub-resume-trigger` take an empty `CapabilityBoundingSet`. The trigger
  is the subtle one: `btrfs scrub status` *does* issue the `CAP_SYS_ADMIN`-gated
  scrub-progress ioctl (`fs/btrfs/ioctl.c` `btrfs_ioctl_scrub_progress`), so the
  naive read is "keep the cap." But btrfs-progs swallows the ioctl's `EPERM`
  (`is_scrub_running_in_kernel` returns 0 on any ioctl error) and feeds it only to
  the display-only `in_progress`/"running" annotation; the exit code (`!!err`) and
  the resume-relevant `Status:` words (aborted/interrupted/finished) come from the
  ungated `fs_info`/`dev_info`/`space_info` ioctls and the persisted status file,
  so the command exits 0 and decides correctly with empty caps. Missing caps can
  only misread Running -> Interrupted (safe over-resume), and at pool-online
  trigger time no scrub is running. So **don't** grant `CAP_SYS_ADMIN` here -- it
  would over-privilege the unit (the trap is the *opposite* of monitor's: the
  gated ioctl looks load-bearing but isn't). Neither shell writes, so neither
  needs `ReadWritePaths`; the trigger is `LockPolicy::None`, so unlike monitor it
  never touches `/run/braid-pool.lock` and has no EROFS trap.
- **`systemd-analyze security`** is **not** used as an assertion: it is absent
  from the test VMs and its scores drift across systemd versions. Prefer
  behavioral + `systemctl show -p` presence checks.

## Rejected alternatives

- **Two-unit `braid-alert` split** (separate hardened beep loop vs. unhardened
  command): the textbook maximum, but it would touch `braid ack`, doctor,
  `notifier-config.json`, ADR 018, and the alert tests to tighten a low-risk
  shell loop. Too much complexity for a home NAS; the conditional single-unit
  profile captures the benefit in the common (beep-only) case.
- **`ReadWritePaths=[/run]`** (broad) instead of pre-creating
  `/run/braid-pool.lock`: simpler and survives a manual `rm`, but exposes all of
  `/run` read-write. The pre-created exact-file entry is tighter least-privilege.
- **Moving the pool lock to a `RuntimeDirectory=braid`**: would make `/run`
  handling automatic but changes `POOL_LOCK_PATH` globally (all CLI commands +
  ADR 018/026 + the stop-coordinator), a far larger Rust+docs change for no extra
  benefit here.
- **Keeping `modprobe` in the alert loop** (drop `ProtectKernelModules`, add
  `CAP_SYS_MODULE` to the alert bounding set) instead of a dedicated loader:
  rejected because it grants module-load privilege to the long-lived beep loop --
  the opposite of least-privilege -- and complicates the conditional profile
  (both branches would need the cap + the dropped protection). The tiny
  `braid-pcspkr-load.service` confines that privilege to a one-shot that exits.

## Implementation notes

- The systemd-unit hardening ADR was numbered `033-systemd-unit-hardening.md`
  because `032-pool-mount-hardening.md` already exists in this branch.
- `braid-alert-advisory.service` receives the same strong-vs-light alert profile
  choice as `braid-alert.service` because it runs the same operator-supplied
  `alertCommand` path for warning-only alerts.
- The `braid-alert` VM cannot assert a live `pcspkr` load because the test kernel
  does not ship that module; the test instead proves that every alert start
  re-runs `braid-pcspkr-load.service` via its monotonic start timestamp.
- `braid-seal-mountpoint.service` uses `ReadWritePaths=dirOf(cfg.mountPoint)`,
  not the exact mountpoint. An exact `ReadWritePaths` exception creates a private
  bind mount at the guarded path, so `STATX_ATTR_MOUNT_ROOT` correctly classifies
  it as mounted and the seal becomes inert.
- `braid-monitor.service` omits `PrivateDevices`. The VM gate showed that
  `cryptsetup status` reports healthy mappers as null-backed when the real
  `/dev` tree is hidden, causing a false `MissingDevice` alert.
- `braid-monitor.service` keeps `CAP_SYS_ADMIN`. The VM gate showed that
  `cryptsetup status` cannot initialize device-mapper with an empty capability
  bounding set, even though the btrfs stats ioctls themselves do not need the
  capability.
- `braid-scrub-failed.service` was added to the hardening scope during
  implementation review. It was introduced by a recent scrub-alert change and
  fits the same dispatch-shell profile: it writes only `/var/lib/braid` state
  and starts `braid-alert.service` through PID1, so it uses the shared base,
  `ReadWritePaths=/var/lib/braid`, empty capabilities, and AF_UNIX.
