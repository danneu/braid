# Make systemd lifecycle explicit in runtime config + rename `storageGroup` to `poolAccessGroup`

## Context

The `just test-all` failure surfaces in `tests/cli/braid-add-enroll.py`
Test 4c (lines 224-258): after `braid add` succeeds on a CLI-only VM
that installs only the standalone package and writes
`{ "mount_point": "/mnt/storage" }` (`tests/cli/braid-add-enroll.nix:42-44`),
stderr now carries `braid: WARNING: failed to activate
braid-online.service -- pool is mounted but shutdown may not lock
automatically`. The strict assertion `"WARNING:" not in err`
(`tests/cli/braid-add-enroll.py:255-258`) fails.

The regression is from `ff6f766` (move pool lock ownership into Rust
dispatch). Before the migration, `braid-online.service` activation
lived in the NixOS module wrapper, so it only ran in module-managed
installs where the unit existed. After the migration, Rust dispatch
unconditionally calls `mark_online` after `unlock`, `add`, and
`recover`, and `mark_offline` after plain `lock` -- for every
non-dry-run config, including CLI-only configs that never installed
the unit. `mark_online` snapshots the unit, sees `inactive` (because
`systemctl show -P ActiveState` on a non-existent unit returns
"inactive"), tries to `systemctl start braid-online.service`, fails,
and emits the legacy `WARNING:` line.

A natural-looking fix is to probe whether `braid-online.service`
exists before trying to start it. That works for this test but is a
poor ownership signal in general: the unit could be `not-found`,
`masked`, `bad-setting`, stale from an old generation, or
hand-written. The current `systemctl show -P ActiveState` probe
already collapses non-existent units to "inactive" -- which is
exactly how this warning leaked into CLI-only tests. More
importantly, on a module-managed install where the operator masked
or accidentally removed the unit, "silently skip" is the wrong
response: that is a broken safety path and should be visible.

The right rule is: **config decides capabilities; systemd probes
verify/synchronize an enabled capability.** If the module declares
lifecycle ownership, Rust dispatch synchronizes it (and a missing
unit is a real failure worth warning about). If the runtime config
does not declare lifecycle ownership, Rust dispatch should not spawn
`systemctl` for lifecycle work at all. So the fix is to make
lifecycle ownership an explicit runtime-config capability emitted
by the NixOS module. Standalone CLI configs omit it and therefore
never touch `braid-online.service`.

## Approach

Two coupled changes shipped together:

1. **Lifecycle flag.** Add a boolean `systemd_lifecycle` field to the
   runtime config. When `false` (default; CLI-only configs), Rust
   dispatch still runs mount permission fixups (mountpoint check +
   `pool_access_group` chown/chmod) but skips all
   `braid-online.service` (and scrub-unit / `BoundBy`) systemctl
   calls. When `true` (module-generated configs), behavior is
   unchanged from `ff6f766` -- Rust still owns lifecycle
   synchronization.

   A boolean rather than a struct because `braid-online.service` is a
   stable module integration contract -- other units
   (`braid-scrub.*`, samba, nfs) reference the name with
   `BindsTo=`/`After=`/`WantedBy=`. The runtime config only says
   "this install is module-managed"; renaming the unit would break
   the module's own dependency graph.
   `pub const BRAID_ONLINE_UNIT: &str = "braid-online.service"`
   (`cli/src/online_state.rs:16`) remains the production unit name.

   `--systemd-stop` does not call `mark_offline` today
   (`cli/src/main.rs:1012-1064`) and stays unchanged; its caller is
   `braid-online.service` itself, so lifecycle is by definition
   enabled in that path.

2. **Rename `storage_group` -> `pool_access_group`.** The current
   name reads as "the storage's group" -- ambiguous with the broader
   "storage" namespace (`storage.nix`, `braid-scrub`, etc.). The
   field's actual purpose is the Unix group granted access to the
   mount root via `root:<group> 2770`. Rename to clarify intent.
   Surfaces:
   - Nix option: `braid.storageGroup` -> `braid.poolAccessGroup`
   - JSON field: `storage_group` -> `pool_access_group`
   - Rust accessor: `Config::storage_group()` ->
     `Config::pool_access_group()`
   - Rust field: `storage_group: Option<String>` ->
     `pool_access_group: Option<String>`

   The default group **name on disk stays `"storage"`** -- changing
   the on-disk group would force users to chown files and reconfigure
   `extraGroups`. The option's *label* is what changes.

   Per AGENTS.md ("No backwards compatibility"), no aliases or
   deprecation shims. Both names move in lockstep.

