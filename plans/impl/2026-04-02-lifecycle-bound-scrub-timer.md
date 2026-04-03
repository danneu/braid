# Plan: Lifecycle-bound scrub timer

## Context

braid delegates scrub scheduling to `services.btrfs.autoScrub` (nixpkgs), which creates a timer `wantedBy timers.target`. This timer fires on calendar boundaries regardless of pool mount state. If the pool is locked when the monthly timer fires, `btrfs scrub start -B /mnt/storage` fails (unmounted). The timer considers itself fired, waits another month. Manual `braid unlock` days later does not trigger a deferred scrub — `Persistent=true` only catches up when a timer transitions from inactive→active, and the nixpkgs timer was active the whole time.

This plan replaces the delegation with braid-owned `braid-scrub.timer` + `braid-scrub.service` whose lifecycle is tied to `braid-online.service`. The timer is only active while the pool is online. `Persistent=true` catches up missed scrubs when the timer is reactivated after a period of inactivity. This also subsumes the option-surface work from `plans/wip/sprightly-tickling-kahn.md`, which this plan supersedes.

## Design

### Timer lifecycle

```
braid unlock → wrapper starts braid-online.service
                 → systemd starts braid-scrub.timer  (wantedBy)
                   → Persistent=true checks stamp file (/var/lib/systemd/timers/)
                     → stamp exists + overdue? immediate catch-up scrub
                     → no stamp (first ever)? schedule next OnCalendar
                     → stamp exists + not overdue? wait for next OnCalendar

braid lock   → wrapper stops braid-scrub.timer     (pre-lock, new)
                 → prevents timer from re-triggering scrub
               → wrapper stops braid-scrub.service   (pre-lock, new)
                 → ExecStop cancels in-flight scrub
               → wrapper runs CLI braid lock
                 → unmount succeeds (scrub no longer holding mount)
               → wrapper stops braid-online.service (post-lock, existing)
                 → BindsTo is now a no-op (timer already stopped)

shutdown     → systemd stops braid-scrub.service first (After ordering)
                 → ExecStop cancels in-flight scrub
               → systemd stops braid-online.service
                 → ExecStop = braid lock → unmount succeeds
```

### How Persistent=true catch-up works

systemd stores the last-trigger timestamp in a stamp file (`/var/lib/systemd/timers/stamp-braid-scrub.timer`). On timer activation, `Persistent=true` reads this stamp and fires immediately if a calendar event was missed while the timer was inactive. Key behaviors:

- **First-ever activation (no stamp file):** No catch-up — systemd has no prior trigger time to compare against. systemd creates the stamp file on activation with `mtime=now`, then updates it with the actual trigger time when the timer fires. The first scrub runs at the next `OnCalendar` boundary (e.g., start of next month for `monthly`).
- **Subsequent activations (stamp exists):** If the gap between the stamp's mtime and now spans a calendar boundary, systemd fires immediately. Only one catch-up activation occurs regardless of how many events were missed (systemd.timer(5)).
- **Practical effect:** After the very first scrub runs (within a month of initial setup), every subsequent `braid unlock` catches up any scrub missed while the pool was locked. The common case — pool locked for weeks/months, then manually unlocked — triggers an immediate scrub.

### Two lock paths, both safe

**Manual lock:** The wrapper must stop `braid-scrub.timer` and then `braid-scrub.service` before running the CLI. The timer must stop first — otherwise it can re-trigger the service in the window between service stop and unmount. The CLI's `umount` fails with EBUSY if scrub holds the mount (`lock.rs:590`). The wrapper already manages systemd state for unlock/add (post-processing); this adds symmetric pre-processing for lock.

**Shutdown/systemctl stop:** systemd's own ordering handles it — `braid-scrub.service` has `After=braid-online.service`, so systemd stops it first, triggering ExecStop's cancel script. Then `braid-online.service`'s `ExecStop=braid lock` runs against a clean mount.

### New consumer pattern

