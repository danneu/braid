# Refactor: drop dead parameters and orphaned generic bounds

## Context

A review finding flagged `resolve_replace_source` (`cli/src/replace.rs`)
for carrying an unused `_runner: &R` (plus its `R: CommandRunner` bound)
and an unused `_mount_point: &MountPoint` -- the function resolves the
replace source entirely in-memory from `pool` + `old_member`, does no
I/O, yet every caller and test threads a runner and mount point into it.

Verifying that finding surfaced a whole *class* of the same smell:
parameters (and the generics that only exist to type them) that went
dead when the I/O they once performed moved upstream, but were retained
and `_`-prefixed to silence the unused-variable warning. Git history
confirms the mechanism for the cited case: at introduction (`6b10ab7`)
`resolve_replace_source` genuinely called
`preflight::probe_missing_devids(runner, mount_point)`; the probe later
moved into `PoolState` population and the params became vestigial.

This refactor dissolves the entire class in one behavior-preserving
pass. The removed arguments are provably never read, so there is no
runtime behavior change -- only a simpler, more honest set of
signatures (a function's type no longer implies it probes the system)
and fewer monomorphizations. `braid` is unreleased with no
backwards-compatibility obligation (see `AGENTS.md` "No backwards
compatibility"), so changing public `mount` signatures is in-bounds.

## Scope: four functions, three cascade depths

### Tier 1 -- clean leaf removals (no cascade beyond callsites)

1. **`resolve_replace_source`** -- `cli/src/replace.rs:1672`
   Drop `_runner: &R`, the `<R: CommandRunner>` generic, and
   `_mount_point: &MountPoint`. Keep `old_name, old_uuid, old_member,
   missing_id, pool`. Body (lines 1681-1788) references none of the
   dropped params.
   - Callsites: 1 production (`replace.rs:1288`) + 16 tests (`replace.rs`
     2254, 2277, 2303, 2527, 2556, 2595, 2628, 2666, 2704, 2742, 2773,
     2827, 2864, 2901, 6176, 6274).
   - **Test-coverage caveat (callsites 6176 + 6224):** the missing-path
     decoy test loses a no-probe assertion that goes *vacuous* (not
     compiler-flagged) once the runner is dropped. It must be re-homed at
     the `plan_replace` level, not silently deleted -- see "Test coverage
     relocation" below.

2. **`require_mutation_preflight`** -- `cli/src/preflight.rs:612`
   Drop `_runner: &R` and the `R: CommandRunner + Sync` bound; keep
   `<F: Filesystem + ?Sized>` and params `fs, fsid, mount_point`. Body
   (618-631) uses only `fs`/`fsid`/`mount_point`.
   - Callsites: 4 production (`add.rs:1701`, `remove.rs:517`,
     `remove_missing.rs:368`, `replace.rs:1228`) + 6 tests (`preflight.rs`
     1721, 1733, 1750, 1772, 1797, 1970).

### Tier 2a -- mount cascade (drops `<F>` on two public fns)

3. **`scan_and_mount`** -- `cli/src/mount.rs:799`
   `_fs: &F` is dead (body uses `runner`, `config`, `plan`,
   `color_enabled`, and `std::fs::create_dir_all`). Drop `_fs` and the
   `<F: Filesystem + ?Sized>` generic; keep `<R: CommandRunner>`.

   Its only two callers thread `fs` *solely* to feed it, so removing it
   makes their `fs`/`<F>` dead too. Follow the cascade:

   - **`execute_mount_only`** (`mount.rs:633`, `pub fn`): drop `fs: &F`
     and `<F>`. Keep `<R>`.
   - **`execute_unlock_and_mount`** (`mount.rs:662`, `pub fn`): drop
     `fs: &F` and `<F>`. Keep `<R>`.

   This *enforces in the type system* the contract already documented on
   both functions: "Planning + probing + validation lives in
   `plan_open_pool` ... This function does NOT plan; it only executes."

   The cascade terminates here. Every remaining caller keeps `fs` for
   other work (`plan_open_pool`), so each just drops the argument:
   - `execute_mount_only` callers: `unlock.rs:103`, `recover.rs:975`,
     `test_fixtures/mount.rs:427`, `mount.rs:980` (test).
   - `execute_unlock_and_mount` callers: `unlock.rs:111`,
     `recover.rs:992`, `recover.rs:3555`, `test_fixtures/mount.rs:432`,
     and 7 tests (`mount.rs` 2668, 2728, 2825, 2886, 2983, 3044, 3150).
   - `open_and_mount_for_test` (`test_fixtures/mount.rs:403`) keeps its
     own `<F>`/`fs` -- still needed by `plan_open_pool` at line 413.

### Tier 2b -- recover cascade (drops a crate-private enum field)

4. **`execute_replace_pool_mutation_recovery`** -- `cli/src/recover.rs:3059`
   `_old_name: &DiskName` is dead (body 3076+ never reads it). Drop the
   param. Both `<R>` and `<F>` stay (used elsewhere). The value that
   feeds it is itself dead data, so follow the cascade within
   `recover.rs` (the `RecoverCompletion` enum is crate-private --
   `recover.rs:252` -- so no public API change):
   - Remove the `old_name: DiskName` field from
     `RecoverCompletion::ReplacePoolMutation` (`recover.rs:270`).
   - Remove `old_name: old_name.clone()` from its construction
     (`recover.rs:1506`) and drop `old_name,` from the enclosing
     `journal::OpKind::Replace { .. }` destructure pattern
     (`recover.rs:1497`); the pattern already has `..` (1502).
   - Drop `old_name,` from the execute-path destructure
     (`recover.rs:669`) and the matching argument at the callsite
     (`recover.rs:685`).
   - The render-path destructure (`recover.rs:527`) already ignores it
     via `..` -- no change.
   - **Do not touch** the persisted journal type `journal::OpKind::Replace`'s
     `old_name` field; it is on-disk recovery metadata and out of scope.
   - Function callsites: 1 production (`recover.rs:674`) + ~11 tests
     (`recover.rs` 9484, 9567, 9626, 9744, 9850, 9913, 10012, 10133,
     10219, 10340, 10477).

## Cross-cutting rule: clean up newly-dead bindings

Removing an argument frequently orphans a local in the *test* callsites
(e.g. `let runner = MockRunner::default();` used only for that one call;
likewise a `let fs = ...` or a `DiskName::parse(...)` bound only to feed
the dropped `old_name`). The crate does not deny warnings, so these are
non-fatal -- but the ideal refactor removes them. For every modified
callsite, delete any `let` binding the compiler then reports as unused.
Production callers keep their bindings (used elsewhere). Note:
`resolve_replace_source` tests pass the mount point as an inline
`&mp()`, not a binding, so only the `runner` binding needs removing
there.

**Caveat -- bindings kept alive by a now-vacuous assertion.** The
compiler-flag heuristic misses one case: a `runner` binding still read by
a `runner.requests()` assertion. After the param drop, that assertion can
become vacuous (the runner is no longer threaded into the code under
test), yet the binding stays "used" and is *not* flagged. These must be
hunted by hand. The known instance is the missing-path decoy test in
`replace.rs` -- handled under "Test coverage relocation", not here.

## Test coverage relocation (do not drop the no-probe guarantee)

Dropping `_runner` from `resolve_replace_source` guts a real assertion.
The helper test
`replace_missing_path_decoy_regression_resolves_by_name_to_uuid`
(`replace.rs:6128`) asserts, via `runner.requests()`
(`replace.rs:6224-6232`), that missing-path resolution issues no
`CryptsetupLuksUuid` probe against the decoy by-ids -- i.e. it resolves
from persisted state, not live probes. Once `resolve_replace_source` no
longer receives a runner, that assertion is vacuous (it only proves an
unrelated empty `MockRunner` stayed empty), and because `runner.requests()`
keeps the binding live, the compiler will not flag it. Preserve the
behavioral coverage by relocating it to the level that still owns a
runner:

1. **Trim the helper test (`replace.rs:6128`).** Remove the `runner`
   binding (`replace.rs:6162`) and the no-probe block
   (`replace.rs:6224-6232`); drop `&runner` and `&mp()` from its
   `resolve_replace_source` call. Keep everything else -- the
   name->UUID resolution dodging the by-id decoy, `Missing { devid: 2 }`,
   and the `target_membership` assertions. The helper test becomes a
   pure in-memory resolution test (no runner), which is correct.

2. **Add one `plan_replace`-level missing-path decoy test** with a
   recording runner. No existing `plan_replace` test fits: the closest,
   `plan_replace_missing_preview_has_no_notes_and_matches_legacy_step_render`
   (`replace.rs:5175`), uses a plain `one_live_one_missing` fixture with
   no by-id decoy. The new test reproduces the decoy membership (name
   `misleading-label` -> U_R at by-id `/dev/disk/by-id/right`; `decoy` ->
   U_D at by-id `/dev/disk/by-id/misleading-label`; pool reports devid 2
   missing), drives `plan_replace` on the missing path with a recording
   runner, and asserts the resolved source is the missing devid 2.

3. **Assert by command *target*, not by probe *variant*.** Two traps to
   avoid. First, a blanket "no `CryptsetupLuksUuid`" assertion would fail:
   `plan_replace` legitimately probes the `--new` disk --
   `probe_config_disk` (`probe.rs:172`) issues
   `CryptsetupLuksUuid`/`CryptsetupLuksDumpText` against the `--new`
   by-id. Second -- and the reason this finding recurs -- enumerating the
   *forbidden* probe variants is itself brittle: there are three by-id
   LUKS readers (`CryptsetupLuksUuid`, `CryptsetupLuksDumpText`, and
   `CryptsetupLuksDump` -- the last used by `check_key_slot`,
   `luks.rs:1021`) plus device-bearing mutators (`CryptsetupLuksOpen`,
   `Format`, `HeaderBackup`, `OpenKeyFile`, `AddKeyFile`), and a
   hand-maintained variant list has already missed one twice
   (`DumpText` in round 1, `Dump` in round 2).

   Instead assert the structural contract directly: **no issued request
   names either decoy by-id (`/dev/disk/by-id/right`,
   `/dev/disk/by-id/misleading-label`) as a command target.** Implement it
   over the canonical renderer -- for each `runner.requests()` entry, call
   the existing `pub CmdRequest::to_argv()` (`cmd.rs:446`) and assert no
   token in the resulting `args` equals a decoy by-id. `to_argv` already
   matches every variant in one place, so the check covers reads and
   mutators alike and can never go stale as new requests are added; no new
   production code is needed. The distinct `--new` by-id and live
   member-device requests are different strings and remain allowed, as
   required. (Choose a `--new` by-id with no substring overlap with the
   decoy paths, e.g. `/dev/disk/by-id/new`, and match tokens by equality.)

This guards the real regression -- missing-path planning re-introducing a
by-id probe of the *old* disk -- at the command boundary, structure-
insensitively (it asserts on issued `CmdRequest`s, not internal helper
names).

## Out of scope (deliberate non-removals)

- `journal::OpKind::Replace.old_name` (persisted journal schema).
- `<F>` on `plan_open_pool`, `open_and_mount_for_test`, and the
  command-level functions in `unlock.rs`/`recover.rs` -- all still use
  `fs` for real probing.
- The `F: Filesystem` bound on `require_mutation_preflight` (live).
- The `R: CommandRunner` bound on `scan_and_mount` (live).

## Files touched

- `cli/src/replace.rs` (definition + 17 callsites + missing-path
  no-probe test relocation)
- `cli/src/preflight.rs` (definition + 6 test callsites)
- `cli/src/mount.rs` (3 definitions + test callsites)
- `cli/src/recover.rs` (1 definition + enum + construction + ~12 callsites)
- `cli/src/add.rs`, `cli/src/remove.rs`, `cli/src/remove_missing.rs`
  (one `require_mutation_preflight` callsite each)
- `cli/src/unlock.rs` (2 callsites)
- `cli/src/test_fixtures/mount.rs` (2 callsites)

## Docs

No design-doc or principle changes: this is a pure simplification with
no behavior or invariant change. No `///` doc comment names any dropped
parameter (the `mount` doc comments describe the execute-vs-plan
contract, which the change reinforces). Re-scan the three `mount`
function doc comments during impl only to confirm none names `fs`.

## Verification

This refactor cannot alter runtime behavior -- every removed argument was
provably unread -- so the compiler plus the existing unit tests are a
necessary-and-sufficient gate; a missed callsite or cascade site is a
compile error, and the existing tests already pin each function's
behavior and keep passing with fewer arguments.

1. **Primary:** `just test-rust` -- compiles the whole CLI crate
   (including every modified test callsite) and runs `cargo test`. Green
   = done.
2. **One test relocated (see "Test coverage relocation"); no others
   added.** Trim the helper test
   `replace_missing_path_decoy_regression_resolves_by_name_to_uuid` and
   add one `plan_replace`-level missing-path decoy test so the no-probe
   guarantee survives the runner drop. Beyond that, no new tests --
   asserting "a parameter was removed" would be structure-sensitive and
   add no behavioral coverage. Existing tests for all four functions are
   the regression guard and keep passing with fewer arguments.
3. **Optional reassurance** (not required, since no runtime path
   changes): a focused VM lifecycle run such as
   `just test-vm unlock-and-mount` (or the nearest mount/unlock test),
   given Tier 2a touches the unlock/mount execute entry points at the
   signature level only.

Do not run `cargo fmt` / `just fmt` (per `AGENTS.md` Formatting); make
narrow hand edits so the diff stays confined to the parameter removals.