## Rust changes

### `cli/src/config.rs` -- rename `storage_group` and add lifecycle flag

Rename `storage_group` to `pool_access_group` in `Config` (lines 41,
53, 64-65), `RawConfig` (line 101), and the `TryFrom<RawConfig>` impl
(line 113). The accessor becomes:

```rust
/// Optional Unix group that receives write access on the mounted pool
/// root via `root:<group> 2770`.
pub fn pool_access_group(&self) -> Option<&str> {
    self.pool_access_group.as_deref()
}
```

Update the existing parser tests (lines 154-165) to use the new key
name -- the test asserting `"storage_group":"storage"` becomes
`"pool_access_group":"storage"` (note: the *value* `"storage"` is the
Unix group name and stays the same).

Add the lifecycle flag alongside:

```rust
// Config:
pool_access_group: Option<String>,
systemd_lifecycle: bool,

// RawConfig:
#[serde(default)]
pool_access_group: Option<String>,
#[serde(default)]
systemd_lifecycle: bool,
```

`Config::new` initializes both: `pool_access_group: None,
systemd_lifecycle: false`. Add accessor:

```rust
/// True when the runtime is module-managed and Rust dispatch should
/// activate/deactivate `braid-online.service` and stop lifecycle-bound
/// units. Emitted by `modules/braid/cli.nix`; absent in standalone CLI
/// configs.
pub fn systemd_lifecycle(&self) -> bool {
    self.systemd_lifecycle
}
```

Add `#[serde(deny_unknown_fields)]` to `RawConfig` (currently lines
97-106 has only `#[derive(Deserialize)]`). Today an unknown
top-level key in `config.json` silently parses to the default --
hiding a stale rename, a typo, or a hand-edit. With
`deny_unknown_fields`, the renamed `storage_group` key in a stale
config now fails fast with a precise error, which is the right UX
for a runtime that is config-driven. This matches the
`Ups`/`SystemdLifecycle`-style strictness already used elsewhere
in the file and pairs naturally with the rename: an operator who
upgrades and forgets to migrate their JSON gets a clear parse
error instead of silent fallback to defaults.

### `cli/src/online_state.rs` -- gate systemctl calls on the flag

`snapshot` stays unchanged (still reads `BRAID_ONLINE_UNIT`). Two
behavioral changes:

1. `mark_online` (lines 233-286): take
   `snap: Option<&OnlineSnapshot>` instead of `&OnlineSnapshot`. Run
   the mountpoint check and `pool_access_group` chown/chmod (lines
   238-263, renamed from `storage_group`) unconditionally as today.
   Gate the unit-state match (lines 265-283) on both
   `cfg.systemd_lifecycle()` being true and `snap.is_some()`:

   ```rust
   pub fn mark_online(
       snap: Option<&OnlineSnapshot>,
       cfg: &Config,
       ops: &dyn OnlineStateOps,
   ) -> Result<(), OnlineError> {
       let mount_point = Path::new(cfg.mount_point().as_str());
       let mounted = match ops.is_mountpoint(mount_point) { /* unchanged */ };
       if !mounted { return Ok(()); }

       if let Some(group) = cfg.pool_access_group() {
           // chown + chmod unchanged
       }

       if cfg.systemd_lifecycle() {
           if let Some(snap) = snap {
               match &snap.online_state { /* unchanged body, still uses BRAID_ONLINE_UNIT */ }
           }
       }

       Ok(())
   }
   ```

   This shape preserves the "pool_access_group fixups even without
   lifecycle" invariant the planner feedback called out. Dispatch
   calls `mark_online` after non-dry-run mount-producing commands; the
   Option lets the caller omit the snapshot when lifecycle is disabled.

   The accessor call at `cli/src/online_state.rs:250` is the only
   production call site of `storage_group()` and changes to
   `pool_access_group()`.

