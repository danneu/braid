# Pick a single MountPoint calling convention

## Context

`MountPoint` (cli/src/types.rs:18) is `pub struct MountPoint(pub String)` — a
public-field newtype, so anyone can fabricate one. It does **not** enforce
"this came from config validation"; that framing would require a private
field plus a checked constructor, which the type doesn't have. This refactor
does not change that, and does not pretend to.

What it does fix is two real, smaller problems:

1. **API consistency.** Two calling conventions coexist for the same value.
   Some helpers take `mount_point: &str` (cli/src/idle.rs:49,
   cli/src/monitor.rs:20, cli/src/ack.rs:10, cli/src/probe.rs:116, plus ~20
   more in status/pool/preflight/progress/replace/remove/remove_missing/tui),
   while others take `mount_point: &MountPoint` (cli/src/mount.rs:227,
   cli/src/remove.rs:228, cli/src/remove_missing.rs:264). New callers have no
   rule to follow.
2. **Pointless allocations.** Every `&str`-taking helper above immediately
   does `MountPoint(mount_point.to_owned())` to construct the `CmdRequest`
   field — usually multiple times per call (probe_pool does it twice). All
   six call sites in main.rs have a `&MountPoint` already and write
   `config.mount_point().as_str()` to feed the `&str`-taking signature, so
   the round-trip `&MountPoint → &str → owned String` is pure waste.

The dead `impl AsRef<str>` and `impl AsRef<Path>` for `MountPoint`
(cli/src/types.rs:50, :56) get removed in the same pass — confirmed unused
across the codebase.

Goal: **standardize config-derived mount-point call sites on `&MountPoint`
and delete the redundant re-wrapping.** Nothing more is claimed.

## Scope

### Functions to convert (`mount_point: &str` → `mount_point: &MountPoint`)

All of the following take `&str` and immediately wrap into a fresh
`MountPoint`. After the change, each function deletes its internal
`MountPoint(mount_point.to_owned())` and uses the parameter directly (cloning
only for owned `CmdRequest` fields, where a `.clone()` on the borrow
replaces the per-call allocation in only the cases that actually need an
owned value).

**probe / lifecycle:**
- cli/src/probe.rs:116 `probe_pool` (drops wraps at 121, 151; inline
  `mount_point.to_owned()` at 145 inside `ProbeError::NotBtrfs` becomes
  `mount_point.0.clone()`)
- cli/src/idle.rs:49 `cmd_idle`
- cli/src/idle.rs:93 `is_btrfs_mounted`
- cli/src/monitor.rs:20 `cmd_monitor`
- cli/src/ack.rs:10 `cmd_ack`

**status helpers (cli/src/status.rs):**
- :561 `summarize_df`
- :601 `get_capacity`
- :629 `get_device_stats`
- :644 `get_scrub_report`
- :680 `get_balance_report`
- :719 `paused_balance_warning` (also adjust the `format!` body — display via
  `mount_point` Display impl, which is identical output)
- :732 `emit_paused_balance_warning`

**pool helpers (cli/src/pool.rs):**
- :15 `pool_add_device`
- :36 `balance_error` (private helper; takes `&str` only for the error
  message — convert for consistency, render via `Display`)
- :54 `pool_balance_raid1`
- :78 `pool_balance_single`
- :103 `pool_balance_raid1_soft`
- :132 `maybe_restore_raid1`
- :152 `pool_remove_device`
- :178 `pool_remove_devid`
- :198 `pool_replace_device`
- :226 `pool_resize_device`
- :266 `evict_present_device`
- :311 `pool_bootstrap_mount`
- :353 `pool_bootstrap_mount_raid1`

**preflight (cli/src/preflight.rs):**
- :165 `check_not_read_only`
- :216 `probe_missing_devids`
- :312 `require_mutation_preflight`

**progress (cli/src/progress.rs):**
- :136 `progress_device_replace`
- :211 `progress_balance_data`

**plan builders / commands:**
- cli/src/replace.rs:428 `device_replacement_plan`
- cli/src/remove.rs:186 `remove_plan` (note: cli/src/remove.rs:228
  `compile_remove_present_steps` already takes `&MountPoint` — this finishes
  the job)
- cli/src/remove_missing.rs:224 `remove_missing_plan` (note: :264 already
  takes `&MountPoint`)
- cli/src/tui/probe.rs:21 `probe_pool_for_tui`

### Call sites to update

After the signatures change, drop `.as_str()` at every call site that already
has a `&MountPoint` in scope. Known sites (non-exhaustive — `cargo check`
will surface the rest):