This is a third pattern for `systemd-lifecycle.md`, distinct from the existing two:

- **Frequent periodic** (monitor): `ConditionPathIsMountPoint` only. Fires every 5min; missed fires are cheap.
- **Long-running** (samba/nfs): `BindsTo + After braid-online.service`. Must stop before lock.
- **Infrequent periodic** (scrub): Timer and service both use `BindsTo + After braid-online.service`. Like the long-running pattern for lifecycle, but the timer drives periodic activation. `Persistent=true` handles catch-up. `ConditionPathIsMountPoint` on the service is defense-in-depth.

### Why BindsTo on braid-online, not mnt-storage.mount

Per `systemd-lifecycle.md`: `mnt-storage.mount` doesn't exist until the CLI mounts the pool at runtime (auto-generated from `/proc/mounts`). `BindsTo`/`After` on it forces systemd to load the unit, which fails before first unlock. `braid-online.service` is declared in the NixOS config and always exists.

## Changes

### 1. `modules/braid/options.nix` — add `autoScrub` options + assertion

Add after the `autoUnlock` block (after line 57):

```nix
autoScrub = {
  enable = lib.mkEnableOption "periodic btrfs scrub" // { default = true; };

  interval = lib.mkOption {
    type = lib.types.str;
    default = "monthly";
    description = "systemd calendar expression for periodic scrub scheduling.";
  };
};
```

Add to the `assertions` list (line 61):

```nix
{
  assertion = !(cfg.autoScrub.enable && config.services.btrfs.autoScrub.enable);
  message = "braid.autoScrub replaces services.btrfs.autoScrub. Disable one to avoid duplicate scrubs.";
}
```

### 2. `modules/braid/storage.nix` — replace scrub wiring with braid-owned units

Remove lines 26–30 (`services.btrfs.autoScrub = { ... }`).

Add, guarded by `lib.mkIf cfg.autoScrub.enable`:

```nix
systemd.timers.braid-scrub = lib.mkIf cfg.autoScrub.enable {
  description = "Periodic btrfs scrub for braid pool";
  wantedBy = [ "braid-online.service" ];
  bindsTo = [ "braid-online.service" ];
  after = [ "braid-online.service" ];
  timerConfig = {
    OnCalendar = cfg.autoScrub.interval;
    AccuracySec = "1d";
    Persistent = true;
  };
};

systemd.services.braid-scrub = lib.mkIf cfg.autoScrub.enable {
  description = "btrfs scrub on ${cfg.mountPoint}";
  documentation = [ "man:btrfs-scrub(8)" ];
  conflicts = [ "shutdown.target" "sleep.target" ];
  before = [ "shutdown.target" "sleep.target" ];
  bindsTo = [ "braid-online.service" ];
  after = [ "braid-online.service" ];
  unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
  serviceConfig = {
    Type = "simple";
    Nice = 19;
    IOSchedulingClass = "idle";
    ExecStart = "${btrfsProgs}/bin/btrfs scrub start -B ${cfg.mountPoint}";
    ExecStop = pkgs.writeShellScript "braid-scrub-maybe-cancel" ''
      (${btrfsProgs}/bin/btrfs scrub status ${cfg.mountPoint} | ${pkgs.gnugrep}/bin/grep finished) || ${btrfsProgs}/bin/btrfs scrub cancel ${cfg.mountPoint}
    '';
  };
};
```

