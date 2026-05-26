# Plan: delete dead `pub fn`s hidden behind the lib+bin split

## Context

`braid-cli` has both a library target (`cli/src/lib.rs`) and a binary target
(`cli/src/main.rs`). Because `pub` items in a library are treated as reachable
public API, the `dead_code` lint **never fires** on an unused `pub fn` in this
crate -- so dead public functions accumulate invisibly.

The originating finding flagged one such function (`device_has_btrfs_superblock`).
A reproducible, tracked-file sweep (commands + current results recorded in
[Inventory sweep](#inventory-sweep-reproducible) below) shows the crate has 384
`pub fn` declarations across 335 unique names, of which **exactly four**
uniquely-named functions are dead -- their only word-boundary occurrence is
their own definition. None ship behavior; all four are uncalled from `main.rs`,
any other function, or any test (scanned across `cli/src` + `cli/tests`).

This is not cosmetic. Dead public surface actively rots: an in-flight
auto-spindown review doc (`plans/review-2026-04-30-auto-spindown-prev.md:41`)
already asserts a *false* fact -- that `check_power_mode` is "used by
status/doctor" -- when it has zero callers. Removing the dead code removes the
surface that misleads the next reader. Decision (confirmed with the user):
delete all four, including the `hdparm` module; git history is the re-add path
if a future feature wires the read path.

## Inventory sweep (reproducible)

Run from the repo root. Inventory is derived from tracked files (`git ls-files`)
per AGENTS.md, and usage is counted across both `cli/src` and `cli/tests` (the
integration-test dir). `pub fn ` captures the entire public-fn surface here --
there are no `pub async/const/unsafe/extern fn` declarations. The `-I`
(`--no-filename`) flag on the name extraction is required, or `rg` prefixes each
captured name with `file:` and every later word-count collapses to zero.

```sh
srcfiles=$(git ls-files -- cli/src | rg '\.rs$')              # 106 files
allfiles=$(git ls-files -- cli/src cli/tests | rg '\.rs$')    # 113 files

# Surface size:
echo "$srcfiles" | xargs rg -No '^[[:space:]]*pub fn ' | wc -l                    # 384 declarations
echo "$srcfiles" | xargs rg -INo '\bpub fn ([a-z_]\w*)' -r '$1' | sort -u | wc -l # 335 unique names

# Dead = a pub fn name whose only word-boundary occurrence across src+tests is its definition:
names=$(echo "$srcfiles" | xargs rg -INo '\bpub fn ([a-z_]\w*)' -r '$1' | sort -u)
echo "$names" | while IFS= read -r n; do
  c=$(echo "$allfiles" | xargs rg -ow -- "$n" 2>/dev/null | wc -l | tr -d ' ')
  [ "$c" -eq 1 ] && echo "$n"
done
```

Current result (2026-05-26) -- exactly the four deletions below, nothing else:
`check_power_mode`, `device_has_btrfs_superblock`, `emit_credential_wait_line`,
`set_state`.

**Limitation (honest scope of the claim):** this catches only *uniquely-named*
dead fns. A dead `pub fn` whose name collides with a live identifier elsewhere
(another definition, a local, a struct field) would have count >= 2 and be
masked -- a false negative; it cannot produce a false positive. Mitigated by:
(a) every count==2 name was inspected and is a genuine one-definition/one-caller
pair (e.g. `set_mountpoint_err`, `with_coord_file`); (b) no collision-named pub
fn (`as_str`, `new`, `parse`, ...) has total occurrences < 3, so none are dead;
(c) each of the four is independently verified at its definition site in the
per-item notes below. "Four" is therefore exhaustive for uniquely-named dead pub
fns and a lower bound otherwise.

## Scope: four deletions

### 1. `device_has_btrfs_superblock` -- `cli/src/luks.rs:947-958`
Delete the doc comment + function. **Dead because:** its only caller was
deliberately removed in commit `43875a89` ("remove dead bootstrap superblock
check, add regression test for the invariant that makes it dead"), which
replaced the runtime check with the `bootstrap_rejects_braid_labeled_luks_disk`
regression test in `add.rs`. **No cascade:** the `CmdRequest::BtrfsDeviceScan`
variant it used stays live (heavily used in `recover.rs`). No `use` import to
remove -- the import was dropped in `43875a89`.

### 2. `emit_credential_wait_line` -- `cli/src/status_tag.rs:84-86`
Delete the function (the thin `emit_status(&credential_wait_line(...))` wrapper).
**Dead because:** the hoist refactor in plan
`2026-05-07-hoist-enroll-keyfile-probe-helper.md` removed its last direct caller
and explicitly noted to "Drop `emit_credential_wait_line`" -- but the drop was
never done. **No cascade:** its callee `credential_wait_line` (`status_tag.rs:76`)
stays -- it has many live callers in `credential_verify.rs` and is exercised by
`status_tag.rs` tests. `emit_status` (`status_tag.rs:66`) also stays. No caller
or `use` import to remove.

### 3. `set_state` (test fixture method) -- `cli/src/online_state.rs:447-449`
Delete the method from the `#[cfg(test)] impl RecordingOnlineStateOps` block.
**Dead because:** it is the one setter on this otherwise heavily-used recording
mock that no test calls (siblings `set_mounted`, `set_mountpoint_err`,
`set_bound_by_ok/err`, `set_systemctl_stop_err` are all used). **No cascade:**
the `state` field (`online_state.rs:409`) stays -- it is initialized in `new()`
(defaults to `Ok(UnitActiveState::Inactive)`) and read by the
`unit_active_state` impl; every test already relies on that default, so removing
the unused override changes no test outcome. Test-only code -- never shipped.

### 4. `hdparm` module (whole file) -- `cli/src/hdparm.rs` + `cli/src/lib.rs:20`
Delete the entire `cli/src/hdparm.rs` (49 lines: `DrivePowerState` enum +
`check_power_mode` + the two `const`s) and remove the `pub mod hdparm;` line at
`cli/src/lib.rs:20`. **Dead because:** `check_power_mode` has zero callers and
`DrivePowerState` is used only inside the module; the module was added in
`b38cd98c "tui: big update"` but is no longer wired anywhere. The in-flight
auto-spindown v1 plan does **not** consume it (it adds a new `spindown.rs` for
the `hdparm -S` *write* path; its `status` section cites
`status.rs`/`probe.rs`/`luks.rs`, never `check_power_mode`). **No cascade:** the
`libc` dependency stays (used by `cmd.rs` signal-name table and `inhibit.rs`),
so no `Cargo.toml` change. The only other `hdparm` token in the tree is an
unrelated `cmd.rs` comment about "HDD spindown".

## Out of scope: prevention

The root cause is the lib+bin split defeating `dead_code`. A real preventive fix
means demoting internal `pub` -> `pub(crate)` so the lint works -- but `main.rs`
is a separate crate and the integration tests import `braid_cli::parse` /
`braid_cli::cmd::RawCommandOutput`, so a visibility refactor must carefully
preserve the genuinely-external surface across the 384 `pub fn` declarations
(335 unique names). That is a large, separate effort. Note: `#![warn(unreachable_pub)]` does **not** help here -- a
`pub fn` in a `pub mod` is publicly reachable, so the lint won't flag it. Defer;
the scripted occurrence-count sweep used to produce this plan is the lightweight
guard if recurrence becomes a problem.

## Verification

These are pure deletions of uncalled code, so the existing suite passing is the
proof of no behavior change. No fixture refresh and no VM tests are needed: no
parser, tool-output, systemd unit, or NixOS module is touched.

1. `just test-rust` -- must compile clean with no new warnings and the full Rust
   suite green. This is the primary proof (it covers the `online_state` /
   `lock.rs` tests touched by the `set_state` removal and the
   `credential_verify` / `status_tag` tests around `emit_credential_wait_line`).
2. Post-deletion grep gate -- each of these must return **zero** hits across
   `cli/src cli/tests` after the edits (same surface as the inventory sweep):
   - `rg -w device_has_btrfs_superblock cli/src cli/tests`
   - `rg -w emit_credential_wait_line cli/src cli/tests`
   - `rg -w 'check_power_mode|DrivePowerState' cli/src cli/tests`
   - `rg -n 'hdparm' cli/src cli/tests` (should be empty)
   (`set_state` is a common name; verify by reading
   `RecordingOnlineStateOps` rather than a bare grep.)
3. Do **not** run `cargo fmt` / any formatter (per AGENTS.md). Use narrow edits.

## Notes for the implementer

- Pure removal; no new code, no renames, no signature changes.
- Re-add path for `hdparm` if a power-state read display is ever built:
  `git show b38cd98c -- cli/src/hdparm.rs` restores it verbatim, to be wired and
  tested in that same change.
- The historical plan-doc references to these symbols (under `plans/impl/` and
  `plans/review*`) are dated records and should be left as-is.