- cli/src/main.rs:485 `cmd_idle`
- cli/src/main.rs:513 `cmd_monitor`
- cli/src/main.rs:538 `cmd_ack`
- cli/src/status.rs:300, :348-364, :399, :450-470 (cmd_status's helper calls)
- cli/src/recover.rs:274 (already binds `params.config.mount_point().as_str()`
  — collapse to `params.config.mount_point()`)
- cli/src/unlock.rs:90 (already binds `mount_point = params.config.mount_point()`,
  drop `.as_str()`)
- cli/src/lock.rs:130 (drop `.as_str()`)
- cli/src/replace.rs:70, :88, :106, :320 (drop `.as_str()`)
- cli/src/remove_missing.rs:77, :95, :111, :131, :189, :193 (drop `.as_str()`)

### Explicit non-goals

- **Do not change the `browse` module** (cli/src/browse/mod.rs:28, :42;
  cli/src/browse/model.rs). Browse accepts a CLI-supplied `--mount-point`
  override at cli/src/main.rs:639-657, so its mount point is *not*
  necessarily a config-validated value. The `&str` signature there honestly
  reflects "arbitrary user input", which is exactly what `MountPoint` is
  meant *not* to mean. Leaving browse on `&str` keeps the newtype's
  invariant honest.
- **Do not introduce any new `MountPoint::new()` constructor or validation.**
  This refactor only redistributes existing values; it does not change how
  they are produced.
- **Do not modify `cmd_status`'s outer signature** (cli/src/status.rs:389) —
  it already takes `&Config`. Only its internal helpers change.

### Dead code to remove

- cli/src/types.rs:50-54 `impl AsRef<str> for MountPoint` — confirmed
  unused; no caller invokes `.as_ref()` on a `MountPoint` to get a `&str`,
  and no function takes `impl AsRef<str>` and is passed a `MountPoint`.
- cli/src/types.rs:56-60 `impl AsRef<Path> for MountPoint` — also unused.
  If a future caller needs a `Path`, `Path::new(mp.as_str())` is one line
  and zero ambiguity.

Keep `MountPoint::as_str()` (cli/src/types.rs:21) and `impl Display`
(:44-48). `as_str()` is the discoverable form callers already reach for, and
`Display` is used by error messages and `paused_balance_warning`.

## Approach

Mechanical, file-by-file. The change is a search-and-replace with type
checking; there is no logic to redesign.

1. Update `cli/src/types.rs` first: delete the two `AsRef` impls. (`cargo
   check` will not yet flag anything.)
2. Convert helpers in dependency order so each layer compiles before its
   callers move:
   1. `probe.rs` → `idle.rs` → `monitor.rs` → `ack.rs`
   2. `status.rs` helpers → callers inside `status.rs`
   3. `pool.rs` helpers → `progress.rs` → callers (`add.rs`, `replace.rs`,
      `remove.rs`, `remove_missing.rs`, `recover.rs`, `unlock.rs`, `lock.rs`)
   4. `preflight.rs` → callers
   5. `tui/probe.rs`
3. For each function: change the parameter type, delete the
   `MountPoint(mount_point.to_owned())` line, update internal references
   (`mp.clone()` → `mount_point.clone()`, `entry.target == mount_point` →
   `entry.target == mount_point.as_str()`, etc.).
4. Update test call sites. Approximate scope from exploration: ~200–400
   call sites across cli/src/{probe,idle,monitor,ack,status,pool,preflight,
   progress,replace,remove,remove_missing,add,unlock,lock,recover,tui}.rs.
   `MountPoint` owns a `String`, so it cannot be a `const`. The fix per
   test module is a small zero-arg helper that returns a fresh value:

   ```rust
   fn mp() -> MountPoint {
       MountPoint("/mnt/storage".into())
   }
   ```

   Then call sites become `cmd_idle(&runner, &mp())` instead of
   `cmd_idle(&runner, MP)`. Drop the existing `const MP: &str = ...`
   declarations (cli/src/idle.rs:110, cli/src/preflight.rs's `MOUNT`, and
   any others discovered during conversion) in favor of `fn mp()`.

   Where a test module already constructs an inline literal in only one or
   two places, prefer a local `let mount_point = MountPoint("/mnt/storage".into());`
   at the top of the test fn instead of introducing a helper for two callers.
5. Run `cargo check`, `cargo clippy`, `cargo test`, then VM tests as below.

The `fn mp()` helper is worth doing per-file rather than per-call-site — it
collapses ~17 conversions in idle.rs to one helper definition + a uniform
`&mp()` substitution.

## Critical files

- cli/src/types.rs (delete dead impls)
- cli/src/probe.rs, idle.rs, monitor.rs, ack.rs (the originally-flagged five)
- cli/src/status.rs (7 helpers + cmd_status internals)
- cli/src/pool.rs (13 functions)
- cli/src/preflight.rs (3)
- cli/src/progress.rs (2)
- cli/src/replace.rs, remove.rs, remove_missing.rs, recover.rs, unlock.rs,
  lock.rs (call-site updates and one-each helper conversions)
- cli/src/tui/probe.rs (1)
- cli/src/main.rs (drop `.as_str()` at idle/monitor/ack call sites)

## Verification

1. `cargo check` — must compile clean.
2. `cargo clippy --all-targets -- -D warnings` — no new warnings.
3. `just test-rust` — Rust unit tests, including the parser golden suite.
4. `just test-vm` — full NixOS VM suite. The refactor touches no behavior, so
   any failure is a typo in the conversion.
5. Spot-check that no new allocations crept in: grep cli/src/ for
   `MountPoint(mount_point.to_owned())` and `MountPoint(mount_point.into())`
   — should return zero hits inside the converted functions.
6. Confirm the dead code removal: grep cli/src/ for `as_ref` on `MountPoint`
   contexts; should return zero hits beyond the deleted impls.

No fixture refresh needed — parser inputs/outputs are unchanged.
