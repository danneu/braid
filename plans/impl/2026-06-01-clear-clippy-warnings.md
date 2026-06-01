# Plan: clear clippy warnings (except large-err and too-many-args)

## Context

`just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) currently
emits **44 unique warnings** across 12 lint types. None fail the build (no
`-D warnings`, no `[lints]` table, no `clippy.toml`), but they're noise.

The user wants a single commit that fixes **all** warnings **except** two
deliberately-deferred lint families:

- `clippy::result_large_err` ("the `Err`-variant returned from this function is
  very large") -- needs boxing of error enums, a real design decision.
- `clippy::too_many_arguments` ("this function has too many arguments") -- needs
  parameter-struct refactors.

Both deferred families are real refactors with behavioral blast radius, out of
scope for a mechanical lint sweep. After this commit, `just clippy` should emit
**only** those 12 warnings, documented in the table below.

**Base tree (re-verified 2026-06-01):** `cargo clippy --manifest-path
cli/Cargo.toml --tests` compiles clean (exit 0) and the inventory below matches
the live tree. The earlier draft of this plan listed one additional deferred
`too_many_arguments` warning in `cli/src/mount.rs`, but commit `bbc4686`
had already fixed that by bundling the open-pool planning inputs before this
implementation ran.

This is a pure cleanup commit: every fix is a clippy-equivalent rewrite with
**zero behavior change**. The bulk (21 of 32) is test-only code in one file.

## Scope

| | Lint | Count | Action |
|---|---|---:|---|
| fix | `field_reassign_with_default` | 21 | struct-literal init |
| fix | `collapsible_if` | 2 | let-chain |
| fix | `cloned_ref_to_slice_refs` | 2 | `std::slice::from_ref` |
| fix | `if_same_then_else` | 1 | merge arms with `\|\|` |
| fix | `manual_div_ceil` | 1 | `usize::div_ceil` |
| fix | `derivable_impls` | 1 | `#[derive(Default)]` + `#[default]` |
| fix | `io_other_error` | 1 | `io::Error::other` |
| fix | `new_without_default` | 1 | add `impl Default` |
| fix | `needless_borrows_for_generic_args` | 1 | drop `&` |
| fix | `expect_fun_call` | 1 | `unwrap_or_else(\|err\| panic!(...))` (keep error in msg) |
| **fix total** | | **32** | |
| keep | `result_large_err` | 6 | deferred (table below) |
| keep | `too_many_arguments` | 6 | deferred (table below) |
| **keep total** | | **12** | |

## Fixes by lint

### 1. `field_reassign_with_default` (21) -- all in `cli/src/tui/browse/state.rs` tests

Each test does `let mut state = BrowseState::default();` then assigns one or more
fields before the first method call. Lift those leading assignments into a struct
literal: `let mut state = BrowseState { <fields>, ..Default::default() };`.

Only the **leading run** of `state.<field> = ...` (before any method call or
`assert!`) moves into the literal. Later re-assignments (e.g. a second
`state.focus = ...` after a `select_next()`, or `state = BrowseState::default()`
re-bindings) stay as-is -- clippy does not flag those.

`BrowseState`'s fields are reachable from the test module (the tests already
assign them directly), so the literal compiles.

Per-site spec (test fn -> fields to lift):

| test fn | fields in literal |
|---|---|
| `command_display_smartctl_picker_preview_uses_selected_device` | `program: Smartctl, smartctl_command: SmartctlHealth` |
| `command_display_nut_snapshot_source_shows_upsc_query` | `program: Nut` **(also drop `mut` -- see note)** |
| `command_display_subvolume_detail_shows_dispatched_request` | `btrfs_command: BtrfsSubvolumes, focus: Content` |
| `l_at_rightmost_is_noop` | `focus: Content` |
| `l_from_command_skips_subview_when_no_subviews` | `focus: Command` |
| `l_from_command_enters_subview_when_filesystem` | `focus: Command` |
| `l_from_command_enters_subview_when_devices` | `focus: Command` |
| `j_in_subview_cycles_filesystem_views` | `focus: Subview` |
| `j_in_subview_cycles_devices_usage_stats` | `focus: Command` |
| `new_btrfs_command_groups_have_expected_subviews` | `btrfs_command: BtrfsSubvolumes` |
| `new_browse_selections_map_to_expected_requests` | `filesystem_subview: CommitStats` |
| `nut_upses_without_config_runs_discovery_command` | `program: Nut, nut_command: NutUpses` |
| `smartctl_per_device_without_disks_sets_empty_state` | `program: Smartctl, smartctl_command: SmartctlHealth` |
| `smartctl_scan_runs_without_disks` | `program: Smartctl, smartctl_command: SmartctlScan` |
| `nut_views_that_need_a_name_still_require_config` | `program: Nut, nut_command: command` (inside `for`, 12-space indent) |
| `enter_in_subvolume_row_drills_in` | `focus: Command` |
| `enter_in_smartctl_device_row_drills_in` | `program: Smartctl, smartctl_command: SmartctlHealth, focus: Content` |
| `enter_in_systemd_unit_row_drills_in` | `program: Systemd, systemd_command: SystemdStatus, focus: Content` |
| `non_list_subvolume_views_do_not_drill_in` | `btrfs_command: BtrfsSubvolumes, subvolume_subview: Full, focus: Content` |
| `esc_pops_back` | `focus: Command` |
| `esc_pops_back_from_smartctl_detail` | `program: Smartctl, smartctl_command: SmartctlHealth, focus: Content` |