2. `mark_offline` (lines 288-305): change signature to
   `mark_offline(cfg: &Config, ops: &dyn OnlineStateOps)`. Mountpoint
   short-circuit (lines 290-299) stays. Gate the `systemctl_stop`
   call (line 301) on `cfg.systemd_lifecycle()`:

   ```rust
   pub fn mark_offline(cfg: &Config, ops: &dyn OnlineStateOps) -> Result<(), OnlineError> {
       let path = Path::new(cfg.mount_point().as_str());
       match ops.is_mountpoint(path) { /* unchanged */ }
       if cfg.systemd_lifecycle() {
           if let Err(e) = ops.systemctl_stop(BRAID_ONLINE_UNIT, false) {
               eprintln!("braid: WARNING: failed to deactivate braid-online.service: {e}");
           }
       }
       Ok(())
   }
   ```

### `cli/src/doctor.rs` -- gate the UPS safety detector

`check_braid_online_active_when_mounted` (cli/src/doctor.rs:823-862)
runs `systemctl show -P ActiveState braid-online.service` whenever
config is available, `ups` is configured, and the pool is mounted.
With the new lifecycle gate, a CLI-only-but-UPS-configured install
would probe a unit it never owned and report `Fail`. The check
needs an explicit lifecycle gate.

Add the skip branch **after** the existing UPS check (line 830-832),
before the mountpoint probe (line 833):

```rust
if config.ups().is_none() {
    return CheckResult::skip(name, "skipped (braid.ups not enabled)");
}
if !config.systemd_lifecycle() {
    return CheckResult::skip(
        name,
        "skipped (systemd_lifecycle not configured -- braid-online is not Rust-managed)",
    );
}
let mount_point = config.mount_point().clone();
// ... existing mountpoint probe ...
```

Final gate order: **config available -> UPS configured -> lifecycle
enabled -> pool mounted -> systemctl probe.** Placing the lifecycle
gate after UPS preserves the most-informative skip message for the
common no-UPS case ("braid.ups not enabled") and only surfaces the
lifecycle reason when UPS is actually in play -- which is the only
situation where the missing lifecycle materially changes the safety
story.

This is consistent with ADR 020's revised wording: the UPS-safety
detector only fires on module-managed installs, which are the only
deployments that own `braid-online.service`. The skip text explains
*why* the check is inert for a reader looking at `braid doctor`
output on a UPS-configured-but-standalone host.

### `cli/src/lock.rs` -- gate `run_lock_pre_steps`

`run_lock_pre_steps` (lines 995-1029) stops scrub units and iterates
`BoundBy braid-online.service`. In CLI-only mode it currently
silently fails but still spawns `systemctl` subprocesses. Gate the
whole function on the lifecycle flag:

```rust
fn run_lock_pre_steps(cfg: &Config, online_ops: &dyn OnlineStateOps) {
    if !cfg.systemd_lifecycle() {
        return;
    }
    for unit in [
        "braid-scrub.timer",
        "braid-scrub-resume-trigger.service",
        "braid-scrub.service",
    ] {
        let _ = online_ops.systemctl_stop(unit, false);
    }
    let Ok(bound_by) = online_ops.list_bound_by("braid-online.service") else {
        return;
    };
    // rest unchanged
}
```

Update the single call site in `cmd_lock_impl` (cli/src/lock.rs:984)
to pass `config`.

### `cli/src/main.rs` -- thread snapshot/config through dispatch

For each call site that uses `mark_online`/`mark_offline`:

- `Commands::Add` (lines 400-442): reorder so `online_config`
  (line 409-410) is loaded before computing the snapshot. Gate the
  snapshot on `cfg.systemd_lifecycle()`:

  ```rust
  let online_config = (!args.common.dry_run).then(|| load_config_or_exit(...));
  let online_snapshot = online_config
      .as_ref()
      .filter(|cfg| cfg.systemd_lifecycle())
      .map(|_| snapshot(&online_ops));
  // ...
  if let Some(cfg) = online_config.as_ref() {
      let _ = mark_online(online_snapshot.as_ref(), cfg, &online_ops);
  }
  ```

  The post-cmd call is now gated on `cfg` alone (not the
  snapshot-and-config pair), so storage_group fixups run even when
  lifecycle is disabled.

