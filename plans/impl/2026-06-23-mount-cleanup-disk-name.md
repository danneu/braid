# Plan: carry a typed `DiskName` through the mount cleanup close path (carry-both)

## Status note -- the add half already landed

Since this plan was first drafted, HEAD advanced. Commit `324fee31`
("fix(cli): label add cleanup rows from disk names", local, unpushed) **already
converted the `add` rollback guard**, and it did so with a **carry-both** design,
not the `OpenedMapper::for_disk` derive this plan originally specified:

- `cli/src/add.rs#TrackedMapper` is `{ name: DiskName, mapper: MapperName }` --
  the operator name and the *observed* mapper carried side by side.
- `LuksCleanupGuard::track(name, mapper)` records both; `LuksCleanupGuard`'s
  `Drop` renders `disk <name>: ... (cleanup)` rows from `tracked.name` and closes
  `tracked.mapper` (the strip is gone), preserving `.iter().rev()`.
- `cli/src/add.rs#guard_cleanup_row_uses_disk_name_under_mapper_drift` is the
  drift-injection regression: it tracks `(disk("disk2"), braid-WRONG)`, asserts
  the rows say `disk disk2`, that the output never contains `WRONG`, and that the
  closed dm slot is still `braid-WRONG`. It passes against the shipped body.
- ADR 024's label-provenance section was extended to describe `add`'s rollback
  cleanup, and the test inventory gained the add guard test.

The working tree is clean; this is committed history, not an in-progress edit.
**This plan does not revert any of it -- it rebases onto it.** The shipped
carry-both shape is correct and is now the precedent the remaining work mirrors.
What is left is the `mount` half plus the ADR/inventory extension to cover it.

## Context

