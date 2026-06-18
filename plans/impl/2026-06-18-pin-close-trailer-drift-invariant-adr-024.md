# Plan: pin the close-trailer drift invariant in ADR 024

## Context

A review finding asked for a Rust test pinning the live-replace close-trailer
display label under mapper drift (assert the row reads `disk disk2: locking...`,
not `disk WRONG: ...`). Investigation showed that test **already exists** --
`live_replace_old_close_labels_drifted_mapper_with_disk_name`
(`cli/src/replace.rs`), with siblings
`recover_replace_old_close_labels_drifted_mapper_with_disk_name`
(`cli/src/recover.rs`) and `drifted_member_remove_closes_observed_mapper`
(`cli/src/remove.rs`). All three shipped in commit `fb23e72c`
("fix(cli): type close mapper labels as disk names"), the same commit that fixed
the underlying bug by typing `close_mapper_best_effort`'s `disk_label` parameter
as `DiskName`. They pass today. So there is **no test work to do**.

The one real residue: `fb23e72c` updated code + tests but never updated the
authority doc. [ADR 024 luks-uuid-identity](../../docs/design/decisions/024-luks-uuid-identity.md)
(status: Active) is where decision-024 display invariants and their enforcing
tests are recorded, and it has two gaps around the close trailer:

1. **Stated-invariant gap (the paragraph is internally false, not just
   incomplete).** The "Display code has an explicit join rule" paragraph asserts
   a universal -- *"Every display surface uses the same join ... resolves a live
   pool device's UUID back to `DiskName`."* That universal is already wrong
   today, independent of this finding: the post-commit close/remove progress
   trailer is a drift-surviving display surface that does **not** use the UUID
   join. It carries the already-known operator name down as a typed `DiskName`
   (`close_mapper_best_effort(disk_label: &DiskName)`) -- a type-provenance
   route. Because the false universal sits in the same paragraph, the fix is to
   **rewrite the paragraph around label *provenance*** (a user-facing disk row
   must carry an operator `DiskName` sourced by one of two attested routes --
   the live-UUID join, or an already-validated typed `DiskName`), not to append
   a second sentence that contradicts the first. The provenance framing is not
   invented here: `CredentialVerifyTarget`'s two constructors
   (`existing_pool_member` = UUID-joined, `named_candidate` = attested
   `DiskName`) are exactly that split in type form (ADR 024 already documents
   them); the display paragraph just needs to be generalized to match.

