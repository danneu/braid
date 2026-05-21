# Plan: finish the "cmd_* take already-loaded Config" sweep

## Context

The 2026-05-19 Rust-owned pool-lock migration set this rule (plan file
line 531):

> `cmd_*` signatures take already-loaded config (loaded under the guard)
> rather than loading it themselves.

The intent was: once the pool lock moved into Rust dispatch
(`cli/src/main.rs:489`), every covered command should load `Config`
exactly once -- inside `main()`, above the dispatch -- and hand a borrow
down. That gives one config-load site per process, under the lock, with
uniform error handling.

The migration commit (`ff6f766`) hoisted the lock but never finished the
config-hoisting half of the rule for the `cmd_*` modules. Today:

| Command          | Dispatch loads? | `cmd_*` loads internally? | Status                                          |
| ---------------- | --------------- | ------------------------- | ----------------------------------------------- |
| `unlock`         | yes (uncond.)   | no                        | conforms                                        |
| `recover`        | yes (uncond.)   | no                        | conforms                                        |
| `add`            | yes (cond.)     | yes (`plan_add`)          | **double-reads** -- tiny TOCTOU + rule miss     |
| `remove`         | no              | yes (`plan_remove`)       | rule miss                                       |
| `remove-missing` | no              | yes (`plan_remove_missing`) | rule miss                                     |
| `replace`        | no              | yes (`plan_replace`)      | rule miss                                       |
| `enroll`         | n/a -- doesn't read config | no             | out of sweep                                   |
| `discover`       | n/a -- doesn't read config | no             | out of sweep                                   |

`Add` is the worst case: dispatch loads `Config` to compute
`online_snapshot`, then `plan_add` re-reads the same file. The TOCTOU
window between those reads is small (no prompts in between) and the file
is NixOS-managed (the CLI never writes it), but the duplication exists
purely because the migration stopped halfway. Remove / RemoveMissing /
Replace don't double-read but still violate the architectural rule: the
file load happens below the lock-guard boundary that the migration
established.

Outcome: every mutating command loads `Config` exactly once, in dispatch,
above the command body. `*Params` structs carry `config: &'a Config`
instead of `config_path: &'a Path`. The "loaded under the guard" rule is
satisfied everywhere it was supposed to apply.

## Scope

In:

- `Commands::Add` (cli/src/main.rs:500), `plan_add` /
  `AddParams` (cli/src/add.rs).
- `Commands::Remove` (cli/src/main.rs:550), `cmd_remove` /
  `RemoveParams` (cli/src/remove.rs).
- `Commands::RemoveMissing` (cli/src/main.rs:576), `cmd_remove_missing`
  / `RemoveMissingParams` (cli/src/remove_missing.rs).
- `Commands::Replace` (cli/src/main.rs:602), `cmd_replace` /
  `ReplaceParams` (cli/src/replace.rs).
- `docs/decisions/022-dry-run-preview-model.md` -- the active ADR
  currently says "migrated mutating `plan_*()` owns everything above
  the dry-run gate: config/state loading, preflight checks, live
  probes, ..." (line 30). This sweep deliberately moves config loading
  and the pending-op preflight up into dispatch; the ADR must be
  amended in the same change so future work isn't told to restore the
  old shape. Update planner doc comments on the four affected
  `plan_*` functions to match.

