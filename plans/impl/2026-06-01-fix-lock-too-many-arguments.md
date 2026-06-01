# Fix clippy `too_many_arguments` in `cmd_lock_impl_with_notes`

## Context

`cargo clippy` flags `cmd_lock_impl_with_notes` (`cli/src/lock.rs:1224`) at 8/7
arguments. The fix collapses the three behavioral knobs into a small struct,
dropping the signature to 6 args.

Two scoping decisions, both deliberate:

- **Keep the three generic DI handles (`runner`/`fs`/`sleeper`) as separate
  params.** Folding them into a struct would force a `&dyn` conversion of the
  generic `plan_lock` / `LockPlan::execute` / `RealOnlineStateOps::new(runner)`
  path -- large blast radius and object-safety risk for no real benefit. The
  whole module threads DI as generics; lock should stay consistent.
- **Bundle `dry_run` + `extra_notes` + `mode`, not `config`/`membership`.**
  Those three flags are exactly the axis along which the helper's three call
  sites differ, so they form a cohesive unit ("how to perform this lock").
  `config`/`membership` are the universal "which pool" pair threaded as explicit
  params throughout `lock.rs` (`plan_lock`, `run_lock_pre_steps`, `mark_offline`,
  `cmd_lock_orchestrate_impl`); bundling them would fight that idiom, and a
  `LockParams`-style name would falsely imply parity with the public-boundary
  `*Params` structs (`AddParams`, `RemoveParams`), which are the *public* cmd's
  args and include DI as `&dyn`.

Intended outcome: warning gone, behavior identical, change confined to
`lock.rs`, no public-signature or test churn.

## Change

All edits in `cli/src/lock.rs`.

### 1. New private `LockOptions` struct

Place it just above `cmd_lock` (~line 1103), with a `///` doc comment per
AGENTS.md (justify the boundary, not the signature). `LockMode` is the enum at
`lock.rs:90`; `PreviewNote` is already imported at `lock.rs:10`.

```rust
/// The behavioral knobs that distinguish braid's lock entry points -- a user
/// dry-run/exec versus the systemd ExecStop shutdown path. Bundled so the
/// shared lock body stays under clippy's argument-count limit while the
/// load-bearing DI handles and the pool's config/membership stay explicit.
struct LockOptions {
    dry_run: bool,
    extra_notes: Vec<PreviewNote>,
    mode: LockMode,
}
```

Private (used only within `lock.rs`); no derives needed (constructed and moved
once per call site).

### 2. Retarget `cmd_lock_impl_with_notes` (line 1224)

Replace the trailing `dry_run: bool, extra_notes: Vec<PreviewNote>, mode: LockMode`
params with a single `opts: LockOptions`. Rewrite the four uses in the body:

- `if !dry_run {` -> `if !opts.dry_run {`
- `plan_lock(runner, fs, config, membership, mode)` -> `... opts.mode)`
- `plan.notes.splice(0..0, extra_notes);` -> `plan.notes.splice(0..0, opts.extra_notes);`
- `if dry_run {` -> `if opts.dry_run {`

No borrow trouble: `dry_run`/`mode` are `Copy`, `extra_notes` is moved exactly
once after its last `dry_run` read. Keep the existing doc comment -- its
rationale (dispatch-supplied notes joining preview notes without changing the
test-facing helper arity) still holds: `cmd_lock_impl` keeps its 6-arg shape.

### 3. Update the 3 call sites (all in `lock.rs`)

Construct `LockOptions { .. }` as the final argument:

- `cmd_lock` (line 1111): `LockOptions { dry_run, extra_notes, mode: LockMode::User }`
- `cmd_lock_systemd_stop` (line 1133): `LockOptions { dry_run: false, extra_notes: Vec::new(), mode: LockMode::SystemdStop }`
- `cmd_lock_impl` (test helper, line 1210): `LockOptions { dry_run, extra_notes: Vec::new(), mode: LockMode::User }`

No change to the public signatures (`cmd_lock`, `cmd_lock_systemd_stop`), to the
`#[cfg(test)]` `cmd_lock_impl` 6-arg shape, or to any test -- tests call
`cmd_lock_impl`, never the helper directly (confirmed: only 3 callers exist, all
above).

## Verification

- `cargo clippy -p braid-cli --all-targets` (crate is `braid-cli`) -- confirm the
  `too_many_arguments` warning at `lock.rs` is gone and no new warnings appear.
- `just test-rust` -- the lock unit tests (`cli/src/lock.rs` `mod tests`) compile
  and pass, proving the `cmd_lock_impl` -> `LockOptions` rewrite is behavior-preserving.
- No VM tests required: the change is pure-Rust, behavior-preserving, and
  contained to `lock.rs` (no systemd/lifecycle/mount blast radius).
