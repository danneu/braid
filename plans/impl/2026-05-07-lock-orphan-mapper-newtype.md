# Make orphan-mapper invariant type-level

## Context

The orphan close loop in `cli/src/lock.rs` calls
`name_from_mapper(entry).unwrap_or(entry)` on each `entry: &String` in
`LockPlan.orphan_mappers: Vec<String>`. The `unwrap_or` arm is
unreachable because `scan_orphan_mappers` (`lock.rs:37-55`) only pushes
entries that already pass the `braid-` prefix strip.

This site has flip-flopped twice in the last hour:

- `ccee87e refactor(lock): document orphan-mapper invariant with expect`
  -- switched the fallback to `expect("scan_orphan_mappers returns only
  braid-* mapper names")`. Rationale: the invariant is unreachable, so
  panic on violation rather than leak a raw mapper name into a status
  row.
- `f035492 refactor(lock): collapse close loops via shared helper` --
  reverted to `unwrap_or(entry)` with a longer comment. Rationale (in
  `plans/impl/2026-05-07-lock-collapse-close-loops.md:166-181`):
  `orphan_mappers` is `Vec<String>` with no type-level invariant, so
  panicking on a soft invariant is too aggressive when graceful
  degradation of a status-row label is the worst outcome.

The reviewer's finding then re-flagged the `unwrap_or` as dead defensive
code despite the comment that was meant to head off exactly that flag.

The author's objection to `expect` -- `Vec<String>` carries no
type-level guarantee -- is the actual root cause. This plan moves the
invariant from a runtime check (whether `expect` or `unwrap_or`) into
the type system. Once `scan_orphan_mappers` is the only path that can
build an `OrphanMapper`, the fallback is unrepresentable; both the
`unwrap_or`/`expect` debate and the explanatory comment go away.

## Approach

Introduce a small newtype `OrphanMapper` module-private to
`cli/src/lock.rs` that bundles the mapper name with its pre-stripped
disk name. There is no constructor function; the only construction
site is the struct literal inside `scan_orphan_mappers`.

```rust
/// Internal scanned-orphan representation. Constructed by
/// `scan_orphan_mappers`: a `/dev/mapper/braid-*` entry observed at
/// plan time that is not part of pool membership. Fields are private
/// and there is no constructor, so the only production code path that
/// can create one is the struct literal inside `scan_orphan_mappers`
/// itself -- the prior `unwrap_or`/`expect` fallback for
/// `name_from_mapper` is unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OrphanMapper {
    mapper: String,    // "braid-toshiba"
    disk_name: String, // "toshiba"
}

impl OrphanMapper {
    fn mapper(&self) -> &str { &self.mapper }
    fn disk_name(&self) -> &str { &self.disk_name }
}
```

Visibility (corrects the previous draft, which leaked the type):

- `OrphanMapper` is non-`pub`. The reviewer's point: a `pub` newtype
  with a module-private constructor only enforces the `braid-` prefix,
  not "discovered by `scan_orphan_mappers`." Module-private with no
  constructor function is the form that matches the documented
  invariant.
- Fields are private. No `pub fn from_mapper_entry`. The struct literal
  is the constructor, and only `scan_orphan_mappers` writes one.
- `LockPlan.orphan_mappers` drops from `pub` to non-pub. No cross-file
  caller reads it today (verified by `grep -rn 'orphan_mappers' cli/`
  returning hits only inside `cli/src/lock.rs`), so the tightening
  costs nothing. (Leaving the field `pub` while the type is
  module-private would compile -- it would only emit a
  `private_interfaces` warning if some other module actually named the
  private type -- but the visibility on the field still carries
  intent: no caller is meant to reach into the planned orphan set.)
- `compile_lock_steps` drops from `pub fn` to `fn`. Only `plan_lock`
  and the in-module test call it; no cross-file callers.
- `scan_orphan_mappers` and `close_set_paths` are already non-pub --
  unchanged.