Out (already conformant or don't need config):

- `Commands::Unlock`, `Commands::Recover` -- already pass `&Config`.
- `Commands::EnrollKeyFile`, `Commands::Discover` -- don't read config
  at all (membership / LUKS-only).
- `Commands::Status`, `Commands::Doctor`, `Commands::Tui`,
  `Commands::Ups`, `Commands::Lock`, `Commands::ScrubMount`,
  `Commands::ScrubCancel` -- already load via `config_read` /
  `load_config_or_exit` in their own arms; orthogonal to this sweep.

## Refactor pattern (applied four times)

For each of `add`, `remove`, `remove_missing`, `replace`:

1. **`*Params` struct change.** Replace
   `pub config_path: &'a Path,` with `pub config: &'a Config,`.
   - The only existing consumer of `params.config_path` in each module
     is the lone `config_read(params.config_path)` call at the top of
     `plan_*`. Verified by `grep params.config_path` across all four
     files -- no secondary uses.

2. **Drop the internal `config_read` AND the internal preflight.**
   - `cli/src/add.rs:1496-1503`, `cli/src/remove.rs:485-492`,
     `cli/src/remove_missing.rs` (analogous block), `cli/src/replace.rs`
     (analogous block) -- delete both the
     `preflight::check_no_pending_operation(params.paths)` block AND
     the `let config = match config_read(params.config_path) { ... }`
     block. Both contracts move to dispatch (see step 3). Consume
     `params.config` directly inside `plan_*`. Also drop the now-unused
     `crate::config::config_read` import and the
     `crate::preflight` import if it becomes dead in that module.
   - Drop each module's `ConfigError` -> `<Cmd>Error::Config` mapping
     only if it becomes dead after this change. The `Validation`
     variants that today wrap the preflight message stay -- only the
     `From<ConfigError>` impl is at risk of becoming dead. Check each
     enum individually (`AddError`, `RemoveError`,
     `RemoveMissingError`, `ReplaceError`).

3. **Dispatch ordering: preflight -> load -> snapshot.** The dispatch
   arm runs three steps in this exact order, mirroring the order that
   `plan_*` enforces today:

   ```rust
   // 1. Preflight FIRST. A pending-op journal must win over a bad
   //    config; otherwise we'd hide the "run braid recover" hint
   //    behind a config-parse error.
   if let Err(msg) = preflight::check_no_pending_operation(&paths) {
       print_cli_error(&msg);
       std::process::exit(1);
   }

   // 2. Load Config under the pool guard. Use a dispatch-local wrap
   //    so users see the same "config error: <details>" text
   //    cmd_remove / cmd_replace etc. produce today.
   let config = load_config_for_cmd_or_exit(Path::new(&config_path), 1);

   // 3. Online snapshot (only the arms that need it).
   let online_snapshot =
       (!args.dry_run && config.systemd_lifecycle())
           .then(|| snapshot(&online_ops));
   ```

   - Step 1 (preflight) is the **critical reorder** vs the current
     plan. Today `plan_add` / `plan_remove` / `plan_remove_missing` /
     `plan_replace` call `preflight::check_no_pending_operation` at
     line ~1 of the function, before `config_read`. With config
     hoisted to dispatch, a bad config would otherwise win over a
     journal-triggered "recovery mode" hint. Putting preflight first
     in dispatch preserves the journal-priority contract verbatim.
   - Step 2 introduces `load_config_for_cmd_or_exit`, a thin local
     variant of `load_config_or_exit` that prefixes the error message
     with `"config error: "`. This matches the
     `<Cmd>Error::Config` Display impls (`cli/src/remove.rs:43`,
     `cli/src/add.rs:94`, plus the analogous `#[from] ConfigError`
     variants in `RemoveMissingError` and `ReplaceError`) that already
     emit `"config error: <details>"` today for Remove / RemoveMissing /
     Replace, and for `braid add --dry-run`. Implementation: add it
     next to `load_config_or_exit` at `cli/src/main.rs:1072`; it
     differs only in the `print_cli_error(&format!("config error:
     {e}"))` call. Reuse `load_config_or_exit` (no prefix) for Status /
     Doctor / Tui / Unlock / Recover / Lock arms where the existing
     surface is prefix-less.
   - **Add has split behavior today** that this change intentionally
     unifies. Today's Add dispatch
     (`cli/src/main.rs:508-509`) only loads `Config` when
     `!args.common.dry_run`, so a broken config on a real-run add
     surfaces through `load_config_or_exit` -- prefix-less -- while
     a broken config on `--dry-run` add falls through to
     `plan_add` and surfaces through `AddError::Config` with the
     `"config error: "` prefix. After this sweep, both Add modes go
     through `load_config_for_cmd_or_exit` and produce the wrapped
     `"error: config error: <details>"` form. The non-dry Add stderr
     wording **does change** -- it gains the `"config error: "`
     prefix it never had. This is intentional: it makes the four
     mutating commands consistent. Call this out in the commit
     message; do not bury it as a "no user-visible change" claim.
   - Standardize on the **Recover/Unlock pattern**: load `Config`
     unconditionally. Drop Add's existing
     `(!args.common.dry_run).then(...)` conditional load. Dry-run
     already needs config to plan (`plan_add` reads it on the dry-run
     path today, just one layer down); the conditional only defers
     the error site.
   - `online_snapshot` is only needed by the arms that thread it into
     `run_with_online_marker` -- Add and (already-done) Recover.
     Remove, RemoveMissing, Replace don't currently snapshot online
     state and stay that way.
   - Pass `config: &config` into the new `*Params`.

4. **Online-marker wiring.** Add's existing dispatch already feeds
   `online_config.as_ref()` (Option) into `run_with_online_marker`'s
   second arg. With unconditional load this becomes
   `(!args.common.dry_run).then_some(&config)` -- the exact pattern
   Recover (cli/src/main.rs:964) and Unlock (cli/src/main.rs:687) use.
   This is the only non-trivial signature touch outside the `*Params`
   change.

5. **ADR + planner doc updates.** Same commit:
   - `docs/decisions/022-dry-run-preview-model.md:30` -- amend the
     "migrated mutating `plan_*()` owns everything above the dry-run
     gate" paragraph to split ownership: dispatch owns config loading
     and pending-op preflight (the journal-priority contract);
     `plan_*` owns live probes, accumulated preview notes, and
     construction of the typed work plan. Note the rationale (pool-lock
     migration moved both the lock and the read-side fences above
     `plan_*` -- this commit finishes that move).
   - Top-of-function doc comments on `plan_add`, `plan_remove`,
     `plan_remove_missing`, `plan_replace` -- update to reflect that
     preflight and config are now caller-provided.

## Test-callsite updates

The patterns:

- **add.rs**: ~45 `AddParams { ... config_path: ..., ... }` literals
  under `#[cfg(test)]`. Most pass `Path::new("/dev/null")` and never
  exercise the read path, so the migration is a mechanical swap to
  `config: &Config::default()` (or the existing test-fixture helper).
  Tests that pass a real config file path and rely on `plan_add` reading
  it (e.g. `cli/src/add.rs:2374-2390` range -- per Explore) need the
  fixture content loaded once and passed by reference.
- **remove.rs / remove_missing.rs**: 0 literal `*Params { config_path:
  ... }` sites -- both go through `PoolFixture::remove_params()` /
  `PoolFixture::remove_missing_params()` builders. Update each builder
  in `cli/src/test_fixtures.rs` once; all tests inherit.
- **replace.rs**: ~4 direct literal `ReplaceParams { ... }` sites plus
  fixture builders; same shape as remove.

Use a shared test-side `Config` value -- either `Config::default()`
where the field values are irrelevant, or a single
`fn test_config() -> Config` helper in `cli/src/test_fixtures.rs` if the
defaults aren't right for the call site. Don't proliferate per-test
configs.

Tests that previously asserted a `ConfigError` propagation from inside
`plan_*` / `cmd_*` need their failure mode reworked: the error now
surfaces from `load_config_for_cmd_or_exit` at the dispatch site,
which calls `process::exit(1)`. If any such tests exist, migrate the
coverage to a VM/integration test that drives the full binary (see
verification step 3 below), or delete if redundant. (Explore did not
find any -- this is a precaution.)

**Pinned ordering test to migrate.**
`plan_add_pending_op_wins_over_locked_pool_refusal`
(`cli/src/add.rs:8760`) currently asserts that when both
`pending-op.json` and a locked pool with non-empty membership are
present, `plan_add` returns the pending-op error rather than the
locked-pool refusal. The test exists specifically to pin the
ordering between `check_no_pending_operation` and
`check_pool_unlocked_if_membership_exists` inside `plan_add`. After
this sweep, preflight no longer runs inside `plan_add` -- the test
would either fail compile (if `plan_add` no longer threads paths the
same way) or pass for the wrong reason. Replace it with a
**dispatch-level** test that drives the real `braid add` binary
under both `pending-op.json` and a locked pool, asserts the
pending-op stderr wording wins, and asserts the locked-pool
"not unlocked" string is absent. The new test belongs in
`tests/module/` (the existing pending-op fixture VMs are the natural
home) and carries the same Intent / Why-it-exists / Scenario
preamble. Do not silently delete the original -- the ordering
contract it pinned moves to dispatch; the assertion moves with it.

## Critical files

- `cli/src/main.rs` -- four dispatch arms (`Commands::Add` at 500,
  `Commands::Remove` at 550, `Commands::RemoveMissing` at 576,
  `Commands::Replace` at 602). Add `load_config_for_cmd_or_exit`
  alongside `load_config_or_exit` at `cli/src/main.rs:1072`. Reuses
  `run_with_online_marker` (already at use sites for
  Add/Recover/Unlock) and `preflight::check_no_pending_operation`
  (`cli/src/preflight.rs`).
- `cli/src/add.rs` -- `AddParams` (line 808), `plan_add` (line 1486).
- `cli/src/remove.rs` -- `RemoveParams`, `plan_remove` (line 476),
  `cmd_remove` (line 608).
- `cli/src/remove_missing.rs` -- `RemoveMissingParams`,
  `plan_remove_missing`, `cmd_remove_missing`.
- `cli/src/replace.rs` -- `ReplaceParams`, `plan_replace`,
  `cmd_replace` (line 1169 is the `config_read` site).
- `cli/src/test_fixtures.rs` -- `PoolFixture::remove_params()`,
  `remove_missing_params()`, any equivalent helpers for replace/add.
- `docs/decisions/022-dry-run-preview-model.md` -- amend the "Decision"
  paragraph at line 30 to reflect dispatch-owned preflight + config.

Reuse:

- `load_config_or_exit(path, exit)` (cli/src/main.rs:1072) -- existing
  one-line helper used by Unlock, Recover, Lock dry-run, Status, Tui,
  ScrubMount, ScrubCancel. Keep using it where it's already used.
- `preflight::check_no_pending_operation(&paths)`
  (`cli/src/preflight.rs`) -- the exact function `plan_add` and
  `plan_remove` call today; lift the call site, not the implementation.
- `run_with_online_marker(snapshot, config, ops, body)`
  (cli/src/main.rs, used at lines 520/685/962). Keep its
  `Option<&Config>` signature -- the change is just what we feed it.
- `Config::default()` if it exists, or add a single test helper.
  Don't construct per-test configs inline.

## Verification

1. `just test-rust` -- the entire `cli/src/{add,remove,remove_missing,replace}.rs`
   unit-test surface (`#[cfg(test)]` modules) must pass after the
   `*Params` field swap. This is the high-churn lane; failures here are
   almost certainly mechanical.

2. **New unit / VM regression matrix: journal-priority contract.**
   Reordering preflight + config load is the single highest-risk piece
   of this refactor. Cover it explicitly. For each of
   `add`, `remove`, `remove-missing`, `replace`, add an assertion that:

   | pending-op.json | config file       | expected stderr                                          | exit |
   | --------------- | ----------------- | -------------------------------------------------------- | ---- |
   | present         | corrupt or missing | the "recovery mode" guidance from `check_no_pending_operation` -- NOT a config-error line | 1    |
   | absent          | corrupt or missing | `error: config error: <ConfigError Display>`             | 1    |
   | present         | valid              | the "recovery mode" guidance (unchanged from today)      | 1    |

   Implement as VM tests (`tests/module/...`) driving the real
   `braid` binary so the dispatch-side ordering is the thing under
   test. Reuse the existing pending-op fixtures the
   `preflight::check_no_pending_operation` tests already build on.
   Unit-level coverage of `plan_*` alone cannot catch a dispatch-side
   reorder regression.

3. **Config-error message surface: pin it.** Add at least one
   assertion per mutating arm that the `error: config error: <details>`
   exact prefix lands on stderr when the config file is invalid.
   For Add specifically, pin **both** `--dry-run` and non-dry modes
   to the wrapped prefix -- today's non-dry Add path produces the
   raw `ConfigError` Display without `"config error: "`, so this
   assertion is what locks in the intentional standardization (see
   "Add has split behavior today" in step 3 of the refactor pattern).
   For Remove / RemoveMissing / Replace, both modes already produce
   the wrapped prefix today; the assertion ensures
   `load_config_for_cmd_or_exit` keeps it that way. A single shared
   helper that iterates `(subcommand, dry_run_flag)` tuples against
   a known-broken `--config` path is fine -- it doesn't need eight
   separate fixture trees.

4. `just test-vm` -- the full NixOS VM test suite. Required because the
   dispatch-side change affects real-binary behavior (exit codes,
   error-message routing, online-snapshot wiring). Specifically watch:
   - `tests/module/pool-lock-*-contention.py` -- pool-lock semantics
     under the dispatch arms we're touching.
   - Any test that asserts behavior of Remove / RemoveMissing /
     Replace / Add `--dry-run` on a missing/corrupt config -- those
     now exit at `load_config_for_cmd_or_exit` rather than later in
     `plan_*`. The `"config error: "` prefix is preserved by
     `load_config_for_cmd_or_exit`, so the stderr text matches
     today's wrapped output verbatim; only the source-level call site
     moves. Expect no test adjustments needed; flag any that fail.
   - Tests that assert non-dry `braid add` stderr on a corrupt
     config: today they observe a **prefix-less** `ConfigError`
     because dispatch's `load_config_or_exit` for Add doesn't wrap.
     After this sweep, non-dry Add gains the `"config error: "`
     prefix and aligns with the dry-run path. If any such test
     exists (Explore did not find one), update its expected stderr
     in the same commit.

5. Manual smoke (after VM tests are green): `cargo run -- add --dry-run
   d1=/dev/disk/by-id/...` against a fixture config -- confirm no
   regression in dry-run UX, no double-read shows up in strace if anyone
   bothers checking.

End state: `grep config_read cli/src/{add,remove,remove_missing,replace}.rs`
returns nothing, and `grep "check_no_pending_operation"
cli/src/{add,remove,remove_missing,replace}.rs` returns nothing.
The only call sites for both helpers in the CLI live in
`cli/src/main.rs` plus the read-only / non-pool-lock arms.
ADR 022's "Decision" paragraph reflects the dispatch-vs-planner split.
