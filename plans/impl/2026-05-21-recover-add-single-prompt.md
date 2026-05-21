# Plan: single-prompt fix for `execute_add_pool_mutation_recovery`

## Context

When a user runs `sudo braid recover` interactively (no
`--passphrase-stdin` / `--passphrase-file`) on an **already-mounted** pool
with an **interrupted add** -- specifically, a crash that left at least
one journaled target with a closed mapper AND at least one target whose
`pool_add_device` hadn't completed -- `execute_add_pool_mutation_recovery`
in `cli/src/recover.rs` prompts the operator for the LUKS passphrase
**twice** in succession.

The double prompt happens because:

- `discover_add_targets_before_mount` at `cli/src/recover.rs:2082-2087`
  early-returns `Ok(None)` when the pool is already mounted, leaving
  `pre_resolved_credential = None`.
- `execute_recover_initial_open` at `cli/src/recover.rs:944-969` only
  eagerly resolves `state.credential` when `open_plan.is_some()`. With a
  mounted pool, `open_plan` is `None`, so `state.credential` stays `None`.
- Inside `execute_add_pool_mutation_recovery`, two independent code blocks
  each call `recover_passphrase(credential=None, params)`:
    - **Discovery block** (`:2406-2479`): the local `passphrase` cache at
      `:2408` resolves once via `:2455` when a closed mapper is found.
    - **Replay block** (`:2481-2490`): unconditionally calls
      `recover_passphrase` again at `:2482`, ignoring whatever the
      discovery block cached.

Both prompts authenticate against the same LUKS slot 0 -- this is **UX
only**, not a correctness bug. But it contradicts Principle 4
([`docs/decisions/004-single-passphrase.md`](../../docs/decisions/004-single-passphrase.md))
which commits to "one passphrase, all drives unlock".

The bug is isolated to `execute_add_pool_mutation_recovery`. The
analogous `finish_uncommitted_replace_recovery` at `:2960-3128` is safe
because its two `recover_passphrase_for_context` calls (at `:3020` and
`:3080`) live in mutually exclusive `match` arms.

Intended outcome: an interactive `sudo braid recover` on an
already-mounted pool with any shape of interrupted-add journal prompts
the operator exactly once, with a mechanical unit test that prevents
regression.

## Approach

Two pieces, both applied together:

**Piece A -- hoist the passphrase cache to function scope.** Move
`let mut passphrase: Option<RecoverPassphrase<'_>> = None;` from inside
the discovery `if` block at `cli/src/recover.rs:2408` to function scope
(immediately after the `AddPoolReplayCtx` destructure at `:2396-2402`).
Inside the replay block at `:2481-2490`, replace the unconditional
`let passphrase = recover_passphrase(credential, params)?;` with a
lazy-init pattern mirroring the discovery block:

    if passphrase.is_none() {
        passphrase = Some(recover_passphrase(credential, params)?);
    }
    let passphrase = passphrase
        .as_ref()
        .expect("passphrase was resolved above")
        .expose_secret();

Then feed `passphrase` (now `&Passphrase`) into
`verify_recover_passphrase_for_add_replay` at `:2483` and the downstream
uses at `:2535`, `:2556`, `:2586`, `:2622`, `:2632`. The borrow check is
safe because `RecoverPassphrase<'a>` (defined `:87-100`) is an enum
`{ Borrowed(&'a Passphrase), Owned(Passphrase) }` with no `Drop`, and
both blocks share the same `credential: Option<&'a OpenCredential>`
lifetime.

**Piece B -- plumb a `&dyn PassphraseReader` seam through `RecoverParams`
so the fix has mechanical regression coverage.** The seam already exists
in production: `luks::read_passphrase_with(file, stdin, confirm_new,
tty: &dyn PassphraseReader)` at `cli/src/luks.rs:276-305` accepts the
trait directly, but `recover_passphrase` and
`recover_passphrase_for_context` currently bypass it by calling the
no-arg `read_passphrase`. Switching them to `read_passphrase_with` and
adding the `tty` field to `RecoverParams` surfaces the existing trait,
not a new abstraction.

## Files to modify

### `cli/src/recover.rs`

