# Plan: `braid.poolBoundServices` -- module option that stamps the pool-lifecycle consumer contract

## Context

ADR 018 documents a "consumer dependency contract" for long-running services
that read the pool (samba, nfs): `wantedBy` + `bindsTo` + `after` on
`braid-online.service`, plus `unitConfig.ConditionPathIsMountPoint` on the pool
mount. Today users hand-write that 4-field block per service
(`docs/guides/sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle`),
which requires knowing three braid internals (unit name, mount path, field
combo) and is easy to get wrong -- forget `wantedBy` and the service never
starts after unlock; forget `after` and there is no stop-ordering guarantee;
forget the condition and the service serves the empty offline mountpoint.

This feature adds `braid.poolBoundServices = [ "samba-smbd" "nfs-server" ]`: the
module stamps the exact documented contract onto each named service. Pure
module-level sugar -- **zero Rust changes**. `braid lock` already walks
`systemctl show -P BoundBy braid-online.service` and stops bound consumers
before unmount; `bindsTo` is what produces that BoundBy edge.

## Settled design decisions

- **Name:** `braid.poolBoundServices` (user-confirmed, replacing earlier
  working names `gateOnPool` and `poolConsumers`). Adjective-first nixpkgs
  idiom (`allowedTCPPorts`, `trustedInterfaces`); sorts next to the
  `poolAccessGroup` sibling; "bound" names the actual mechanism -- `BindsTo`,
  and the `BoundBy` edge `braid lock` walks. "Consumer" stays ADR 018 prose
  vocabulary, not API surface.
- **Stamp all four fields** exactly as ADR 018's long-running-consumer contract
  and as `tests/module/lock-stops-bound-consumers.nix` hand-writes on
  `dummy-pool-consumer`.
- **Append-only, never `lib.mkForce`.** The consumer keeps its own boot edges
  (`multi-user.target`, `samba.target`); the condition turns premature boot
  starts into a clean skip. List-typed unit options (`wantedBy`/`after`/
  `bindsTo`) merge additively; `unitConfig` keys union (an operator setting a
  *different* `ConditionPathIsMountPoint` scalar is a loud eval conflict --
  acceptable).
- **v1 scope: services only**, bare NixOS names as keyed in
  `systemd.services.<name>` ("samba-smbd", not "samba-smbd.service"). Reject
  suffixed names by assertion. Timer-driven oneshot jobs are docs-warned away
  (a `wantedBy` edge would fire them on every unlock); subvolume consumers keep
  binding to their mount unit per `docs/guides/mounting-subvolumes.md`.
- **Fail closed on typos.** A misspelled name would silently materialize a
  phantom unit skeleton and gate nothing. Assert per name via definitions
  counting (below) -- NOT `config.systemd.services ? name` (always true, our
  own stamp creates the key) and NOT `serviceConfig ? ExecStart` (falsely
  rejects `nfs-server`, whose ExecStart ships via `systemd.packages` and never
  appears in Nix-level config).

## Changes

### 1. New file: `modules/braid/pool-bound-services.nix`

Per-feature-file shape like `modules/braid/ups.nix`: option + config +
assertions in one file. (Not in `storage.nix`: it defines
`systemd.services.braid-online` etc. as dotted keys, so adding
`systemd.services = lib.genAttrs ...` in the same attrset is a duplicate-attr
clash; a separate module file merges for free.)

