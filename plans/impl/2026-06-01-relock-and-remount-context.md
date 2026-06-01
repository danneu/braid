# Fix clippy `too_many_arguments` on `relock_and_remount`

## Context

Clippy warns that `relock_and_remount` in `cli/src/recover.rs:3414` takes 9
arguments (limit 7):

```
warning: this function has too many arguments (9/7)
    --> cli/src/recover.rs:3414:1
```

The fix is to group the call-specific inputs into a struct, per the user's
instruction ("use struct over positional args"). The `#[allow(clippy::too_many_arguments)]`
escape hatch is explicitly *not* wanted here (the codebase reserves it for the
two 11-15 arg recovery functions at `recover.rs:3052` and `:3181`).

The recover.rs module already has the exact pattern to follow:
`AddPoolReplayCtx<'a>` (`recover.rs:2291-2301`) and `ReplaceFinishCtx<'a>` are
private, lifetime-only, doc-commented structs that bundle the per-phase inputs
while their functions keep `runner: &R, fs: &F` as leading positional args
(e.g. `execute_add_pool_mutation_recovery(runner, fs, by_id_resolver, params, ctx)`).
We mirror that convention exactly.

## Design

Keep `runner` and `fs` as positional generic args (matches every sibling
recovery function in the file) and bundle the remaining 7 args into a new
private `RelockAndRemountCtx<'a>`. This drops the count to **3 args**
(`runner, fs, ctx`) and avoids putting generic type params on the struct.

Rejected alternative: reusing the existing `RecoverParams` (which already holds
`sleeper`/`config`/`backing_path_resolver`/`allow_degraded`). The 4 test
callsites do not build a `RecoverParams` -- they pass those 4 fields
individually -- so reusing it would force each test to fabricate 7 unrelated
fields (`paths`, `tty`, `progress`, etc.) and would misrepresent the function's
true dependency surface. A dedicated ctx struct is the right granularity.

## Changes -- all in `cli/src/recover.rs`

### 1. Add the struct (just above the `relock_and_remount` doc comment, ~line 3404)

Mirror `AddPoolReplayCtx`'s style: private, single lifetime, private fields,
`///` doc comment (required by AGENTS.md for new top-level types).

```rust
/// Bundles the call-specific inputs for `relock_and_remount` so the recovery
/// remount cycle keeps the `runner, fs, ctx` positional shape shared with the
/// sibling recovery phases (`AddPoolReplayCtx`, `ReplaceFinishCtx`) and stays
/// under clippy's argument-count threshold.
struct RelockAndRemountCtx<'a> {
    sleeper: &'a dyn Sleeper,
    config: &'a Config,
    membership: &'a PoolMembership,
    backing_path_resolver: &'a dyn BackingPathResolver,
    allow_degraded: bool,
    credential: &'a OpenCredential,
    close_names: &'a [DiskName],
}
```

### 2. Change the function signature + destructure at the top of the body

Replace the 7 bundled params with `ctx`, then destructure on the first line so
the existing 167-line body (lines 3425-3580) stays **byte-for-byte unchanged**
(all field names match the old param names). All 7 fields are used in the body,
so the destructure produces no unused-binding warnings.

```rust
fn relock_and_remount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    ctx: RelockAndRemountCtx<'_>,
) -> Result<(), RecoverError> {
    let RelockAndRemountCtx {
        sleeper,
        config,
        membership,
        backing_path_resolver,
        allow_degraded,
        credential,
        close_names,
    } = ctx;
    let color_enabled = color_enabled_for_stderr();
    // ... rest of body unchanged ...
}
```

### 3. Update the 1 production callsite (`recover.rs:475-485`)

```rust
relock_and_remount(
    runner,
    fs,
    RelockAndRemountCtx {
        sleeper: params.sleeper,
        config: params.config,
        membership: &recovery_mount_membership,
        backing_path_resolver: params.backing_path_resolver,
        allow_degraded: params.allow_degraded,
        credential: cred,
        close_names,
    },
)?;
```

### 4. Update the 4 test callsites (same module, so private struct/fields are accessible)

`recover.rs:13183`, `:13339`, `:13494`, `:13645`. Each keeps `&runner, &fs`
positional and wraps the rest in the struct literal, e.g.:

```rust
relock_and_remount(
    &runner,
    &fs,
    RelockAndRemountCtx {
        sleeper: &progress::NoopSleeper,        // test 4 uses its RecordingSleeper instead
        config: &config,
        membership: &membership,
        backing_path_resolver: &backing_path_resolver,
        allow_degraded: false,
        credential: &OpenCredential::Passphrase(/* ... */),
        close_names: &close_names,
    },
)
```

## Notes

- No behavior change; no doc/principle updates needed (this is an internal
  refactor with no user-facing or invariant impact).
- All types referenced (`Sleeper`, `Config`, `PoolMembership`,
  `BackingPathResolver`, `OpenCredential`, `DiskName`) are already in scope in
  `recover.rs` -- they appear in the current signature.

## Verification

1. **Scoped clippy check.** The crate already has *other* un-suppressed
   `too_many_arguments` warnings -- e.g. `plan_enroll` (`enroll_key_file.rs:620`,
   8 args) and several `lock.rs` functions -- and only two functions carry
   `#[allow(clippy::too_many_arguments)]` (`recover.rs:3052`, `:3181`). So a
   crate-wide `-D clippy::too_many_arguments` would fail even *after* this fix,
   and plain `just clippy` is no gate either (the recipe has no `-D`, so it exits
   0 regardless of warnings). Instead, scope the gate to this one function -- it
   must **fail on a match** (warning still present), pass otherwise:

   ```sh
   log=$(mktemp)
   cargo clippy --manifest-path cli/Cargo.toml --tests >"$log" 2>&1
   rc=$?
   if [ "$rc" -ne 0 ]; then   # compile/lint error -- the --tests callsites don't build
       cat "$log" >&2
       echo "FAIL: cargo clippy exited $rc" >&2
       exit "$rc"
   fi
   if rg -A4 'this function has too many arguments' "$log" | rg -q 'fn relock_and_remount'; then
       echo "FAIL: relock_and_remount still trips too_many_arguments" >&2
       exit 1
   fi
   echo "OK: relock_and_remount no longer trips too_many_arguments; --tests compiled clean"
   ```

   The `rc` check fails the gate on any compile/lint error, which (with
   `--tests`) is what proves the production callsite and 4 test callsites still
   build -- `cargo clippy` returns non-zero only on a real error, not on
   warnings. The inner `rg -A4` then isolates each `too_many_arguments`
   diagnostic's rendered source snippet; `rg -q 'fn relock_and_remount'` matches
   only *this* function's snippet (other functions' snippets print their own `fn`
   line), so the remaining `plan_enroll`/`lock.rs` warnings don't trip it.
   Scoping by the function name in the snippet -- not a hardcoded
   `recover.rs:NNNN` line -- keeps it robust to the line shift from inserting the
   struct.
2. `just test-rust` -- confirms the 4 `relock_and_remount` tests
   (`recover_remount_cycle_*`) still pass, proving the destructure preserved
   behavior.