- **Struct field**: add `pub tty: &'a dyn luks::PassphraseReader,` to
  `RecoverParams<'a>` near `:202-220` (alongside `sleep_inhibitor` and
  `sleeper`). The struct already has `<'a>`; the new field fits the same
  shape as the existing trait-object fields.
- **`recover_passphrase`** at `:2036-2050`: change the `None` arm at
  `:2045-2048` from `luks::read_passphrase(params.passphrase_file,
  params.passphrase_stdin)` to `luks::read_passphrase_with(
  params.passphrase_file, params.passphrase_stdin, false, params.tty)`.
- **`recover_passphrase_for_context`** at `:2884-2899`: same swap at
  `:2894-2897`.
- **`execute_add_pool_mutation_recovery`** at `:2389-2693`: apply Piece A
  exactly as described above. Remove the now-redundant local declaration
  at `:2408`; keep the lazy `if passphrase.is_none()` guard at `:2454`
  unchanged (now mutates the function-scope binding instead of a local).

### `cli/src/main.rs`

- The sole production `RecoverParams { ... }` literal lives near
  `:971-982`. Add `tty: &braid_cli::luks::RealTty,` to the struct
  literal. `RealTty` is already `pub` at `cli/src/luks.rs:163` and the
  `luks` module is re-exported via `pub mod luks;` in `cli/src/lib.rs`.
  No visibility bumps required.

### `cli/src/test_fixtures/recover.rs`

- **`RecoverParamsBuilder`** at `:67-76`: add
  `tty: &'a dyn luks::PassphraseReader,` field.
- **`PoolFixture::recover_params`** at `:49-60`: seed the builder default
  with `tty: &crate::luks::RealTty` (zero-sized unit struct coerces to
  any `'a`, same shape as `RECOVER_NOOP_INHIBITOR` at `:41`).
- **Builder method**: add
  `pub(crate) fn tty(mut self, tty: &'a dyn luks::PassphraseReader)
  -> Self { self.tty = tty; self }` matching `sleep_inhibitor` at
  `:94-97`.
- **`build()`** at `:99-112`: pass `tty: self.tty,` into the returned
  `RecoverParams`.

No other `RecoverParams { ... }` literal exists in the workspace (`rg
'RecoverParams \{' cli/` returns two hits: `main.rs` and the builder's
`build()`).

## Reuse, don't reinvent

- **`PassphraseReader` trait** at `cli/src/luks.rs:155-160` (already
  `pub`).
- **`RealTty`** singleton at `cli/src/luks.rs:163-180` (already `pub`).
- **`read_passphrase_with`** at `cli/src/luks.rs:276-305` (already `pub`).
- **`ScriptedPassphraseReader`** at `cli/src/luks.rs:226-256`
  (`#[cfg(test)] pub(crate)`) -- queue-based reader with a `remaining()`
  count, already used by `cli/src/add.rs` tests (e.g. `:2258, 7286, 7356`
  and the `assert_eq!(tty.remaining(), 0, ...)` pattern at
  `cli/src/add.rs:7323`). The recover test reuses it directly.
- **`TEST_PASSPHRASE_BYTES`** in `cli/src/test_fixtures/shared.rs`
  (already imported into the `recover.rs` test mod).

## Test design

**Location**: `cli/src/recover.rs` test module, adjacent to the existing
`live_add_recovery_*` tests at `:5837-6004`.

**Name**: `live_add_recovery_prompts_passphrase_once_when_mapper_closed`.

**Required preamble** (matches the existing tests in this mod, e.g.
`:5826-5836`, and the form specified in
[`docs/testing.md:13-22`](../../docs/testing.md)):

    // Intent
    // Live-add recovery on an already-mounted pool with a closed mapper
    // and a pending pool_add_device prompts for the LUKS passphrase
    // exactly once, not twice.
    //
    // Why it exists
    // Principle 4 (docs/decisions/004-single-passphrase.md) commits to
    // "one passphrase, all drives unlock". Independent recover_passphrase
    // calls in the discovery and replay blocks of
    // execute_add_pool_mutation_recovery used to prompt twice; a future
    // refactor could reintroduce the double prompt without this guard.
    //
    // Scenario
    // sudo braid recover on a mounted pool with an interrupted add: disk2
    // mapper is closed (discovery must open it) AND disk2 is not yet a
    // pool member (replay must run pool_add_device). The operator sees
    // one prompt, the replay reuses the cached passphrase, btrfs device
    // add commits the target.