- `Commands::Unlock` (lines 573-612): `config` is already loaded
  unconditionally. Compute snapshot once config is known:

  ```rust
  let online_snapshot = (!args.dry_run && config.systemd_lifecycle())
      .then(|| snapshot(&online_ops));
  // ... after Ok branch:
  if !args.dry_run {
      let _ = mark_online(online_snapshot.as_ref(), &config, &online_ops);
  }
  ```

- `Commands::Recover` (lines 855-901): same pattern as Unlock.

- `run_plain_lock` (lines 977-1010): change line 1009 from
  `mark_offline(config.mount_point(), &online_ops)` to
  `mark_offline(&config, &online_ops)`. No snapshot needed.

- `run_systemd_stop_lock` (lines 1012-1064): no `mark_offline` call
  today; `cmd_lock` already threads `&config`, so the internal
  `run_lock_pre_steps` change is transparent.

## Module changes

### `modules/braid/options.nix` -- rename `storageGroup` to `poolAccessGroup`

Rename the option (line 35), the assertion (lines 96-97), and the
`users.groups` lookup (lines 117-118):

```nix
# Replace lines 35-39:
poolAccessGroup = lib.mkOption {
  type = lib.types.nullOr lib.types.str;
  default = "storage";
  description = "Unix group granted access to the mount root. Sets root:<group> 2770 on the mount root after mount-producing commands (unlock, add). Set to null to disable.";
};

# Lines 96-97 -> use cfg.poolAccessGroup and "braid.poolAccessGroup":
{
  assertion = cfg.poolAccessGroup == null || builtins.match "[a-z_][a-z0-9_-]*" cfg.poolAccessGroup != null;
  message = "braid.poolAccessGroup '${toString cfg.poolAccessGroup}' is not a valid Unix group name.";
}

# Lines 117-118:
users.groups = lib.mkIf (cfg.poolAccessGroup != null) {
  ${cfg.poolAccessGroup} = { };
};
```

The default value `"storage"` (the Unix group name on disk) is
preserved so existing installs continue to chown to `root:storage
2770`.

### `modules/braid/cli.nix` -- rename JSON field and emit `systemd_lifecycle = true`

Update the generated JSON (cli.nix:13-36):

```nix
configFile = (pkgs.formats.json { }).generate "braid-config.json" (
  {
    mount_point = cfg.mountPoint;
    pool_access_group = cfg.poolAccessGroup;
    systemd_lifecycle = true;
  }
  // lib.optionalAttrs cfg.fanControl.enable { ... }
  // lib.optionalAttrs cfg.ups.enable { ... }
);
```

`systemd_lifecycle = true` is unconditional inside `lib.mkIf
cfg.enable` -- enabling the module defines `braid-online.service` in
`modules/braid/storage.nix:130-147`, so lifecycle is by construction
available.

### `modules/braid/storage.nix` -- update the comment

Line 40 has a comment `# Permissions are set by Rust post-unlock
lifecycle fixups (root:storageGroup 2770).` -- change to
`root:poolAccessGroup 2770`.

No changes needed to `modules/braid/wrapper.nix` or
`modules/braid/braid-wrapper.sh`.

## Test changes

### Rust unit tests

Update `cli/src/config.rs::tests`:

- Existing `parses_valid_config` (lines 152-158): switch the JSON key
  `"storage_group"` to `"pool_access_group"` and the accessor call
  from `cfg.storage_group()` to `cfg.pool_access_group()`.

Add to `cli/src/config.rs::tests`:

- `parses_config_without_systemd_lifecycle_defaults_false` -- input
  `{"mount_point":"/mnt/storage"}` parses; `systemd_lifecycle()`
  returns `false`.
- `parses_config_with_systemd_lifecycle_true` -- input has
  `"systemd_lifecycle": true`; getter returns `true`.
- `rejects_systemd_lifecycle_non_boolean` -- input
  `"systemd_lifecycle": "yes"` or `42` fails parse.