ADR 024 (`docs/design/decisions/024-luks-uuid-identity.md`, "Display code has an
explicit label-provenance rule") makes a hard invariant: *every* operator-facing
disk-status row is labeled by an attested `DiskName`, **never** by a mapper
basename. Commit `fb23e72c` enforced this for the post-`btrfs`-commit best-effort
close that `remove`/`replace`/`recover` run, by typing
`close_mapper_best_effort`'s `disk_label: &DiskName`. Commit `324fee31` then
brought `add`'s pre-commit rollback guard under the same rule via carry-both
`TrackedMapper`.

One close path remains on the old idiom -- it re-derives the label by
string-stripping `braid-` off the mapper basename and emits `disk {label}: ...`
rows from it:

- `cli/src/mount.rs` -- `close_opened_mappers` (the fail-closed cleanup after an
  unlock/mount error). Strip at `mount.rs#close_opened_mappers` (the
  `strip_prefix("braid-")` line). The field it iterates,
  `UnlockAndMountFailure.opened_mappers`, is `Vec<MapperName>`; the open site
  `open_disks_with_credential` pushes `mapper_name(name)`.

This is not a **live** mislabel today: `open_disks_with_credential` only ever
pushes mappers it minted this same run via `mapper_name(&name)`
(`cli/src/config.rs#mapper_name` => `braid-<name>`), so the basename always
round-trips to the right name. But the safety is an emergent property of how the
caller happens to be written, not a structural guarantee -- exactly the footgun
braid's newtype-sealing direction sets out to remove, and exactly the residue the
prior promoted plan (`plans/impl/2026-06-16-type-safe-close-mapper-label.md`)
scoped out on the now-falsified premise that "no journaled `DiskName` is in
scope." The typed operator `DiskName` is in hand at the open site (`name:
&DiskName`, from the `to_unlock: &[(DiskName, ByIdPath)]` loop) with no journal
read required. Per AGENTS.md ("Code that contradicts a principle is wrong -- fix
the code"), the ideal is to carry the name through, mirroring what `add` and the
post-commit close already do.

**Outcome.** A typed `DiskName` is carried alongside the *observed* `MapperName`
through `mount`'s cleanup path; the row renders from the name and the close
targets the observed mapper. The last `strip_prefix("braid-")` disk-status-row
site is deleted, so braid's invariant collapses to "no disk row is ever derived
from a mapper basename, full stop," leaving only the two sanctioned carve-outs:
the `cryptsetup close <mapper>` busy-retry **command echo** (a command line, not a
disk row) and `config::name_from_mapper`/`braid_disk_name` (the documented
display-only parser for `discover`/`lock`-candidacy/orphan labels).

## Design decision: carry-both, never derive

The carrier carries the **observed** `MapperName` next to the typed `DiskName`.
It does **not** derive the mapper from the name. This is a deliberate reversal of
this plan's original `OpenedMapper::for_disk` (which computed `mapper_name(&name)`
inside the constructor):

- **The ADR-024 bug is mapper-basename-*derived* labels**, and that bug becomes
  unconstructable the instant the row renders from a typed `DiskName`. Deriving
  the *mapper* from the name guards a different thing -- a name/mapper *mismatch*
  -- which is not the ADR bug.
- **A derive constructor is the exact anti-pattern ADR 024 forbids.**
  `mapper_name(&name)` is the "reconstructed `mapper_name(&member.name)`" that
  ADR 024 names as wrong in "Cleanup follows observed ownership" and in the
  Runtime-Handles rules ("close the observed mapper name, not a reconstructed
  `mapper_name(&member.name)`"; "must not reconstruct `mapper_name(&member.name)`
  during execute"). It is safe in `add`/`mount` only because those paths mint the
  mapper -- but a generically-named derive constructor sitting in the shared close
  domain invites a future drift-*observing* cleanup path (the `lock`/`remove`/
  `replace`/`recover` family, which deliberately close drifted-but-member-owned
  mappers) to reach for it and silently close the *expected* mapper instead of the
  observed one. That is a functional bug, strictly worse than the cosmetic label
  leak this change fixes.
- **The plan's own cited precedents are carry-both, not derive.**
  `cli/src/credential_verify.rs#CredentialVerifyTarget` seals its fields but
  carries the runtime handle *separately* as the *observed* path
  (`existing_pool_member` stores `device.underlying`; `named_candidate` takes the
  `ByIdPath` device as its own argument) -- its doc says "identity is `device`."
  Neither constructor derives the handle from the name.
  `close_mapper_best_effort`'s `(mapper: &MapperName, disk_label: &DiskName)`
  signature is the same carry-both split in parameter form. The shipped
  `add.rs#TrackedMapper` is the same split in struct form.

Carry-both is also unconditionally **behavior-preserving**: it carries exactly the
mapper that was opened, removing the load-bearing-but-untested assumption that
`mapper_name(name)` will forever equal what the open site pushed.

The only thing surrendered versus the derive design is *type-level prevention of a
valid-name/valid-mapper mismatch*. That is not the ADR bug, and
`CredentialVerifyTarget` itself does not prevent it. If a future non-minting
caller ever needs mismatch-proofing, the right shape is a sealed carrier with two
explicit constructors -- `minted(name)` and `observed(mapper, name)` -- so the
derive convenience never becomes the *only* way to build one. We do not need that
today.

## Approach

### 1. The carrier -- carry-both, shared with `add`

Use one carry-both carrier for both `add` and `mount`. `add` already defines
`TrackedMapper { name: DiskName, mapper: MapperName }` (module-private in
`add.rs`). **Recommended:** hoist it to `cli/src/mapper_close.rs` as
`pub(crate)`, because that module already imports `DiskName`/`MapperName`, owns the
close domain, and sits next to `close_mapper_best_effort` whose `(mapper,
disk_label)` signature it mirrors. This is a *move* (plus `pub(crate)` and a `///`),
not a behavior change; `add`'s `Drop` keeps reading `tracked.name`/`tracked.mapper`
and its passing drift test is untouched.

```rust
/// A LUKS mapper opened by this command, paired with the operator `DiskName`
/// it was opened for. `name` is the operator identity used to render every
/// `disk <name>: ...` cleanup row; `mapper` is the *observed* runtime dm
/// handle actually closed. Carrying the observed mapper (never deriving it
/// from `name`) keeps cleanup on "close what was observed" while the row reads
/// from `name` -- the ADR 024 label-provenance rule without the reconstructed-
/// `mapper_name(&name)` anti-pattern ADR 024 forbids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedMapper {
    pub(crate) name: DiskName,
    pub(crate) mapper: MapperName,
}
```

A bare `(DiskName, MapperName)` tuple is rejected: ADR 024 exists precisely to keep
these two axes from being confused, and a positional tuple invites the `.0`/`.1`
transposition a named pair prevents. The derives are needed for the `assert_eq!`
test assertions and `opened_mappers.clone()`; both inner types already derive them.

**Acceptable alternative (lower churn):** leave `add`'s `TrackedMapper`
module-private and give `mount` its own carry-both named pair. The hard constraints
are identical either way -- carry the observed mapper, never derive; render rows
from `name` -- so the home is a style choice. The shared hoist is recommended only
because it removes the duplication and gives the carrier its natural home. Do
**not** add a derive (`for_disk`-style) constructor in either location.

### 2. Convert the mount cleanup path -- `cli/src/mount.rs`

- `pub struct UnlockAndMountFailure.opened_mappers: Vec<MapperName>` ->
  `Vec<TrackedMapper>` (intra-crate `pub`; all consumers are in `cli/src`).
- `open_disks_with_credential`'s `opened: &mut Vec<MapperName>` ->
  `&mut Vec<TrackedMapper>`; the `OpenOutcome::Opened` push becomes
  `opened.push(TrackedMapper { name: name.clone(), mapper: mapper_name(name) })`
  -- `name: &DiskName` is in scope from the `to_unlock` loop, and `mapper_name(name)`
  is exactly the observed mapper the site pushes today (carry it, do not re-derive
  it later).
- `close_opened_mappers`'s `opened: &[MapperName]` -> `&[TrackedMapper]`: the
  `forget_devs` map uses `t.mapper.dev_path()`; delete the
  `strip_prefix("braid-")`; render rows from `t.name`, close `t.mapper`.
  **Preserve** forward iteration order, the forget-before-close ordering, and the
  `if first_error.is_none()` first-error-precedence logic (all unchanged by this
  edit -- the loop element type and label source change, the control flow does
  not). Extend the doc comment to mirror `close_mapper_best_effort`.

The four `UnlockAndMountFailure.opened_mappers` consumers
(`unlock.rs#cmd_unlock`; `recover.rs` x3) just forward `&failure.opened_mappers`
into `close_opened_mappers` -- both ends change element type together, so they
recompile with **no edit**.

### Reuse / do NOT change

- Reuse `close_mapper_with_retry` (`mapper_close.rs`) as-is in the emitter -- the
  busy-retry mechanic is already shared. Its busy diagnostic
  (`cryptsetup close <mapper> busy, retrying...`) legitimately echoes the raw
  mapper and stays (ADR 024 carve-out: command echo, not a disk row).
- **Do not unify the three close-row emitters** (`close_mapper_best_effort`,
  `LuksCleanupGuard::drop`, `close_opened_mappers`). They diverge on return shape
  (bool vs unit vs `Result`), row suffix (`" (cleanup)"`), fail tag (Warn vs
  Fail), pre-step (btrfs `device scan --forget`), and iteration direction.
  Merging would need ~5 behavior knobs and hide nothing. Unify only the label
  provenance (carry the typed name), not the control flow.
- Leave `config::name_from_mapper`/`braid_disk_name` -- the sanctioned
  display-only parser.
- Do **not** revert or re-open commit `324fee31`'s behavior; the shared-carrier
  hoist (if taken) is a move, not a redesign.

## Tests

The production path round-trips the basename, so a black-box test over real
`mount` code **cannot** distinguish typed-from-name vs stripped-from-base. A
behavioral drift-injection unit test is the only structure-insensitive way to prove
the emitter renders from the typed field.

1. **`mount.rs` drift-label regression** (mirrors `add`'s shipped
   `guard_cleanup_row_uses_disk_name_under_mapper_drift`): call
   `close_opened_mappers` with a tracked pair whose name and mapper diverge --
   `TrackedMapper { name: disk("disk1"), mapper: braid-WRONG }`. Assert the rows
   read `disk disk1: locking...` / `locked`, that the output never contains
   `WRONG`, and that the close (and the forget) target the observed `braid-WRONG`.

   Two construction caveats the test MUST honor:
   - **Drive an exit-0 close.** A busy-then-success setup would emit the
     `cryptsetup close braid-WRONG busy, retrying...` command-echo carve-out and
     trip the `!contains("WRONG")` assertion.
   - **Expose `/dev/mapper/braid-WRONG` in the test fs.** `close_opened_mappers`
     builds `forget_devs` from `t.mapper.dev_path()` filtered by `fs.exists(path)`;
     the existing `direct_two_disk_fs_with_mappers` helper only mints
     `braid-disk*`, so the drift fixture must add `/dev/mapper/braid-WRONG` (and
     stub a matching `BtrfsDeviceScanForget` for it) or the "forget targets
     braid-WRONG" assertion exercises nothing.

   This is the one new test the mount conversion *requires*: it pins the behavior
   the change introduces (row labeled from the carried name).

2. **Mechanical test updates** (compile-driven, behavior unchanged): the `mount.rs`
   tests that `assert_eq!` against `vec![MapperName...]` or build a literal
   `opened` vec switch to `vec![TrackedMapper { name: disk("..."), mapper:
   MapperName::from_basename("braid-...".into()) }]` (representative sites: the
   `failure.opened_mappers` assertions and the direct `close_opened_mappers(&opened,
   ...)` callers, including `cleanup_busy_close_attempts_later_mappers...`).
   `recover.rs` tests assert on close-request **counts**, not `opened_mappers`
   contents, so they are unaffected. No new VM test: the closed dm slot and
   production-minted names are byte-identical on real hardware.

### Optional (independent hardening, not required by this refactor)

- **First-error-precedence regression** (`mount.rs`): the existing multi-mapper
  test `cleanup_busy_close_attempts_later_mappers_and_reports_guidance` fails only
  *one* close (disk1 busy, disk2 ok), so first==last and it does not pin "report
  the *first* failure when several fail." A test with two closes failing on
  *distinct* errors would close that gap. **But this refactor does not touch the
  `if first_error.is_none()` logic** -- it changes the loop element type and the
  label source only -- so this is orthogonal hardening, not coverage the mount
  conversion demands. Per the test-quality bar (only own tests for behavior the
  change alters), keep it out of this commit's required set, or land it separately
  so the commit's test delta maps to the behavior the commit actually changes.

## Authority-doc updates (mandatory -- AGENTS.md)

- **ADR 024** (`docs/design/decisions/024-luks-uuid-identity.md`): commit
  `324fee31` already extended the label-provenance section and the test inventory
  to cover `add`'s rollback guard. **Extend the same paragraph to cover `mount`'s
  `close_opened_mappers`**: it carries a typed `DiskName` next to each opened
  mapper and labels its `disk <name>: locking...`/`locked` cleanup rows from that
  value while closing the observed mapper -- the same carry-both route as `add` and
  the post-commit best-effort close, not a reconstructed `mapper_name(&name)`.
  State that the only remaining basename-derived strings are the busy-retry command
  echo and `name_from_mapper`/`braid_disk_name`. Add the new `mount` drift
  regression to the test inventory next to the add guard test. Cite by
  `path#symbol` / heading slug, never line numbers.
- **Prior plan** (`plans/impl/2026-06-16-type-safe-close-mapper-label.md`): amend
  its "Explicitly out of scope" bullet for the `add.rs`/`mount.rs` rollback paths
  to a superseded note -- keep the history, record that a follow-up recognized the
  typed `DiskName` is in scope at both sites so the footgun argument applies
  identically (`add` landed in `324fee31`, `mount` in this change), and point
  forward. (Without this the authority docs contradict each other.) If `324fee31`
  did not already touch this bullet, this change does.

## Commit shape

One atomic commit (the build fails between the type change and the call-site/test
edits -- that coupling is the safety property, mirroring `fb23e72c`/`324fee31`).
Conventional Commits, lowercase: e.g. `refactor(cli): carry typed disk name through
mount cleanup closes`. If the shared-carrier hoist is taken, it rides in the same
commit (it is the type the new mount field uses).

## Verification

```
cargo build                 # fails until all call sites + tests are updated (intended)
just test-rust              # unit tests, incl. the new mount drift-label regression
just check-output-ascii     # cleanup/close row strings stay ASCII
just docs-build             # ADR 024 + prior-plan link/citation check (mdbook-linkcheck2)
```

Expected: the new mount drift assertion fails before the emitter edit (proving it
catches a basename leak) and passes after; the mechanical test edits compile and
pass; no behavior change in any VM test.

## Critical files

- `cli/src/mapper_close.rs` -- home for the shared carry-both `TrackedMapper` (if hoisted); provenance precedent (`close_mapper_best_effort`) to mirror.
- `cli/src/add.rs` -- `TrackedMapper`/`LuksCleanupGuard` already converted (`324fee31`); only touched if the carrier is hoisted (a move, no behavior change).
- `cli/src/mount.rs` -- `UnlockAndMountFailure` field, `open_disks_with_credential`, `close_opened_mappers` (delete the strip) + literal-vec test updates + 1 new drift-label test.
- `cli/src/unlock.rs`, `cli/src/recover.rs` -- type-transparent forward callers (recompile only).
- `docs/design/decisions/024-luks-uuid-identity.md` -- extend label-provenance + test inventory to cover `mount`.
- `plans/impl/2026-06-16-type-safe-close-mapper-label.md` -- supersede the out-of-scope carve-out (now overtaken for both `add` and `mount`).