- `LockPlan` itself stays `pub` (it's the return type of `pub fn
  plan_lock`, even though `plan_lock` has no cross-file callers
  either; tightening that is out of scope for this fix).

Thread the new type through the sites that currently take `&[String]`
/ `Vec<String>`:

1. `scan_orphan_mappers` (`lock.rs:37-55`) returns
   `Vec<OrphanMapper>`. The body becomes:
   ```rust
   for entry in entries {
       let Some(disk_name) = name_from_mapper(&entry) else { continue };
       if membership.disks.contains_key(disk_name) { continue; }
       if fs.exists(&format!("/dev/mapper/{entry}")) {
           let disk_name = disk_name.to_owned();
           orphans.push(OrphanMapper { mapper: entry, disk_name });
       }
   }
   ```
   This is the only production construction site -- nowhere else in
   the module writes `OrphanMapper { ... }`.
2. `close_set_paths` (`lock.rs:87-93`) takes
   `orphan_mappers: &[OrphanMapper]`; the chain maps to `m.mapper()`.
3. `compile_lock_steps` (`lock.rs:160-209`) takes
   `orphan_mappers: &[OrphanMapper]`; orphan loop formats with
   `mapper.mapper()` and clones with `mapper.mapper().to_owned()`.
4. `LockPlan.orphan_mappers` (`lock.rs:223`) becomes
   `orphan_mappers: Vec<OrphanMapper>` (non-pub).
5. The orphan close loop in `LockPlan::execute` (`lock.rs:388-410`)
   reads `entry.mapper()` for `fs.exists` / `close_one_mapper` and
   `entry.disk_name()` for the disk label. The
   `name_from_mapper(entry).unwrap_or(entry)` line and the
   surrounding comment block (`lock.rs:392-398`) are deleted -- the
   type carries the documentation.
6. `plan_lock`'s orphan-warn note loop (`lock.rs:468-479`) passes
   `om.mapper()` to `orphan_mapper_warn_body`.

`orphan_mapper_warn_body` and `orphan_scan_warn_body` keep their
`&str`/`&io::Error` signatures -- the warn body still wants the full
`braid-*` string, not the disk name.

The only test that constructs orphan mappers directly is
`dry_run_lock_forget_step_includes_orphans` (`lock.rs:2235-2248`).
It updates to a struct literal -- `mod tests` is a child of `mod lock`
and so can see private fields:

```rust
let orphan_mappers = vec![OrphanMapper {
    mapper: "braid-orphan".into(),
    disk_name: "orphan".into(),
}];
```

This is the only place in the codebase that bypasses
`scan_orphan_mappers`, and only because `#[cfg(test)]` is by design
allowed to fabricate fixtures. Production code has no path to write
that literal.

`lock_forget_includes_orphan_mappers` (`lock.rs:2324`) drives the
orphan path via `cmd_lock_impl` -> `plan_lock` -> `scan_orphan_mappers`
and stays unchanged: `MockFs` already lists `/dev/mapper/braid-ccc`,
which the production scanner converts to `OrphanMapper` automatically.

## Critical files

- `cli/src/lock.rs` -- add `OrphanMapper`; rewrite
  `scan_orphan_mappers`, `close_set_paths`, `compile_lock_steps`,
  `LockPlan` field, the orphan close loop, and `plan_lock`'s warn
  loop; delete the `unwrap_or(entry)` line and its rationale comment;
  update `dry_run_lock_forget_step_includes_orphans`.

No other source files or tests need to change -- a cross-file grep for
`orphan_mappers`, `LockPlan`, `compile_lock_steps`,
`scan_orphan_mappers`, and `close_set_paths` returned no matches outside
`cli/src/lock.rs`.

## Reused functions and types

- `name_from_mapper` (`cli/src/config.rs:76-78`) -- still the gate
  inside `scan_orphan_mappers`. The new code reuses the same one-liner
  in the same place; the only change is that the stripped disk name is
  now captured into the `OrphanMapper` literal instead of being
  recomputed at the close-loop use site.
- `MockFs`, `MockRunner`, `RecordingRunner`, `NoopSleeper` (test scaffold
  in `cli/src/lock.rs:606+`) -- unchanged.
- `Step`, `CmdRequest`, `Filesystem`, `PoolMembership`, `MountPoint`,
  `PreviewNote` -- consumed unchanged.

## Verification

End-to-end checks, in order:

1. `cargo build -p braid-cli` (or via `just`) -- the type-shape change
   is the highest-risk part; compile errors surface every consumer that
   needs to update.
2. `just test-rust` -- exercises:
   - `dry_run_lock_forget_step_includes_orphans` (updated test).
   - `lock_forget_includes_orphan_mappers` (must still pass; orphan
     enters via the production scanner path, MockFs setup unchanged).
   - `dry_run_lock_forget_step_omitted_when_no_mappers`,
     `lock_forget_is_pool_scoped`,
     `close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts`,
     and the rest of the lock test module.
   - `name_from_mapper_strips_prefix` /
     `name_from_mapper_returns_none_for_non_braid` in
     `cli/src/config.rs` -- unchanged but worth confirming no
     regression since `scan_orphan_mappers` depends on the
     prefix-strip contract these tests pin.
3. `just test-vm lock-orphan-mapper-cleanup` (and any other
   `lock-*` checks that drive orphan close in a real VM with cryptsetup
   + btrfs) -- contract test that `braid lock` still closes a
   crash-created orphan when the binary runs against a real
   `/dev/mapper`.
4. Grep the tree for residual references to the old shape:
   - `grep -n 'orphan_mappers' cli/src/lock.rs` -- every hit should be
     `Vec<OrphanMapper>` / `&[OrphanMapper]`.
   - `grep -n 'unwrap_or(entry)' cli/src/lock.rs` -- no hits.
   - `grep -n 'scan_orphan_mappers admits only braid-' cli/src/lock.rs`
     -- no hits (the explanatory comment is gone).
   - `grep -n 'OrphanMapper {' cli/src/lock.rs` -- exactly two hits
     (the production literal in `scan_orphan_mappers` and the test
     fixture in `dry_run_lock_forget_step_includes_orphans`). Any
     additional hit means a non-scan production construction site
     slipped in and the type-level invariant is broken.
   - `grep -rn 'OrphanMapper' cli/src/ | grep -v 'cli/src/lock.rs:'`
     -- no hits (the type is not referenced outside the lock module).
   - `grep -n 'pub orphan_mappers' cli/src/lock.rs` -- no hits (the
     field is non-pub).
   - `grep -n 'pub fn compile_lock_steps' cli/src/lock.rs` -- no hits
     (the function is non-pub).
   - `grep -n 'pub struct OrphanMapper\|pub fn from_mapper_entry\|pub(crate) struct OrphanMapper' cli/src/lock.rs`
     -- no hits (no public alias of the type or a re-introduced
     constructor).

Stderr rows printed during `LockPlan::execute` and the `LockError`
returned from `cmd_lock` must remain byte-identical to the current
behavior -- the only code-path change is removing an unreachable
branch. Existing tests that pin those strings (e.g. orphan close
status rows in the lock module) carry that contract.

## Out of scope

The four sibling `name_from_mapper(...).unwrap_or(<raw mapper>)` sites
in `cli/src/add.rs:1362-1364`, `cli/src/replace.rs:352-354`, and
`cli/src/recover.rs:1855-1857` / `2395-2397` use a different invariant
(`MapperName(pub String)` is `braid-`-prefixed by construction because
its only documented producer is `mapper_name`). Hardening those is a
separate change against `MapperName`'s type definition -- not in this
plan.