- `rejects_config_with_unknown_top_level_field` -- input
  `{"mount_point":"/mnt/storage","storage_group":"storage"}`
  (the renamed field) fails parse with an "unknown field" error.
  This is the regression test for the
  `#[serde(deny_unknown_fields)]` addition and the rename
  migration: an operator on a stale JSON gets a clear parse
  error instead of silent fallback. Mirrors the existing
  `rejects_config_with_legacy_ups_enable_field` test (line 275).

Update stale tests that used removed top-level config fields:

- `cli/src/doctor.rs::tests::valid_json_with_extra_fields_parses_ok`
  becomes a schema-failure test for an old `disks` key.
- The two replace dry-run preview tests stop building a `Config` value
  with a removed `disks` key; membership belongs in `pool.json`, not
  runtime config.

Add to `cli/src/online_state.rs::tests`:

- `mark_online_skips_systemctl_when_lifecycle_disabled` -- Config
  with no `systemd_lifecycle` (false by default), no
  `pool_access_group`; assert recorded calls contain `mountpoint`
  but not `start ...`.
- `mark_online_applies_pool_access_group_without_lifecycle` --
  Config with `pool_access_group = "storage"` and `systemd_lifecycle
  = false`; assert calls contain `chown` and `chmod` but not
  `start ...`. This is the direct regression test for the planner
  feedback's high-severity gap.
- `mark_online_starts_when_lifecycle_enabled` -- rename + adjust
  existing `mark_online_starts_only_for_inactive_or_failed` (line
  429) to construct a Config with `systemd_lifecycle = true` and
  pass `Some(&snap)`; assert it still starts only for
  `Inactive`/`Failed`.
- `mark_online_skips_systemctl_when_snapshot_absent` -- Config with
  `systemd_lifecycle = true` but `snap = None`; assert no `start`
  call. (Belt-and-braces guard against future callers forgetting to
  snapshot.)
- `mark_offline_skips_systemctl_when_lifecycle_disabled` -- Config
  with `systemd_lifecycle = false`, mountpoint reports unmounted;
  assert no `stop ...` call.
- `mark_offline_stops_when_lifecycle_enabled` -- adjust existing
  `mark_offline_uses_synchronous_stop_when_unmounted` (line 457) to
  construct a Config with `systemd_lifecycle = true`.

Update `cli/src/test_fixtures/doctor.rs::config_with_ups_enabled`
(lines 58-60): change the JSON string from

```rust
r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"}}"#
```

to

```rust
r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"},"systemd_lifecycle":true}"#
```

Without this update, every existing
`check_braid_online_active_when_mounted_*` test
(cli/src/doctor.rs:3286-3556, ten tests) would now skip with
"systemd_lifecycle not configured" instead of exercising the probe
logic they cover. The fixture's purpose is "UPS is configured and
this is a module-managed install" -- that's exactly the
deployment shape that wants both fields set together.

Add to `cli/src/doctor.rs::tests`:

- `braid_online_check_skips_when_lifecycle_disabled`
  -- DoctorContext with an **inline** config (not the shared
  fixture) of
  `r#"{"mount_point":"/mnt/storage","ups":{"name":"ups"}}"#`. UPS
  configured, `systemd_lifecycle` defaults to false, pool mounted.
  Assert the result is `CheckResult::skip(...)` with the new
  lifecycle skip reason, and assert the MockRunner recorded no
  `SystemctlShowActiveState` request. Using a separate inline config
  (rather than reusing `config_with_ups_enabled()`) keeps the
  fixture's contract clean: that fixture means "module-managed +
  UPS" and is shared across the existing tests; the new test
  exercises the inverse and should not import that name.

Add to `cli/src/lock.rs::tests` (planner-feedback medium finding,
covering the `run_lock_pre_steps` gate):

- `cmd_lock_skips_lifecycle_pre_steps_when_lifecycle_disabled` --
  build a Config with `systemd_lifecycle = false`, run
  `cmd_lock_impl` against a `MockRunner`, assert no `CmdRequest`
  matching `SystemctlStop` for `braid-scrub.timer`,
  `braid-scrub-resume-trigger.service`, or `braid-scrub.service`,
  and no `SystemctlShowBoundBy` for `braid-online.service`.