### Mapper-state modeling (the F1 fix)

The discovery block calls `CryptsetupStatus(braid-disk2)` twice in
succession (once in `probe_config_disk` at `recover.rs:2413`, once again
inside `ensure_luks_open` -> `classify_mapper_ownership` at
`luks.rs:837`). Both must return **inactive**. The replay block then
re-probes via `probe_config_disk` at `recover.rs:2507` and must see
**active** (the discovery-block `cryptsetup luksOpen` succeeded).

`MockRunner::with_output` (`cmd.rs:1368`) is a HashMap insert -- one
static output per request key -- so chaining `.with_mapper_open(...)`
then `.with_mapper_closed(...)` on the same mapper would lose one of
the two states. The harness must model time, not just state.

Use `with_output_sequence` (`cmd.rs:1373`) to pop closed responses for
the first two calls, then fall back to the static active response that
`with_mapper_open` (already in
`replay_returned_disk2_runner_base`) installs. `dispatch` at
`cmd.rs:1411-1432` consumes the sequence first and falls back to
`outputs` on empty queue -- exactly the active-fallback shape the
reviewer described.

Compose **over** the existing full helper
`replay_returned_disk2_runner_for_devid4()` at `:5814` so the
post-replay pool/balance/probe stubs come along for free. No new
base-only helper:

    fn replay_returned_disk2_runner_closed_mapper_for_devid4() -> MockRunner {
        let inactive = RawCommandOutput {
            cmd: "cryptsetup status braid-disk2".into(),
            stdout: String::new(),
            stderr: "/dev/mapper/braid-disk2 is inactive.\n".into(),
            exit_status: 4,
        };
        replay_returned_disk2_runner_for_devid4()
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("braid-disk2".into()),
                },
                vec![inactive.clone(), inactive],
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
    }

The two `inactive` queue entries cover the discovery
`probe_config_disk` + `classify_mapper_ownership` pair; the third and
later `CryptsetupStatus(braid-disk2)` calls (replay probe, plus
anything in the existing `with_disk1_disk2_devid4_pool_probe` /
`with_balance_replay` overlays) fall through to the active static
output that `with_mapper_open` left in the base.

### Fixture skeleton (sketch, not literal code)

    let f = PoolFixture::empty();
    let journal = recoverable_pool_mutation_add_journal();
    journal::write_journal(&f.paths, &journal).unwrap();
    let union = union_memberships(&journal);
    let targets = match &journal.op { OpKind::Add { targets, .. } => targets,
                                      _ => unreachable!() };
    let runner = replay_returned_disk2_runner_closed_mapper_for_devid4();
    let resolver = resolver_for(&[
        ("/dev/vda", "virtio-disk1"),
        ("/dev/vdb", "virtio-disk2"),
    ]);
    let pass = std::str::from_utf8(TEST_PASSPHRASE_BYTES).unwrap();
    let reader = ScriptedPassphraseReader::new([pass, pass]);
    let params = f.recover_params()
        .passphrase_file(None)   // force the TTY branch
        .tty(&reader)
        .build();

    execute_add_pool_mutation_recovery(
        &runner,
        &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]),
        &resolver,
        &params,
        AddPoolReplayCtx {
            credential: None,    // matches the un-discovered-credential case
            journal: &journal,
            union: &union,
            targets,
            pool: pool_state_one_disk(),
        },
    ).expect("recovery succeeds with a single prompt");

    // Replay-block-ran proof (F2): mirrors the BtrfsDeviceAdd assertion
    // at recover.rs:5871-5877. A setup mistake that exits in the
    // discovery block (e.g. probe never sees disk2 as not-live) would
    // pass remaining()==1 but fail this assertion.
    assert!(
        runner
            .requests()
            .iter()
            .any(|r| matches!(r,
                CmdRequest::BtrfsDeviceAdd { device, .. }
                if device == "/dev/mapper/braid-disk2"
            )),
        "replay block must reach pool_add_device for disk2"
    );

    assert_eq!(reader.remaining(), 1,
        "passphrase must be prompted exactly once -- second prompt is a Principle-4 regression");