(Enum variants above are unqualified for brevity; in code use the spellings
already present at each site, e.g. `BrowseProgram::Smartctl`,
`BrowseCommand::SmartctlHealth`, `BrowseFocus::Content`, `FilesystemSubview::CommitStats`.)

**`mut` note:** `command_display_nut_snapshot_source_shows_upsc_query` only calls
`state.command_display(&self)` after the literal -- no later mutation -- so it
must become `let state = BrowseState { program: Nut, ..Default::default() };`
(no `mut`), else `unused_mut` fires. Every other site mutates `state` later
(`select_next`, `focus_right`, `command_finished`, `load_current`, `enter`,
`&mut state`, or a re-bind) and keeps `mut`. The final `just clippy` re-run is
the backstop for any missed `unused_mut`.

### 2. `collapsible_if` (2) -- let-chain (edition 2024, `&& let` already used widely)

- `cli/src/main.rs:927` (production, `discover` bare-read gate):
  ```rust
  if !args.write
      && let Err(e) = braid_cli::discover::check_pool_json_for_bare_discover(&pool_json)
  {
      print_cli_error(&e.to_string());
      std::process::exit(1);
  }
  ```
- `cli/src/remove_missing.rs:454` (production, relocation-space gate):
  ```rust
  if pool.devices.len() >= 2
      && let Err(e) = check_relocation_space(runner, config.mount_point(), params.missing_id)
  {
      return Err(PlanFailure::with_notes(notes, e));
  }
  ```

### 3. `cloned_ref_to_slice_refs` (2) -- `cli/src/types.rs:886` and `:918` (tests)

`LuksFormatExtraOpts::parse(&[token.clone()])` ->
`LuksFormatExtraOpts::parse(std::slice::from_ref(&token))`. `token` stays owned
and is still used after the call (the `.clone()` only existed to build a 1-elem
slice).

### 4. `if_same_then_else` (1) -- `cli/src/recover.rs:1099` (production, remove-recovery restore loop)

First two arms are byte-identical (`recovered.insert(uuid.clone(), member.clone())?;`).
Merge their conditions:
```rust
if null_underlying_match.is_some() || in_missing {
    recovered.insert(uuid.clone(), member.clone())?;
} else if member.devid.is_none()
    && (!pool.null_underlying.is_empty() || !pool.missing_devids.is_empty())
{
    return Err(RecoverError::Failed(format!( ... )));
}
```
Control flow is unchanged.

### 5. `manual_div_ceil` (1) -- `cli/src/tui/view/mod.rs:83` (production, `hint_lines` word-wrap)

`(word_len + width - 1) / width` -> `word_len.div_ceil(width)`. `width > 0` is
guaranteed by the `if width == 0 { return 0; }` guard at line 71, so the result
is identical.

### 6. `derivable_impls` (1) -- `cli/src/confirm.rs:38` (test enum `Verdict`)

Add `Default` to the existing derive and tag the unit variant; delete the manual
impl:
```rust
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
enum Verdict {
    #[default]
    Unexpected,
    Accept,
    Decline,
}
// remove the `#[cfg(test)] impl Default for Verdict { ... }` block
```

### 7. `io_other_error` (1) -- `cli/src/lock.rs:1560` (test)

`io::Error::new(io::ErrorKind::Other, "synthetic mark_done failure")` ->
`io::Error::other("synthetic mark_done failure")`. `io` is already imported
(`use std::io::{self, Write};`).

### 8. `new_without_default` (1) -- `cli/src/online_state.rs:420` (test helper `RecordingOnlineStateOps`)

`new()` does non-trivial init, so do **not** `#[derive(Default)]`. Add a
delegating impl next to the existing `impl`:
```rust
#[cfg(test)]
impl Default for RecordingOnlineStateOps {
    fn default() -> Self {
        Self::new()
    }
}
```
(Trait-purpose impl -> no doc comment needed per repo rules.)

### 9. `needless_borrows_for_generic_args` (1) -- `cli/src/unlock.rs:1343` (test)

