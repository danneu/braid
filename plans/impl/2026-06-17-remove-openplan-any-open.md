# Remove the dead `OpenPlan::any_open` field

## Context

`OpenPlan::any_open` (`cli/src/mount.rs`) is a `bool` that is computed in the
planner and stored on the struct, but **no production code reads the field**. A
repo-wide sweep confirms the only reader is one test assertion
(`mount.rs:1828`); every production dispatch gates on `to_unlock` emptiness
(`unlock.rs:96`, `mount.rs:619/651`), `any_missing_member`
(`scan_and_mount` at `mount.rs:808/838`, `recover.rs:1470`), or `mount_device`.

The field is a leftover. Commit `7df11f45` introduced `first_open_mapper:
Option<DiskName>` to fix the degraded-mount fallback, because a bare bool could
not name *which* mapper was open. `first_open_mapper` took over the load-bearing
job (computing `mount_device`) and left `any_open` orphaned -- it now merely
shadows `first_open_mapper.is_some()` (both are set in the same `*mapper_open`
branch, so they never diverge). It survived because `dead_code` does not lint
`pub` fields on a `pub` struct.

Carrying it invites future readers to wire decisions onto a value whose
invariant nobody maintains, and forces every hand-built `OpenPlan` in tests to
guess a value for it. Removing it changes no behavior.

**Bonus simplicity win:** the one production read is the guard
`if to_unlock.is_empty() && !any_open`. Its sibling `.expect()` message already
states the invariant in terms of `first_open_mapper`
("post-check above guarantees to_unlock or first_open_mapper is non-empty").
Replacing `!any_open` with `first_open_mapper.is_none()` makes the guard, the
`.or(first_open_mapper.as_ref())` fallback, and the `.expect()` message all
refer to the *same* variable -- eliminating the bool that only duplicated it.

Scope correction vs. the originating finding: the finding said to delete "the
`first_open_mapper.is_some()` bookkeeping that only fed it." That is backwards.
`first_open_mapper` does **not** feed `any_open`; it is load-bearing (drives
`mount_device`, guarded by `plan_open_pool_degraded_first_absent_picks_open_mapper`)
and **must stay**. Only the bool is redundant.

## The change

### 1. Planner -- `plan_open_pool_inner` (`cli/src/mount.rs`)

- Delete the struct field: `pub any_open: bool,` (`mount.rs:92`) and its
  doc comment.
- Delete the local `let mut any_open = false;` (`mount.rs:218`) and the
  `any_open = true;` mutation inside the `*mapper_open` branch (`mount.rs:271`).
- Rewire the guard (`mount.rs:286`):
  `if to_unlock.is_empty() && !any_open {` -> `if to_unlock.is_empty() && first_open_mapper.is_none() {`
- Drop `any_open,` from the returned `OpenPlan { .. }` literal (`mount.rs:313`).
- **Do not touch** `first_open_mapper` (decl `:219`, set `:272-274`) or the
  `mount_key`/`mount_device` fallback at `:304-309`.

### 2. Remove the field from all constructors

All sites use explicit struct literals (no `..` spreads, no builders), so each
is a one-line deletion of `any_open: <bool>,`:

- Production: `mount.rs:313` (covered above).
- Tests in `cli/src/mount.rs`: `:896`, `:953`, `:3204`.
- Tests in `cli/src/recover.rs`: `:16451`, `:16490`, `:16550`.
- Helper `direct_two_disk_plan()` in `cli/src/test_fixtures/mount.rs`: `:332`.

### 3. Drop the one test assertion (`mount.rs:1828`)

In `plan_open_pool_degraded_first_absent_picks_open_mapper`, delete
`assert!(plan.any_open, "any_open must be true");`. No coverage is lost: the
same test already asserts the full event sequence (`assert_eq!(report.events,
vec![..])` at `mount.rs:1837`), which pins two `ProbeEvent::DiskAlreadyOpen`
entries. `DiskAlreadyOpen` is emitted only when `mapper_open == true`, so the
events assertion already proves "at least one mapper was already open." The
test's primary assertion (`mount_device == "/dev/mapper/braid-disk2"`) is
untouched.

### 4. Reword the doc-comment prose (`mount.rs:1755`)

The regression-test doc comment says "With `to_unlock` empty and `any_open ==
true`, the old code picked the BTreeMap's first key...". Reword to describe the
condition without the deleted field name, e.g. "With `to_unlock` empty and at
least one mapper already open, the old code picked the BTreeMap's first key...".

### Out of scope -- do not edit

Historical plan docs under `plans/impl/` (and `plans/wip/_old/`) mention
`any_open` in pseudocode/prose. These are point-in-time implementation records;
leave them as-is.

## Verification

- `just test-rust` -- whole suite must stay green (the recipe runs
  `cargo test --lib --bin braid ...`; the CLI crate's package is `braid-cli`,
  not `braid`, so prefer the recipe over `cargo test -p <name>` per
  `justfile#test-rust`). The removal is behavior-preserving; if any reader were
  missed, `cargo build` fails to compile rather than passing silently.
- Targeted regression: `cargo test --lib plan_open_pool_degraded_first_absent_picks_open_mapper`
  (the test is a `#[test]` in the lib) -- confirms the `mount_device` fallback
  and event sequence still hold after the guard rewrite and assertion drop. If
  selecting by package, it is `-p braid-cli`, never `-p braid`.
- `just clippy` (`cargo clippy --manifest-path cli/Cargo.toml --tests`) -- no new
  warnings; confirms the guard rewrite and constructor edits are clean.
- `cargo fmt --check`.
- Sanity grep: `grep -rn "any_open" cli/src/` returns nothing after the change.