```nix
# Pool-lifecycle consumer stamping -- braid.poolBoundServices.
#
# Stamps the long-running-consumer contract from ADR 018 "Consumer dependency
# contracts" (wantedBy/bindsTo/after braid-online.service +
# ConditionPathIsMountPoint on the pool mount) onto each listed service.
# Append-only: the service keeps its own boot edges; the condition turns
# premature boot starts into a clean skip while the pool is locked.
{
  config,
  options,
  lib,
  ...
}:
let
  cfg = config.braid;

  # Bare NixOS service names only (the systemd.services.<name> key) --
  # a suffixed name would materialize "<name>.service.service".
  knownUnitSuffixes = [
    "service" "socket" "target" "timer" "mount" "automount"
    "swap" "path" "slice" "scope" "device"
  ];
  isBareServiceName =
    name: name != "" && !builtins.any (s: lib.hasSuffix ".${s}" name) knownUnitSuffixes;

  # Exclude braid-online itself: stamping bindsTo braid-online.service onto it
  # would be a self-dependency.
  validNames = builtins.filter (n: isBareServiceName n && n != "braid-online") cfg.poolBoundServices;

  # Fail closed on typos. Our genAttrs stamp below contributes exactly one raw
  # definition of systemd.services carrying each name, so a real service must
  # appear in at least one OTHER module's definition. Shallow by design:
  # `def ? name` forces only attr names, never service bodies -- no recursion
  # into the services being checked. A declared-but-disabled service still
  # counts; benign, because the stamped skeleton is condition-gated and never
  # starts, and braid lock's BoundBy stop of an inactive unit is a no-op.
  definedElsewhere =
    name: lib.count (def: def ? ${name}) options.systemd.services.definitions > 1;
in
{
  options.braid.poolBoundServices = lib.mkOption {
    type = lib.types.listOf lib.types.str;
    default = [ ];
    example = [ "samba-smbd" "nfs-server" ];
    description = ''
      Long-running systemd services to bind to the pool lifecycle, as bare
      NixOS service names (the systemd.services.<name> key) -- "samba-smbd",
      not "samba-smbd.service".

      Each listed service gets the consumer contract from ADR 018: wantedBy +
      bindsTo + after braid-online.service, plus ConditionPathIsMountPoint on
      braid.mountPoint. It starts after `braid unlock` brings the pool online
      and stops before `braid lock` unmounts. Stamping is append-only: the
      service keeps its existing boot edges, and the condition turns premature
      boot starts into a clean skip while the pool is locked.

      Do not list timer-driven oneshot jobs (backups) -- the wantedBy edge
      would run them on every unlock; give those
      ConditionPathIsMountPoint only. A service consuming a subvolume through
      a dedicated mount unit binds to that mount unit instead -- see the
      mounting-subvolumes guide.
    '';
  };

  config = lib.mkIf cfg.enable {
    assertions =
      (map (name: {
        assertion = isBareServiceName name;
        message = "braid.poolBoundServices: '${name}' must be a bare systemd service name as keyed in systemd.services.<name> -- write \"samba-smbd\", not \"samba-smbd.service\".";
      }) cfg.poolBoundServices)
      ++ (map (name: {
        assertion = name != "braid-online";
        message = "braid.poolBoundServices: 'braid-online' is the lifecycle unit itself and cannot be its own consumer.";
      }) cfg.poolBoundServices)
      ++ (map (name: {
        assertion = definedElsewhere name;
        message = "braid.poolBoundServices: no other NixOS module defines systemd.services.${name} -- probably a typo, or the service is not enabled. If the unit ships only via systemd.packages, acknowledge it with `systemd.services.${name} = { };`.";
      }) validNames);

    systemd.services = lib.genAttrs validNames (_: {
      wantedBy = [ "braid-online.service" ];
      bindsTo = [ "braid-online.service" ];
      after = [ "braid-online.service" ];
      unitConfig.ConditionPathIsMountPoint = cfg.mountPoint;
    });
  };
}
```

Invalid names are filtered out of `validNames` so a suffixed mistake never
materializes a phantom unit while its assertion is being reported, and
`definedElsewhere` only runs on names that passed the bare-name check.

### 2. Edit: `modules/braid/default.nix`

Add `./pool-bound-services.nix` to the imports list.

### 3. New VM test: `tests/module/pool-bound-services.{nix,py}`

Model on `tests/module/lock-stops-bound-consumers.nix` (same
`tests/module/lib/initrd-fixture.nix` 2-disk fixture, same `pool.json`
tmpfiles seed, same `braid-unlock.script` mkForce override, passphrase
`testpassphrase`). Match sibling preamble styles exactly: `.nix` gets the
short `What:`/`Why:` header; `.py` gets `# Test: pool-bound-services` +
`Intent:` / `Why it exists:` / `Scenario:` paragraphs.

Leave `lock-stops-bound-consumers` untouched -- it pins the raw hand-written
contract (still the documented escape hatch); the new test pins the option's
expansion of it.

Node config essentials:

- `braid.poolBoundServices = [ "dummy-pool-consumer" ];`
- The dummy service carries **no hand-written triad and no condition** -- only
  `wantedBy = [ "multi-user.target" ]` (the boot edge real consumers have;
  load-bearing for proving the condition skip and additive merge) and a
  `Type=simple` ExecStart that opens fd 3 under `/mnt/storage` then sleeps
  (copy `dummy-pool-consumer` from the sibling test, minus the triad).

Test script (reuse the sibling's `unlock()` / `assert_consumer_holds_mount()`
helper shapes; mind the driver's auto-prepended `set -euo pipefail` and the
no-placeholder-f-string lint -- use `.format()`):