- `cmd_lock_runs_lifecycle_pre_steps_when_lifecycle_enabled` --
  same harness with `systemd_lifecycle = true`; assert the same
  three `SystemctlStop` calls and the `SystemctlShowBoundBy` call
  do appear.

The existing `mark_offline_uses_synchronous_stop_when_unmounted` and
`mark_online_starts_only_for_inactive_or_failed` tests in
`online_state.rs` need their Config construction updated to set
`systemd_lifecycle = true` to keep their original intent.

### VM tests

- `just test-vm braid-add-enroll`: Test 4c
  (`tests/cli/braid-add-enroll.py:224-258`) passes; stderr contains
  the canonical `[warn]` block and no `WARNING:` line.
- `just test-vm systemd-lifecycle braid-module-add-bootstrap braid-module-single-disk`: still
  pass; module-generated config includes `systemd_lifecycle = true`
  so activate/deactivate of `braid-online.service` and `root:storage
  2770` fixups continue to work.

No changes needed to any `tests/cli/*.nix` files (they all install
the bare `{ mount_point = ... }` config and inherit
`systemd_lifecycle = false` automatically via serde's `default`).

No changes needed to any `tests/module/*.nix` files (they enable the
module which emits `systemd_lifecycle = true`).

## Doc updates

### Lifecycle-fix docs

- [`docs/decisions/026-pool-lock-rust-owned.md`](../../docs/decisions/026-pool-lock-rust-owned.md)
  (Active): update the "Post-success lifecycle work also lives under
  the Rust-held pool lock" section (lines 52-57) to clarify that
  lifecycle work runs only when `systemd_lifecycle` is true in
  runtime config. Note that pool-lock acquisition and
  `pool_access_group` fixups are unaffected by the lifecycle gate.

- [`docs/decisions/018-systemd-lifecycle.md`](../../docs/decisions/018-systemd-lifecycle.md)
  (Active): update the "Rust dispatch as synchronization layer"
  bullet list (lines 119-125) so step 5 ("Rust starts
  `braid-online.service`...") and the offline-stop counterpart only
  apply when `systemd_lifecycle` is configured. Add one sentence
  noting `modules/braid/cli.nix` emits `systemd_lifecycle = true`
  while standalone CLI deployments omit it. Update line 123:
  `root:storageGroup 2770` -> `root:poolAccessGroup 2770`,
  `storageGroup is configured` -> `poolAccessGroup is configured`.

- [`docs/decisions/017-runtime-disk-membership.md`](../../docs/decisions/017-runtime-disk-membership.md)
  (Active): two threads:
  - Line 34: update the `/etc/braid/config.json` example to show
    the minimal standalone shape stays
    `{ "mount_point": "/mnt/storage" }`, while module-generated
    configs also include `pool_access_group` and
    `systemd_lifecycle: true`.
  - Line 85: rewrite "Started by the wrapper after a successful
    `unlock` or `add` that leaves the pool mounted." to "Started by
    Rust dispatch via `mark_online`
    (`cli/src/online_state.rs`) after a successful `unlock`, `add`,
    or `recover` that leaves the pool mounted, gated on
    `systemd_lifecycle = true` in the runtime config."

- [`docs/decisions/019-inhibit-sleep.md`](../../docs/decisions/019-inhibit-sleep.md)
  (Active): line 151 attributes `systemctl stop
  braid-online.service` to the wrapper -- rewrite to attribute it to
  Rust dispatch's post-lock `mark_offline`
  (`cli/src/online_state.rs`), gated on `systemd_lifecycle`.

- [`docs/decisions/020-ups-integration.md`](../../docs/decisions/020-ups-integration.md)
  (Active): two threads in the "`braid-online` becomes
  safety-critical under UPS" section:
  - Line 85: rewrite "`modules/braid/braid-wrapper.sh` currently
    warns and exits successfully when `systemctl start
    braid-online.service` fails..." to attribute the warn-and-exit
    behavior to `mark_online` (`cli/src/online_state.rs`).
  - Line 87: rewrite "The wrapper's warn-and-continue behavior
    otherwise remains unchanged" to refer to `mark_online`. Add one
    sentence noting that under `systemd_lifecycle = false`
    (CLI-only), the lifecycle path is skipped entirely -- so the
    UPS-safety detector in `braid doctor` should only fire when
    `systemd_lifecycle = true` *and* `braid.ups.enable = true`
    (i.e. on module-managed systems, which are the only UPS
    deployments anyway).

### Rename-only docs

- [`docs/decisions/013-mount-permissions.md`](../../docs/decisions/013-mount-permissions.md)
  (Active): two threads:
  - Rewrite "Why wrapper-based fixup" (lines 24-33) -- stale after
    `ff6f766`. Rust now executes the chown/chmod from `mark_online`
    (cli/src/online_state.rs:250-263) based on the module-emitted
    `pool_access_group` field. The wrapper is a pure exec shim.
  - Line 39: `braid.storageGroup` -> `braid.poolAccessGroup`.
    Line 54: same.

- [`docs/decisions/005-sane-defaults.md`](../../docs/decisions/005-sane-defaults.md)
  (Active): defaults table line 41: column key
  `braid.storageGroup` -> `braid.poolAccessGroup`. Default value
  `"storage"` stays.

- [`docs/principles.md`](../../docs/principles.md): line 47 mentions
  `storageGroup` in passing as an example -> `poolAccessGroup`.

- [`docs/testing.md`](../../docs/testing.md): line 80 mentions
  `storageGroup = null` as an example test off-value ->
  `poolAccessGroup = null`.

- [`manual/guides/nixos-configuration.md`](../../manual/guides/nixos-configuration.md):
  primary option reference guide. Line 62 (option table row):
  `braid.storageGroup` -> `braid.poolAccessGroup`. Line 168 (full
  config example): `storageGroup = "storage";` ->
  `poolAccessGroup = "storage";` (value unchanged).

- [`manual/guides/sharing-and-permissions.md`](../../manual/guides/sharing-and-permissions.md):
  user-facing guide referencing `config.braid.storageGroup` and
  `braid.storageGroup` in nine places (lines 30, 35, 39, 44, 52, 94,
  129, 140, 166). Rename all to the new option name.

No updates needed to ADR 022 -- its dry-run/preview model is
unaffected by the lifecycle gate (config loads before the dry-run
fork in all migrated commands).

## Critical files

Lifecycle + rename:

- `cli/src/config.rs` -- rename `storage_group` field/accessor to
  `pool_access_group`; add `systemd_lifecycle` bool field + getter;
  update parser tests.
- `cli/src/online_state.rs` -- `mark_online` takes
  `Option<&OnlineSnapshot>`; `mark_offline` takes `&Config`; both
  gate systemctl calls on `cfg.systemd_lifecycle()` while keeping
  `pool_access_group` fixups unconditional. Renamed accessor call
  at line 250.
- `cli/src/lock.rs` (lines 984 + 995-1029) -- `run_lock_pre_steps`
  takes `&Config` and short-circuits when lifecycle is disabled.
- `cli/src/doctor.rs` (lines 823-862) --
  `check_braid_online_active_when_mounted` skips when
  `!config.systemd_lifecycle()`, ordered after the existing UPS
  check.
- `cli/src/test_fixtures/doctor.rs` (lines 58-60) --
  `config_with_ups_enabled` fixture adds
  `"systemd_lifecycle": true` so existing doctor tests still
  exercise the probe path.
- `cli/src/main.rs` (lines 400-442, 573-612, 855-901, 977-1010) --
  reorder dispatch so config gates snapshot computation; always
  call `mark_online`; switch `mark_offline` to `&Config` signature.

Rename only (no behavioral change beyond the relabel):

- `modules/braid/options.nix` (lines 35-39, 96-97, 117-118) --
  `storageGroup` -> `poolAccessGroup` for the option, assertion, and
  `users.groups` lookup.
- `modules/braid/cli.nix` (line 16) -- emit `pool_access_group =
  cfg.poolAccessGroup;` (plus the new `systemd_lifecycle = true`
  line).
- `modules/braid/storage.nix` (line 40) -- update comment.
- `docs/decisions/013-mount-permissions.md`,
  `docs/decisions/005-sane-defaults.md`, `docs/principles.md`,
  `docs/testing.md`,
  `manual/guides/nixos-configuration.md`,
  `manual/guides/sharing-and-permissions.md` -- rename references.

Lifecycle-only doc updates:

- `docs/decisions/026-pool-lock-rust-owned.md`,
  `docs/decisions/018-systemd-lifecycle.md`,
  `docs/decisions/017-runtime-disk-membership.md`,
  `docs/decisions/019-inhibit-sleep.md`,
  `docs/decisions/020-ups-integration.md` -- describe the new
  lifecycle gate and rewrite stale wrapper-as-owner wording where
  it survives.

## Verification

1. `just test-rust` -- new + updated unit tests pass, including the
   `pool_access_group`-without-lifecycle and lock-pre-step
   regression tests.
2. `just test-vm braid-add-enroll` -- Test 4c
   (`tests/cli/braid-add-enroll.py:224`) passes; stderr contains the
   canonical `[warn]` block and no `WARNING:` line.
3. `just test-vm systemd-lifecycle braid-module-add-bootstrap braid-module-single-disk` --
   module-managed lifecycle still activates and deactivates
   `braid-online.service`, mount permissions still `root:storage
   2770` (the on-disk group name is unchanged).
4. `rg -n "storageGroup|storage_group" cli modules docs manual/guides
   tests | grep -v "rejects_config_with_unknown_top_level_field"`
   -- expected to be empty. The legacy-key string survives in
   exactly one place: the `rejects_config_with_unknown_top_level_field`
   test in `cli/src/config.rs`, which asserts the renamed key is
   rejected. Matches in `plans/impl/` and `plans/review/` are
   historical and stay (not in the grep scope).
5. `nix flake check` (via `just test-all`) -- module evaluation
   succeeds with the new `poolAccessGroup` option name; no
   reference to a removed `storageGroup` option remains.
6. `just test-all` -- full suite green.

## Assumptions

- Field is `systemd_lifecycle: bool`, default false via
  `#[serde(default)]`. The module-emitted form is
  `systemd_lifecycle = true;`.
- `BRAID_ONLINE_UNIT` constant in `cli/src/online_state.rs:16` stays
  the production unit name in both Rust (`mark_online` /
  `mark_offline` / `snapshot`) and tests. The unit name is part of
  the module's dependency graph and not user-configurable.
- The scrub unit names hard-coded in `run_lock_pre_steps`
  (`braid-scrub.timer`, etc.) stay hard-coded; they are part of the
  same module integration contract.
- No NixOS option is added to disable just `systemd_lifecycle` --
  enabling the module implies lifecycle. CLI-only deployments are
  the only path to omit it, and they do so by not installing the
  module at all.
- Rename touches **label only**: option is `poolAccessGroup`, JSON
  is `pool_access_group`, Rust accessor is `pool_access_group()`.
  The default Unix group name **on disk** stays `"storage"` so
  existing systems keep working without operator action.
- No compatibility shim, no `mkAliasOptionModule` to the old name:
  braid is unreleased (AGENTS.md "No backwards compatibility"). Any
  external NixOS config that referenced `braid.storageGroup` must be
  updated in lockstep with the bump.
- Historical references in `plans/impl/` and `plans/review/` are
  not rewritten -- they document past decisions and continue to
  reflect the names that were current when those plans landed.

## Rejected alternatives

- **Probe whether `braid-online.service` exists at runtime.**
  Rejected because absence can mean either standalone CLI mode or a
  broken module install; runtime config should declare whether
  lifecycle is managed.

- **Use `systemd_lifecycle = { online_unit = "braid-online.service" }`.**
  Rejected because `braid-online.service` is a stable module
  integration contract used by dependent units via `WantedBy=`,
  `BindsTo=`, and `After=`; the config should opt into lifecycle
  management, not rename the unit.

- **Fix only `tests/cli/braid-add-enroll.py` or weaken the
  `"WARNING:" not in err` assertion.** Rejected because the test
  exposed a real standalone-CLI behavior bug: Rust was trying to
  manage a module-owned unit that was not installed.

- **Keep `storageGroup` as the public option/config name.** Rejected
  because this plan already changes the runtime config surface, and
  `poolAccessGroup` more accurately describes the option's purpose:
  the Unix group granted access to the mounted pool root.
