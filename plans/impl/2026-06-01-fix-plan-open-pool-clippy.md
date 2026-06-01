# Fix clippy `too_many_arguments` on `plan_open_pool_inner`

## Context

Clippy warns that `plan_open_pool_inner` in `cli/src/mount.rs:191` has 8
arguments (limit 7):

```
warning: this function has too many arguments (8/7)
   --> cli/src/mount.rs:191:1
```

The fix is to bundle arguments into a struct (per the user's directive),
matching braid's existing `*Params<'a>` convention (`RecoverParams`,
`UnlockParams`, `AddParams`, ...): bundle the **non-generic borrowed data**
and keep the `R`/`F` generics and the `&mut events` accumulator as separate
arguments.

**Scope: private inner function only.** The clippy warning fires solely on the
private `plan_open_pool_inner` (8 args). Its public wrapper `plan_open_pool`
sits at 7 args and is *not* flagged. We introduce a **private**
`PlanOpenPoolParams<'a>`, use it only across the wrapper->inner hop, and leave
the public `plan_open_pool` signature (and its 7 call sites) untouched. This is
the smallest change that clears the lint, adds no call-site churn to the
`unlock`/`recover` hot paths or their tests, and avoids re-packing fields that
prod callers already hold on their own `UnlockParams`/`RecoverParams`.

## Changes (single file: `cli/src/mount.rs`)

### 1. Add a private `PlanOpenPoolParams<'a>` struct

Place it next to `PlanReport` (~line 153). Mirror `RecoverParams`'s field
style (borrowed data, single `'a`; `&'a dyn BackingPathResolver` exactly as
`RecoverParams` declares it). Include a `///` doc comment per AGENTS.md (new
top-level type), justifying the boundary -- not restating fields.

```rust
/// Read-only planning inputs threaded from `plan_open_pool` into
/// `plan_open_pool_inner`. Bundled so the probe planner stays under
/// clippy's argument limit while `runner`/`fs` (generic) and the
/// `events` accumulator stay separate per the `*Params` convention.
struct PlanOpenPoolParams<'a> {
    config: &'a Config,
    membership: &'a PoolMembership,
    backing_path_resolver: &'a dyn BackingPathResolver,
    allow_degraded: bool,
    command_hint: &'a str,
}
```

### 2. Reduce `plan_open_pool_inner` to 4 args

```rust
fn plan_open_pool_inner<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &PlanOpenPoolParams<'_>,
    events: &mut Vec<ProbeEvent>,
) -> Result<Option<OpenPlan>, MountError> {
```

### 3. Re-prefix the 5 bundled-field reads in the `_inner` body

Narrow edits only (no formatter run, per AGENTS.md):

- `cli/src/mount.rs:201` `config.mount_point()` -> `params.config.mount_point()`
- `cli/src/mount.rs:220` `membership.iter_by_name()` -> `params.membership.iter_by_name()`
- `cli/src/mount.rs:228` `backing_path_resolver` (arg to `probe::probe_config_disk`) -> `params.backing_path_resolver`
- `cli/src/mount.rs:296` `!allow_degraded` -> `!params.allow_degraded`
- `cli/src/mount.rs:299` `command_hint` (arg to `format_degraded_refused`) -> `params.command_hint`

`runner`, `fs`, and `events` references are unchanged (still separate args).

### 4. Build the struct in the `plan_open_pool` wrapper

Public signature unchanged; the wrapper is the adapter that translates its
positional params into the struct before calling `_inner`:

```rust
pub fn plan_open_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    backing_path_resolver: &dyn BackingPathResolver,
    allow_degraded: bool,
    command_hint: &str,
) -> PlanReport {
    let mut events = Vec::new();
    let params = PlanOpenPoolParams {
        config,
        membership,
        backing_path_resolver,
        allow_degraded,
        command_hint,
    };
    let result = plan_open_pool_inner(runner, fs, &params, &mut events);
    PlanReport { events, result }
}
```

## Out of scope (deliberately unchanged)

- Public `plan_open_pool` signature and all 7 call sites: `unlock.rs:203`,
  `recover.rs:1288`, `recover.rs:3541`, `mount.rs:1110/1811/2004`,
  `test_fixtures/mount.rs:413`.
- No `#[allow(clippy::too_many_arguments)]` (struct is cleaner; user asked for
  the struct route).

## Verification

1. **Lint cleared:** run clippy the way it surfaced the warning (e.g.
   `cargo clippy` over the `braid-cli` crate / `--all-targets`). Confirm the
   `too_many_arguments` warning on `mount.rs` is gone and no new warnings
   appear.
2. **Behavior preserved:** `just test-rust`. The existing `mount.rs` unit tests
   that drive `plan_open_pool` (the planner tests around lines 1110/1811/2004,
   plus the `test_fixtures/mount.rs` helper) exercise this exact path through
   the public API; they must still pass unchanged. This is a pure internal
   refactor with no behavior change.
3. **No VM tests required.** Per AGENTS.md test-scope guidance this is a small,
   localized, behavior-preserving change confined to one file's internal
   plumbing; the Rust unit tests cover it.