`std::fs::create_dir(&sp.pool_json())` -> `std::fs::create_dir(sp.pool_json())`.
`pool_json()` returns an owned `PathBuf` (`PathBuf: AsRef<Path>`), so the borrow
is needless.

### 10. `expect_fun_call` (1) -- `cli/src/main.rs:1357` (test helper)

`.expect(&format!("argv should parse for lock policy: {argv:?}"))` ->
`.unwrap_or_else(|err| panic!("argv should parse for lock policy: {argv:?}: {err:?}"))`.

The clippy-canonical rewrite is `unwrap_or_else(|_| panic!("...{argv:?}"))`, but
that discards the error: `Result::expect` panics via `panic!("{msg}: {e:?}")`, so
the original already appended the `clap::Error` Debug. Capturing `err` and adding
`: {err:?}` preserves that diagnostic verbatim (and keeps this a true
zero-behavior-change rewrite). `clap::Error: Debug`, so `{err:?}` compiles.

## What remains after this commit (the requested table)

12 warnings in 2 deferred lint families:

### `result_large_err` (6) -- boxing decision deferred

| location | function |
|---|---|
| `cli/src/enroll_key_file.rs:629` | `plan_enroll` |
| `cli/src/mount.rs:653` | (returns large `Err`) |
| `cli/src/recover.rs:1241` | (returns large `Err`) |
| `cli/src/replace.rs:1169` | (returns large `Err`) |
| `cli/src/replace.rs:3030` | (returns large `Err`) |
| `cli/src/unlock.rs:192` | (returns large `Err`) |

### `too_many_arguments` (6) -- param-struct refactor deferred

| location | function | args |
|---|---|---|
| `cli/src/lock.rs:306` | `push_uuid_classified_candidate` | 10/7 |
| `cli/src/lock.rs:942` | `build_close_sets_full` | 9/7 |
| `cli/src/recover.rs:3414` | (recover helper) | 9/7 |
| `cli/src/enroll_key_file.rs:620` | `plan_enroll` | 8/7 |
| `cli/src/lock.rs:1062` | (lock helper) | 8/7 |
| `cli/src/lock.rs:1224` | (lock helper) | 8/7 |

These are left as live warnings (not `#[allow]`-suppressed) so the counts stay
visible in `just clippy`, per the request.

## Files touched

- `cli/src/tui/browse/state.rs` (21 test edits)
- `cli/src/main.rs` (2: collapsible_if + expect_fun_call)
- `cli/src/types.rs` (2 test edits)
- `cli/src/remove_missing.rs`, `cli/src/recover.rs`, `cli/src/tui/view/mod.rs`,
  `cli/src/confirm.rs`, `cli/src/lock.rs`, `cli/src/online_state.rs`,
  `cli/src/unlock.rs` (1 edit each)

Production-code edits: `main.rs` (discover gate), `remove_missing.rs`
(relocation gate), `recover.rs` (restore-loop arms), `tui/view/mod.rs` (wrap
math). All four are clippy-equivalent rewrites with no behavior change, so
existing unit tests already cover them; no new tests are warranted (the changes
are structure-insensitive equivalences, not new behavior). Everything else is
`#[cfg(test)]` code.

## Verification

1. `just clippy` -- expect a clean run **except** the 12 documented warnings
   (`result_large_err` x6, `too_many_arguments` x6). Confirm no `unused_mut` /
   `unused_variables` regressions slipped in from the struct-literal rewrites.
2. `just test-rust` -- the touched code is almost entirely unit-tested CLI logic
   and test helpers; this must stay green.

## Implementation notes

- Commit `bbc4686` had already bundled the mount open-pool planner inputs before
  this implementation ran, so the current tree no longer emits the planned
  leftover `too_many_arguments` warning for `cli/src/mount.rs:191`; final
  `just clippy` leaves 12 deferred warnings, not 13.
- The unrelated WOL / auto-suspend work referenced in the base-tree note was
  already committed before this implementation ran, so `cli/src/main.rs` had no
  pre-existing unstaged hunks and did not require hunk-level staging.

No VM tests needed: the change is CLI-only, behavior-preserving, and touches no
systemd unit, mount/unlock path semantics, or parser output.

## Commit

Single commit, Conventional Commits, lowercase first line, e.g.:

```
chore(cli): clear clippy warnings except large-err and too-many-args

Mechanical, behavior-preserving lint fixes across the CLI crate
(field_reassign_with_default, collapsible_if, cloned_ref_to_slice_refs,
if_same_then_else, manual_div_ceil, derivable_impls, io_other_error,
new_without_default, needless_borrows_for_generic_args, expect_fun_call).

Deliberately leaves result_large_err (6) and too_many_arguments (6), which
need error-boxing / parameter-struct refactors tracked separately.
```