Notes:
- Uses `btrfsProgs` (braid's pinned version, already a `let` binding at `storage.nix:11`).
- `gnugrep` is needed in `ExecStop` — use `pkgs.gnugrep`.
- No `--limit` flag (braid's HDD defaults don't need rate limiting; `Nice=19` + `IOSchedulingClass=idle` suffice).

### 3. `modules/braid/braid-wrapper.sh` — cancel scrub before CLI lock

The wrapper runs the CLI first, then manages systemd state. For lock, the CLI's `umount` fails with EBUSY if scrub holds the mount open (`lock.rs:590 lock_umount_busy_fails`). Add a pre-processing case to stop the scrub service before the CLI runs.

Insert between lines 55 and 57 (after the unlock re-check case, before `@braidBin@ "$@"`):

```sh
# Stop scrub timer and service before CLI lock attempts unmount.
# Timer must stop first — otherwise it can re-trigger the service between
# service stop and unmount. braid-scrub.service holds the mount busy while
# running (-B flag); without this, umount would fail with EBUSY.
# Harmless no-op when autoScrub is disabled (units don't exist) or scrub
# isn't running.
case "$subcmd" in
  lock)
    if ! $skip_fixup; then
      @systemctlBin@ stop braid-scrub.timer 2>/dev/null || true
      @systemctlBin@ stop braid-scrub.service 2>/dev/null || true
    fi
    ;;
esac
```

Both `systemctl stop` calls are synchronous (no `--no-block`) — the timer stop returns immediately (timers have no ExecStop), and the service stop waits for ExecStop to complete before proceeding to the CLI. The `|| true` and `2>/dev/null` handle the case where autoScrub is disabled (units don't exist) or scrub isn't running.

### 4. `modules/braid/auto-suspend.nix` — update wakeup match

Change lines 101–104 from:

```nix
BtrfsScrub = {
  class = "SystemdTimer";
  match = "btrfs-scrub@.*";
};
```

To:

```nix
BtrfsScrub = {
  class = "SystemdTimer";
  match = "braid-scrub";
};
```

autosuspend uses `re.match()` (`reference/autosuspend/src/autosuspend/checks/systemd.py:71`), anchored at start — `"braid-scrub"` matches `braid-scrub.timer`.

### 5. `tests/module/auto-scrub.nix` + `auto-scrub.py` — config test (new)

Three-node config-only test following the `braid-auto-suspend` pattern. No pool fixture needed.

**Nodes:** `defaults` (enabled, monthly), `disabled` (`autoScrub.enable = false`), `weekly` (custom interval).

**`auto-scrub.py` subtests:**

1. `defaults: braid-scrub.timer is loaded` — `systemctl cat braid-scrub.timer` succeeds
2. `defaults: timer is bound to braid-online.service` — `BindsTo` and `After` contain `braid-online.service`
3. `defaults: timer fires monthly with Persistent=true` — `OnCalendar` contains `monthly`, `Persistent` is `yes`
4. `defaults: scrub service targets pool mount point` — `ExecStart` contains `btrfs scrub start -B` and `/mnt/storage`
5. `defaults: scrub service has correct scheduling priority` — `Nice=19`, `IOSchedulingClass=idle`
6. `defaults: scrub service has ConditionPathIsMountPoint` — check condition
7. `defaults: scrub service conflicts with shutdown and sleep` — check `Conflicts` and `Before`
8. `defaults: scrub service is bound to braid-online` — `BindsTo` and `After`
9. `defaults: nixpkgs scrub timer does not exist` — `fail("systemctl cat btrfs-scrub-mnt-storage.timer")`
10. `disabled: braid-scrub.timer does not exist` — `fail("systemctl cat braid-scrub.timer")`
11. `weekly: timer fires weekly` — `OnCalendar` contains `weekly`

### 6. `tests/module/scrub-lifecycle.nix` + `scrub-lifecycle.py` — behavioral test (new)

Two-node test with real pool fixtures (initrd-fixture pattern from `systemd-lifecycle.nix`). Tests the two behaviors that justify this redesign.

**Node `catchup`:** Real scrub ExecStart. Verifies Persistent catch-up fires when the timer activates with an overdue stamp.

**Node `cancel`:** Override ExecStart to simulate a long-running scrub that holds the mount busy (`exec 3>/mnt/storage/.scrub-lock; sleep 300`). Verifies `braid lock` succeeds because the wrapper stops the scrub service first.

The cancel node's ExecStart override makes the test deterministic — no timing race with a real scrub that might complete before lock on tiny test disks.

**`scrub-lifecycle.py` subtests:**

```
# === catchup node ===

1. "timer inactive before unlock"
   - systemctl cat braid-scrub.timer succeeds (unit exists)
   - systemctl is-active braid-scrub.timer fails (not running)

2. "Persistent catch-up fires on unlock with overdue stamp"
   - Seed old stamp file to create explicit overdue state:
       mkdir -p /var/lib/systemd/timers
       touch -t 202501010000 /var/lib/systemd/timers/stamp-braid-scrub.timer
     This simulates a timer that last fired on 2025-01-01 — well past the
     monthly boundary, making the scrub overdue.
   - printf passphrase | braid unlock --passphrase-stdin
   - systemctl is-active braid-online.service
   - systemctl is-active braid-scrub.timer
   - wait_until_succeeds: systemctl show braid-scrub.service
       -p Result --value | grep success
     (Persistent=true reads old stamp → fires immediately. Scrub on tiny
      disk completes in milliseconds, so check Result.)

3. "timer stops when pool is locked"
   - braid lock
   - systemctl is-active braid-scrub.timer fails
   - systemctl is-active braid-online.service fails

4. "catch-up fires again after stamp is re-aged"
   - Record ExecMainStartTimestampMonotonic from previous run:
       old_ts = systemctl show braid-scrub.service
         -p ExecMainStartTimestampMonotonic --value
   - Age the stamp file back to 2025-01-01 (lock stopped the timer,
     so systemd won't interfere with the file):
       touch -t 202501010000 /var/lib/systemd/timers/stamp-braid-scrub.timer
   - printf passphrase | braid unlock --passphrase-stdin
   - wait_until_succeeds: ExecMainStartTimestampMonotonic changed from old_ts
     AND Result == success
     (Uses timestamp comparison to distinguish second run from stale state.
      Full cycle: lock → age stamp → unlock → catch-up fires.)

# === cancel node ===

5. "lock succeeds while scrub holds mount busy"
   - Seed old stamp (same as catchup node — needed so Persistent fires
     the fake scrub immediately on unlock, not at next month boundary):
       mkdir -p /var/lib/systemd/timers
       touch -t 202501010000 /var/lib/systemd/timers/stamp-braid-scrub.timer
   - printf passphrase | braid unlock --passphrase-stdin
   - wait_until_succeeds: systemctl is-active braid-scrub.service
     (fake scrub runs sleep 300 with open FD on /mnt/storage/.scrub-lock)
   - braid lock  ← succeeds because wrapper stops scrub first
   - mountpoint -q /mnt/storage fails (pool unmounted)
   - test -e /dev/mapper/braid-disk1 fails (LUKS closed)
   - test -e /dev/mapper/braid-disk2 fails (LUKS closed)
```

**`scrub-lifecycle.nix`:** Same structure as `systemd-lifecycle.nix` — imports `initrd-fixture.nix`, seeds `pool.json` via tmpfiles, overrides `braid-unlock.script` to avoid interactive prompt. The `cancel` node additionally overrides `braid-scrub.serviceConfig.ExecStart` with `lib.mkForce`.

**Why subtest 4 re-ages the stamp instead of just checking timer state on re-unlock:** Subtest 3 verifies the timer stops. Subtest 4 verifies the full catch-up lifecycle: lock stops the timer, time passes (simulated by aging the stamp), unlock restarts the timer, Persistent detects the gap and fires. Without re-aging, the stamp would reflect the recent scrub from subtest 2, and the re-unlock would just schedule the next calendar event without a catch-up — which wouldn't test the feature.

### 7. Update `tests/module/braid-auto-suspend.py` line 72

Change:
```python
assert "btrfs-scrub@" in config, "Missing btrfs-scrub@ match pattern in config"
```
To:
```python
assert "braid-scrub" in config, "Missing braid-scrub match pattern in config"
```

### 8. `flake.nix` — register tests

Add near line 505 (after `braid-auto-suspend`):

```nix
braid-auto-scrub = pkgs.testers.nixosTest (
  import ./tests/module/auto-scrub.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
scrub-lifecycle = pkgs.testers.nixosTest (
  import ./tests/module/scrub-lifecycle.nix {
    braid = linuxCrane.braid-cli-unwrapped;
  }
);
```

### 9. Documentation updates

**`docs/decisions/systemd-lifecycle.md`:**
- Add `braid-scrub.timer → braid-scrub.service` to the ASCII diagram
- Add subsection documenting the timer-lifecycle pattern and both lock paths (manual vs shutdown)
- Update "Consumer dependency contracts" with the third pattern (infrequent periodic)
- Update "CLI wrapper as synchronization layer" to document the new lock pre-processing

**`docs/decisions/sane-defaults.md`:**
- Replace `services.btrfs.autoScrub.*` rows in the defaults table with `braid.autoScrub.*` rows
- Change "Wrap scrub" alternative from Rejected → Accepted with rationale
- Add case study to "When to wrap" section

**`docs/principles.md` line 40:**
- Update Principle 7: replace the "only wrap when non-obvious" sentence with the product-boundary framing

**`README.md` lines 398–406:**
- Update scrub section to use `braid.autoScrub` with a note about lifecycle-aware scheduling

### 10. Clean up superseded plan

Remove `plans/wip/sprightly-tickling-kahn.md` (superseded by this plan, which will move to `plans/impl/` on completion).

## Files modified

| File | Change |
|------|--------|
| `modules/braid/options.nix` | Add `autoScrub` option block + conflict assertion |
| `modules/braid/storage.nix` | Remove `services.btrfs.autoScrub`; add `braid-scrub.timer` + `braid-scrub.service` |
| `modules/braid/braid-wrapper.sh` | Pre-lock: stop `braid-scrub.timer` then `braid-scrub.service` before CLI runs |
| `modules/braid/auto-suspend.nix` | Wakeup match: `btrfs-scrub@.*` → `braid-scrub` |
| `tests/module/auto-scrub.nix` | **New** — three-node config test |
| `tests/module/auto-scrub.py` | **New** — unit property assertions |
| `tests/module/scrub-lifecycle.nix` | **New** — two-node behavioral test (catch-up + cancellation) |
| `tests/module/scrub-lifecycle.py` | **New** — Persistent catch-up on unlock, safe lock during scrub |
| `tests/module/braid-auto-suspend.py` | Update scrub wakeup assertion (line 72) |
| `flake.nix` | Register `braid-auto-scrub` and `scrub-lifecycle` tests |
| `docs/decisions/systemd-lifecycle.md` | Document scrub units, timer-lifecycle pattern, wrapper pre-lock |
| `docs/decisions/sane-defaults.md` | Reverse "Wrap scrub" rejection; update defaults table |
| `docs/principles.md` | Update Principle 7 wording |
| `README.md` | Update scrub config section |
| `plans/wip/sprightly-tickling-kahn.md` | Remove (superseded) |

## Verification

1. **TDD Red:** Write `auto-scrub.{nix,py}`, `scrub-lifecycle.{nix,py}`, register in `flake.nix`. Run `just test braid-auto-scrub` and `just test scrub-lifecycle` — both fail (options/units don't exist).
2. **Implement:** Apply `options.nix`, `storage.nix`, `braid-wrapper.sh`, `auto-suspend.nix` changes.
3. **TDD Green:** `just test braid-auto-scrub` and `just test scrub-lifecycle` both pass.
4. **Auto-suspend:** Update `braid-auto-suspend.py` assertion. `just test braid-auto-suspend` passes.
5. **Regression:** `just test` — all tests pass. Key risk: every test node with `braid.enable = true` now gets the `braid-scrub` timer by default, but since it's `wantedBy braid-online.service` (not `timers.target`), it won't start until pool is unlocked — existing tests that don't unlock are unaffected. The wrapper change (`systemctl stop braid-scrub.service`) is a no-op when the service isn't running.
6. **Docs:** Apply doc updates.
