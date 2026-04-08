# Plan: USB Keyfile Auto-Unlock (Stage-2, Best-Effort, CLI-Centric)

## Summary

Implement USB keyfile auto-unlock as a stage-2 best-effort path that never
blocks boot and does not couple CLI behavior to systemd.
Core decisions locked:

- braid unlock remains pure (no systemctl calls, works outside systemd).
- Auto-unlock is handled by a new boot service that calls braid unlock
  --passphrase-file ....
- Service dependencies for consumers (Samba, etc.) use
  RequiresMountsFor=${config.braid.mountPoint} as primary readiness contract.
- Keep braid-pool.target as compatibility alias for manual workflows.

## Public Interface Changes

### New module options

Add to braid options in modules/braid/options.nix (/Users/dan/Code/braid/
modules/braid/options.nix):

- braid.autoUnlock.enable (bool, default false)
- braid.autoUnlock.keyDevice (string /dev/disk/by-id/..., required when
  enabled)
- braid.autoUnlock.passphraseFile (string, default "/passphrase.txt",
  path inside mounted USB FS)
- braid.autoUnlock.timeoutSec (int, default 5)

### No CLI API changes

braid unlock flags/behavior remain unchanged.

## Implementation Design

## 1) Keep CLI unlock as source of truth

No changes to unlock algorithm in Rust.
braid unlock continues to perform:

- probe disks
- single-passphrase verification
- open LUKS
- btrfs device scan
- mount pool (degraded when needed)

This preserves non-systemd usability (rescue environments).

## 2) Refactor existing manual unit to call CLI (dedupe logic)

In modules/braid/storage.nix (/Users/dan/Code/braid/modules/braid/storage.nix),
replace shell-open loop in systemd.services.braid-unlock with wrapper behavior:

- prompt via systemd-ask-password
- pipe passphrase to CLI:
  - printf '%s\n' "$pass" | braid unlock --passphrase-stdin
- keep ConditionPathIsMountPoint = "!${cfg.mountPoint}"

Prerequisite: this makes the manual service depend on the braid CLI binary.
Add assertion: cfg.package != null when braid.enable = true. The module
already requires the CLI for `braid add`, `braid status`, etc. — making it
explicit prevents a broken service if someone enables the module without
setting the package.

Result: manual service and CLI share one unlock implementation while CLI
remains systemd-agnostic.

## 3) Add USB key mount units (stage-2 only)

Define dedicated systemd mount/automount units for /run/braid-key (no initrd
changes):

- mount source: cfg.autoUnlock.keyDevice
- mountpoint: /run/braid-key
- DirectoryMode=0700 (root:root) — passphrase file is plaintext; non-root
  users must not be able to traverse to it even during the brief mount window.
  See docs/luks-unlock.md § "Mount point permissions".
- fstype: "auto" (hardcoded — kernel probes vfat, ext4, etc.)
- options: ro,nofail,noauto,x-systemd.device-timeout=${timeout}s
- add automount unit so first file access triggers mount attempt

nofail + device-timeout + noauto together guarantee the USB never blocks
boot: systemd waits at most timeoutSec for the device, then gives up. The
automount unit is not started at boot — it fires only when the auto-unlock
service accesses the mount point. See docs/luks-unlock.md § "Boot
resilience".

These units must not make boot fail if device is missing.

## 4) Add best-effort auto-unlock service

New unit: systemd.services.braid-auto-unlock:

- WantedBy = [ "multi-user.target" ]
- After = [ "local-fs.target" ]
- ConditionPathIsMountPoint = "!${cfg.mountPoint}" (skip if already mounted)
- script behavior:
  1. Build key path: /run/braid-key${cfg.autoUnlock.passphraseFile}
  2. Check readability of key file (this triggers automount)
  3. If missing/unreadable: log informational message, exit 0
  4. If present: run braid unlock --passphrase-file "$key_path"
  5. Cleanup: umount /run/braid-key || true (always, regardless of outcome)
  6. On failure: log warning, exit 0 (boot resilience invariant)

Step 5 is critical: the standard USB-key-unlock pattern is mount → read →
unmount. Without cleanup, the plaintext passphrase stays readable at
/run/braid-key/passphrase.txt for the rest of the session. This is exactly
the Unraid CVE pattern (passphrase at rest on a mounted filesystem). The
umount closes that window. See docs/luks-unlock.md § "Plaintext keyfile
exposure".

No systemctl start braid-pool.target from this service.

## 5) Keep braid-pool.target for convenience

Target remains a convenience alias over manual unlock flow; not required for
dependent service readiness.

- Discoverable through systemctl list-units.
- A place to hang future services that should start when the pool comes online
  (if you ever want WantedBy=braid-pool.target on something).