1. Boot: `wait_for_unit("multi-user.target")`; consumer inactive
   (`machine.fail("systemctl is-active dummy-pool-consumer.service")`),
   `/mnt/storage` not a mountpoint -- condition-skipped despite its boot edge.
2. Stamp pinned at the unit level: `systemctl show -P WantedBy
   dummy-pool-consumer.service` contains both `multi-user.target` and
   `braid-online.service` (additive merge), and `systemctl show -P After
   dummy-pool-consumer.service` contains `braid-online.service`. The
   `After` check is load-bearing: `BindsTo` implies no ordering, so
   without it an implementation that drops `after` would still pass every
   behavioral subtest (ADR 018's BindsTo-without-After caveat).
3. Unlock starts it: `unlock(machine)`; `braid-online` active;
   `wait_until_succeeds` is-active on the consumer; holds the mount.
4. `systemctl show -P BoundBy braid-online.service` lists
   `dummy-pool-consumer.service`.
5. `braid lock` succeeds while the consumer holds fd 3 open (stop-before-
   unmount); afterwards consumer inactive, mount gone, mappers gone
   (`machine.fail("test -e /dev/mapper/braid-disk1")` -- not
   `cryptsetup status`, pipefail gotcha).
6. Manual `systemctl start` while locked returns 0 but unit stays inactive
   (condition gate).
7. Second `unlock(machine)` restarts it (wantedBy propagation); holds mount.
8. `machine.shutdown()`.

### 4. New eval test: `tests/eval/pool-bound-services-assertion-fails.nix`

Follow `tests/eval/mountpoint-assertion-fails.nix` verbatim in shape
(tryEval-filter `config.assertions` for exact expected messages, wrap in
`pkgs.runCommand`). Three cases:

- `poolBoundServices = [ "samba-smbd.service" ]` -> bare-name message;
- `poolBoundServices = [ "no-such-service-xyz" ]` -> definedElsewhere message
  (also behavior-locks that `options.systemd.services.definitions` evaluates
  without recursion inside a full nixosSystem);
- `poolBoundServices = [ "braid-online" ]` -> self-dependency message (pins the
  guard against stamping `bindsTo = [ "braid-online.service" ]` onto the
  lifecycle unit itself).

### 4b. New eval test: `tests/eval/pool-bound-services-assertion-ok.nix`

Follow `tests/eval/mountpoint-assertion-ok.nix` in shape (force
`system.build.toplevel`, which enforces assertions at eval). One case:
`poolBoundServices = [ "stub-consumer" ]` with an extra module defining only the
documented acknowledgment `systemd.services.stub-consumer = { };` -- an
ExecStart-less, package-backed-style definition. This pins the
definitions-count contract directly: the VM test's dummy service has a
Nix-visible `ExecStart`, so it cannot catch a regression to an
`ExecStart`-based existence check that would falsely reject package-backed or
acknowledged services (e.g. `nfs-server`).

### 5. Edit: `tests/eval/_braid-eval-harness.nix`

Add passthrough param `poolBoundServices ? [ ],` and `inherit ... poolBoundServices;`
in the `braid = { ... }` block, plus `extraModules ? [ ]` appended to the
`modules` list (needed by the acknowledgment case in 4b). Existing callers
unaffected.

### 6. Edit: `flake.nix` (two registrations)

- Next to `lock-stops-bound-consumers` (~the module-test block):
  `pool-bound-services = pkgs.testers.nixosTest (import ./tests/module/pool-bound-services.nix { braid = linuxCrane.braid-cli-unwrapped; });`
- Next to `eval-mountpoint-rejects-bad-chars` (~the eval-check block):
  `eval-pool-bound-services-rejects-bad-names = import ./tests/eval/pool-bound-services-assertion-fails.nix { inherit pkgs linuxPkgs nixpkgs linuxSystem; };`
  and
  `eval-pool-bound-services-accepts-acknowledged = import ./tests/eval/pool-bound-services-assertion-ok.nix { inherit pkgs linuxPkgs nixpkgs linuxSystem; };`

### 7. Docs edits (each minimal)

- `docs/design/decisions/018-systemd-lifecycle.md#consumer-dependency-contracts`:
  1-2 sentences in the long-running paragraph -- the module ships
  `braid.poolBoundServices`, which stamps exactly this contract per listed
  service (canonical path); the hand-written triad stays documented as the
  expansion and escape hatch (non-service units, units invisible to the
  existence assertion). `## See`: add code-span bullets for
  `modules/braid/pool-bound-services.nix` and `tests/module/pool-bound-services.py`
  (paths must exist on disk -- `scripts/docs/check-see-paths.py`; no line
  numbers).
- `docs/guides/sharing-and-permissions.md#binding-shares-to-the-pool-lifecycle`:
  lead with `braid.poolBoundServices = [ "samba-smbd" ];`; keep the existing
  4-field snippet reframed as "what this stamps" / manual expansion; keep the
  four-bullet field explanation and the leave-samba.target-alone guidance.
  NFS section: one-liner `braid.poolBoundServices = [ "nfs-server" ];`.
- `docs/guides/nixos-configuration.md`: Core table row
  (`braid.poolBoundServices | list of strings | [] | ...`) + line in the Full
  config example.
- `docs/guides/troubleshooting.md#smbnfs-service-inactive-after-braid-lock`:
  name `braid.poolBoundServices` as the fix that wires both sides.
- `docs/commands/lock.md` + `docs/commands/unlock.md`: one clause each in the
  consumer prose mentioning the option.
- `docs/guides/mounting-subvolumes.md`: one clarifying sentence -- subvolume
  mount units and their consumers are NOT `poolBoundServices` entries; they bind
  to the mount unit (Jellyfin example unchanged).
- `README.md`: one commented line in the `configuration.nix` example (~the
  `braid = { ... }` block):
  `# poolBoundServices = [ "samba-smbd" ];  # start/stop services with the pool lifecycle`.
- No `docs/SUMMARY.md` change (no new pages).

Module option description and all output are ASCII (`--`, straight quotes).

## Work ordering (TDD)

1. Write `tests/module/pool-bound-services.{nix,py}` + flake registration. Run
   `just test-vm pool-bound-services` -> eval fails: option does not exist
   (confirms wiring, not yet the behavioral failure).
2. Add `pool-bound-services.nix` with the **option declaration only** (no config
   stamp) + `default.nix` import. Rerun -> test boots, passes subtest 1
   vacuously, **fails at "unlock starts it" timeout** -- the right-reason
   behavioral failure.
3. Add the `systemd.services = lib.genAttrs ...` stamp -> VM test passes.
4. Write `tests/eval/pool-bound-services-assertion-fails.nix` +
   `tests/eval/pool-bound-services-assertion-ok.nix` + harness params + flake
   registrations. Run
   `just test-vm eval-pool-bound-services-rejects-bad-names eval-pool-bound-services-accepts-acknowledged`
   -> the fails-test fails ("did not reject"); the ok-test fails too if the
   acknowledgment path is broken. Add the assertions block -> both pass.
   Rerun the VM test (assertions must not break the positive path).
5. Docs edits.
6. Full verification below.

## Verification

- `just fmt-nix`
- `just test-vm pool-bound-services eval-pool-bound-services-rejects-bad-names eval-pool-bound-services-accepts-acknowledged lock-stops-bound-consumers`
- Blast radius: `just test-vm systemd-lifecycle subvol-mount-lifecycle eval-mountpoint-accepts-valid eval-lock-systemd-stop-deadline-ok`
- `just docs-build` (mdbook-linkcheck2), `just check-docs`,
  `just check-docs-see-paths` (ADR 018 See edit), `just check-doc-links`
  (README edit), `just check-line-cites`
- No Rust changes -> no fixture/parser lanes.
- Full `just test-vm` before handoff.

## Risks / verified gotchas

- **Verified in pinned nixpkgs:** list-typed unit options merge additively;
  `unitConfig` keys union with loud conflict on differing scalars;
  `script`-derived ExecStart IS config-visible but package-shipped units
  (nfs-server) have none -- hence the definitions-count assertion.
- **Accepted imprecision:** a `mkIf false`-disabled service passes
  `definedElsewhere` (benign: condition-gated skeleton never starts); units
  existing only in `systemd.packages` need the documented
  `systemd.services.<name> = { };` acknowledgment.
- **Watch item:** exotic `mkMerge`/`mkIf` shapes at the `systemd.services`
  definition level across all nixpkgs modules could surprise
  `options.systemd.services.definitions`; both new tests exercise it end-to-end
  at step 4, before docs work.

## Implementation notes

- The VM fixture preloads btrfs in the initrd and seeds a file that the dummy
  consumer opens read-only, so the lock path exercises a real pool-held
  descriptor without leaving fixture format mappers open.
- The service-existence assertion counts raw
  `options.systemd.services.definitions`; braid's generated stamp contributes
  one definition, so a listed service must have one additional definition from
  another module or an explicit empty acknowledgment.