2. **Enforcement-list gap.** The "Tests That Enforce This" list cites the
   parallel drift surfaces (status join, credential-verify join, TUI Bus column)
   but omits all three close-trailer drift tests. Per AGENTS.md ("any change to
   an invariant must update the decisions"), they belong in the list.

The audit confirmed these three tests are the *only* true omissions; the
credential-verify/membership tests are already covered by existing bullets (the
list cites files, not symbols -- see Citation style below).

This is a **docs-only** change to one file. No code changes.

## Change

Edit `docs/design/decisions/024-luks-uuid-identity.md` only. Two edits.

### Edit 1 -- rewrite the invariant as label-provenance (Display code paragraph)

**Rewrite** the "Display code has an explicit join rule" bullet (under
`## Concrete Improvements`) so the invariant is provenance-based and the false
"every display surface uses the same join" universal is gone. Preserve every
concrete example the current paragraph carries (TUI Bus column lsblk transport
bridge; the `passphrase: checking against ...` and `... does not match existing
pool member '...'` lines; "blanking to `--`"); only the framing changes. Draft
prose (refine for flow on implementation; retitle the bullet, e.g. **"Display
code has an explicit label-provenance rule."**):

> Every user-facing disk row is labeled with an operator `DiskName`, never a
> mapper basename. That `DiskName` is sourced by one of two attested routes.
> **(1) The live-UUID->`DiskName` join** -- resolve a live pool device's UUID
> back to its name for presentation (UUIDs stay available to
> verbose/machine-readable paths as evidence). The join covers the read
> surfaces: the TUI Data-tab Bus column (its lsblk transport bridge joins the
> parent disk's LUKS UUID to the member name) and the passphrase
> credential-verification display for `add`, `replace`, and their recovery
> replays. **(2) An already-validated typed `DiskName` carried through the
> operation** -- the post-`btrfs`-commit best-effort close that `remove`,
> `replace`, and `recover` run keeps its close *target* on the observed mapper
> (`braid-WRONG`, per "Cleanup follows observed ownership" above) but labels its
> `disk <name>: locking...`/`locked` progress *row* from the journaled operator
> name carried as `close_mapper_best_effort`'s typed `disk_label`.
> (`CredentialVerifyTarget`'s two constructors -- a UUID-joined
> `existing_pool_member` and an attested `named_candidate` -- are this same split
> in type form.) Either route, member names survive mapper drift: a member open
> as `braid-WRONG` still shows its operator name in the
> `passphrase: checking against ...` line, the `... does not match existing pool
> member '...'` rejection, and the close progress row -- instead of blanking to
> `--` or echoing the drifted mapper basename. The low-level
> `cryptsetup close <mapper>` busy-retry diagnostic is the deliberate exception:
> it still names the real mapper because it is a command echo, not a disk-status
> row.

The closing busy-retry sentence is load-bearing: it keeps the invariant accurate
so a future reviewer does not "fix" that diagnostic to say `disk2` and break the
deliberate design (recorded in `plans/impl/2026-06-16-type-safe-close-mapper-label.md`).
Its regression coverage is cited in Edit 2, not here (test citations live in the
"Tests That Enforce This" list, not the principle prose).

### Edit 2 -- cite the tests (Tests That Enforce This)

Insert one bullet immediately after the credential-verify cluster (the
`CredentialVerifyTarget` / `cli/src/credential_verify.rs` bullet), before the
TUI Data-tab Bus column bullet -- this keeps the drift-display-surface bullets
contiguous (status drift -> credential-verify drift -> close-trailer drift ->
TUI). Draft prose:

> - `cli/src/replace.rs`, `cli/src/recover.rs`, and `cli/src/remove.rs` unit
>   tests pin that the post-commit best-effort close trailer closes the observed
>   drifted mapper (`braid-WRONG`) while its operator-facing
>   `disk <name>: locking...`/`locked` row -- and `remove`'s
>   `pool: removing <name>...` row -- names the member by its journaled operator
>   name (`disk2`), not the mapper basename. `close_mapper_best_effort`'s
>   `disk_label` parameter is typed `DiskName`, so no caller can pass a
>   mapper-derived label. Each drives the exit-0 close path; the deliberate
>   carve-out -- that the busy-retry diagnostic still echoes the raw mapper
>   (`cryptsetup close <mapper> busy, retrying...`) because it is a command
>   echo, not a disk-status row -- is pinned by
>   `cli/src/add.rs#guard_retries_busy_close_before_success`.

Tests cited (for the implementer's navigation):
- `cli/src/replace.rs#live_replace_old_close_labels_drifted_mapper_with_disk_name`
- `cli/src/recover.rs#recover_replace_old_close_labels_drifted_mapper_with_disk_name`
- `cli/src/remove.rs#drifted_member_remove_closes_observed_mapper`
- `cli/src/add.rs#guard_retries_busy_close_before_success` -- the busy-retry
  carve-out (asserts the `cryptsetup close braid-aaa busy, retrying (1/3)...`
  row while the disk-status rows still read `disk aaa: ...`)

## Deliberate non-changes

- **No code changes.** The behavior and tests are already correct and shipped.
- **No new test.** The finding's proposed test already exists and passes.
- **Busy-retry stays mapper-named.** Do not add a test or doc claim that the
  `cryptsetup close <mapper> busy, retrying...` row should say `disk2`; it is a
  command echo by design (`cli/src/mapper_close.rs#close_mapper_with_retry`),
  already pinned by `cli/src/add.rs#guard_retries_busy_close_before_success`. The
  ADR now documents this carve-out *and* cites that coverage (Edit 1 prose +
  Edit 2 bullet), so the exception reads as deliberate, not as a gap.
- **Citation style: prose + file-level, with `path#symbol` only where a
  file-level cite is ambiguous.** Every existing "Tests That Enforce This" bullet
  cites files in prose and narrates behavior; the parallel credential-verify
  bullet names four files without symbols, and the close-trailer trio matches
  that voice (their files are specific in context). The busy-retry carve-out is
  the exception: `cli/src/add.rs` holds many tests, so a file-level cite would be
  ambiguous -- it is cited as `cli/src/add.rs#guard_retries_busy_close_before_success`
  per `docs/dev/doc-citations.md`. A wholesale conversion of the list to
  `path#symbol` is a separate sweep, out of scope here.
  (`scripts/docs/check-see-paths.py` validates only the `## See` section, so
  neither style affects CI.)

## Verification

- `just docs-build` -- mdBook build + `mdbook-linkcheck2`; confirms the edited
  ADR still builds and no cross-links broke. (The relative link added in the
  Context section of this plan is not shipped; only the ADR edits are.)
- Re-read the **rewritten** paragraph + new bullet for prose coherence and
  ASCII-only punctuation (`--`, `...`, straight quotes). Confirm the rewrite
  preserved the concrete examples the old paragraph carried (TUI Bus column,
  `passphrase: checking against ...`, `... does not match existing pool member
  '...'`, "blanking to `--`"), since those phrasings are referenced by the
  enforcement list and must not silently change.
- Confirm the cited tests still pass, so the prose is accurate:
  `cargo test --manifest-path cli/Cargo.toml --lib old_close_labels_drifted`
  (replace + recover),
  `cargo test --manifest-path cli/Cargo.toml --lib drifted_member_remove_closes_observed_mapper`
  (remove), and
  `cargo test --manifest-path cli/Cargo.toml --lib guard_retries_busy_close_before_success`
  (the busy-retry carve-out). All currently green.
- No `scripts/docs/check-see-paths.py` impact (the `## See` section is
  untouched); no `check-output-ascii.py` impact (that lints `cli/`/`modules/`,
  not docs).