Note: braid-pool.target stays inactive after a successful auto-unlock because
nothing starts it. This is intentional — the target is a manual trigger, not
a status indicator. Operators should check mount state directly
(`mountpoint -q ${cfg.mountPoint}`). Document this in README.

## 6) Dependency guidance for consumers

Document and apply where relevant:

- Primary: RequiresMountsFor=${config.braid.mountPoint}
- Optional: After=braid-auto-unlock.service only if useful for startup ordering
  noise reduction
- Do not depend on unlock target/service as readiness signal

## Files To Change

- modules/braid/options.nix (/Users/dan/Code/braid/modules/braid/options.nix)
  - add autoUnlock option subtree + validation assertions
  - add assertion: cfg.package != null when braid.enable = true
- modules/braid/storage.nix (/Users/dan/Code/braid/modules/braid/storage.nix)
  - refactor braid-unlock wrapper
  - add /run/braid-key mount/automount (DirectoryMode=0700)
  - add braid-auto-unlock.service (with umount cleanup)
- README.md (/Users/dan/Code/braid/README.md)
  - add “USB Auto-Unlock” section with config + behavior guarantees
  - keep/manual braid unlock and braid-pool.target docs
  - explicitly state best-effort semantics
  - note that braid-pool.target does not reflect auto-unlock state
- docs/1-user-stories.md (/Users/dan/Code/braid/docs/1-user-stories.md)
  - add flow for boot with USB key present/absent
- docs/luks-unlock.md (/Users/dan/Code/braid/docs/luks-unlock.md)
  - already created — research reference for USB naming stability,
    passphrase vs keyfile semantics, Unraid CVE, boot resilience, mount
    permissions
- docs/decisions/004-single-passphrase.md (/Users/dan/Code/braid/docs/decisions/
  004-single-passphrase.md)
  - already updated — “Constraint: No keyfiles” replaced with “Scope”
    section clarifying that additional unlock mechanisms (USB keyfiles, TPM)
    are orthogonal to the shared passphrase

## Validation and Assertions

Add module assertions:

- cfg.package != null when braid.enable = true (the refactored braid-unlock
  service and auto-unlock both call the CLI binary)
- keyDevice starts with /dev/disk/by-id/ (see docs/luks-unlock.md § "USB
  device naming stability" — /dev/sdX names shift across reboots)
- passphraseFile is absolute within the USB FS (/…)
- timeoutSec > 0

These are config-time validation only; runtime failures remain non-fatal.

## Implementation Comments

Code implementing this plan should include comments at key points
referencing docs/luks-unlock.md so future readers find the research:

- keyDevice validation: why by-id is required (naming stability)
- mount unit options: why nofail + device-timeout + noauto (boot resilience)
- DirectoryMode=0700: why the mount point is locked down (passphrase exposure)
- umount cleanup: why we unmount after use (Unraid CVE pattern)
- passphraseFile option: how this differs from a binary keyfile and why they
  are not interchangeable (PBKDF vs raw key material)

## Test Plan

### New VM tests

Add module-focused tests (NixOS VM):

1. module-auto-unlock-key-present

- USB key device exists with valid passphrase file
- boot to multi-user.target
- assert /mnt/storage mounted automatically
- assert data read/write works

2. module-auto-unlock-key-missing

- configured key device absent
- boot succeeds
- assert /mnt/storage not mounted
- assert system remains functional
- assert journal contains “keyfile missing, skipping auto-unlock”

3. module-auto-unlock-key-wrong

- key device present, wrong passphrase
- boot succeeds
- assert pool remains locked/unmounted
- assert warning logged

### Regression tests to run

- existing unlock CLI tests: tests/cli/braid-unlock.nix (/Users/dan/Code/braid/
  tests/cli/braid-unlock.nix)
- degraded boot and replace flows
- full suite (just test)

## Acceptance Criteria

- Boot never blocks/fails due to missing or invalid USB key.
- With valid key, pool comes online automatically in stage-2.
- braid unlock continues to work without systemd orchestration.
- systemctl start braid-pool.target still works (compatibility).
- Dependent services can rely on RequiresMountsFor=${mountPoint}.

## Failure Modes and Expected Outcomes

- USB not inserted: auto-unlock skipped, boot OK, pool locked.
- USB unreadable/fs mismatch: auto-unlock warning, boot OK, pool locked.
- wrong passphrase: unlock fails, warning, boot OK, pool locked.
- partial disks present: unlock opens available disks; mount may degrade as
  current CLI behavior dictates.

## Assumptions and Defaults Chosen

- Passphrase file format: plain text passphrase (newline-trimmed), matching
  current --passphrase-file behavior.
- CLI remains systemd-agnostic by design.
- braid-pool.target retained for compatibility, but mount readiness
  (RequiresMountsFor) is the authoritative dependency model.
- No initrd modifications are part of this feature.