**Why this test catches the regression**: seeded with two identical
passphrases. Pre-fix, the replay block at `recover.rs:2482` calls
`recover_passphrase` independently of the discovery cache and pops a
second passphrase from the queue, so `remaining() == 0` and the
assertion fails. Post-fix, the replay block reuses the cached binding,
no second pop, `remaining() == 1`, assertion passes. The
`BtrfsDeviceAdd` assertion is the independent witness that the replay
block actually ran -- without it, a harness mistake that completed in
the discovery block alone would false-pass `remaining() == 1`.

**Optional companion test**:
`live_add_recovery_completes_with_single_passphrase_when_seeded_with_one`
using `ScriptedPassphraseReader::new([pass])` -- the stronger statement
that one passphrase truly suffices. Cheap to add, complements the
`remaining() == 1` check.

## Risks and gotchas

- **Existing tests must keep compiling**. Every `recover_params()` call
  in the test mod (50+ call sites) goes through the builder; the new
  default `tty: &RealTty` is wired in once at `:49-60` and propagates.
  No call site needs editing unless it wants to substitute. The
  `RealTty` default reads `/dev/tty` if invoked, but every existing test
  also sets `passphrase_file: Some(self.pass_path.as_path())` via the
  default at `:54`, which short-circuits the TTY branch at
  `cli/src/luks.rs:316-319`. No regression.
- **Sync/Send**: `execute_add_pool_mutation_recovery` requires
  `R: CommandRunner + Sync` only on the runner, not on params. Existing
  `RecoverParams` already holds non-`Sync` trait objects (`&dyn
  AcquireSleepInhibitor`); `ScriptedPassphraseReader` uses `RefCell`
  (`!Sync`) and that's fine -- grep confirmed no
  `rayon`/`par_iter`/`thread::spawn` in `cli/src/recover.rs` or its
  callees.
- **Replay-block variable shadowing**: today `:2482` introduces
  `let passphrase = recover_passphrase(...)?;` as a fresh binding (no
  outer scope to collide with). After Piece A, the outer
  `Option<RecoverPassphrase<'_>>` is in scope. Use
  `let passphrase = passphrase.as_ref().expect(...).expose_secret();`
  *once* just before the verify call, so every `passphrase.expose_secret()`
  in the for-loop body at `:2496-2680` becomes a direct
  `passphrase` reference -- minor mechanical rewrite, no semantic change.
- **Piece B applies to `recover_passphrase_for_context` too**: replace
  recovery (`finish_uncommitted_replace_recovery`) doesn't have the
  double-prompt bug, but the same `read_passphrase` -> `read_passphrase_with`
  swap is consistent and enables future tests on the replace path. In
  scope, zero added risk.

## Verification

1. **Build**: `just test-rust` -- confirms the trait-object plumbing
   compiles across `main.rs`, `recover.rs`, `test_fixtures/recover.rs`.
2. **Targeted unit test**:
   `cargo test -p braid-cli live_add_recovery_prompts_passphrase_once`
   (alternatively run all recover tests:
   `cargo test -p braid-cli recover::`).
3. **Full Rust suite**: `just test-rust` again to confirm no existing
   test regressed.
4. **Regression-catching evidence**: locally revert Piece A only (keep
   Piece B), re-run the new test; it must turn red (`remaining()` would
   be `0`, not `1`). Restore Piece A; it must turn green again.
5. **Manual integration check** (optional but recommended given the
   UX-only nature of the fix): on a NixOS VM, deliberately interrupt a
   `braid add` after the first target is opened but before
   `pool_add_device` commits, leave the pool mounted, then run
   `sudo braid recover` with no flags. The operator should see exactly
   one `LUKS passphrase:` prompt, not two.
6. **VM tests**: `just test-vm` -- no recover VM test currently exercises
   the interactive-TTY path, so no change is expected. The fix is
   protected by the new unit test.

## Critical files

- `cli/src/recover.rs` (Piece A + Piece B field + helper swaps + new test)
- `cli/src/main.rs` (production `tty: &RealTty`)
- `cli/src/test_fixtures/recover.rs` (builder field + `tty(...)` method
  + default seed)
- `cli/src/luks.rs` (reference only -- no edits; trait/`RealTty`/
  `read_passphrase_with`/`ScriptedPassphraseReader` are reused as-is)
