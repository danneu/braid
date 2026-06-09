# Plan: align `add.md` `--enroll` docs with actual enrollment behavior

## Context

`docs/commands/add.md` undersells `braid add --enroll`. The flag-table row
(`docs/commands/add.md#important-flags`) scopes keyfile enrollment to "new
disks," and the `docs/commands/add.md#what-happens-under-the-hood` numbered list
omits keyfile enrollment entirely. But the implementation enrolls `braid.key`
into LUKS slot 1 on **every** adopted disk -- fresh *or* returning
(recoverable) -- and has a third behavior the doc never mentions: a
slot-1-occupied-by-an-unknown-key **refusal**.

The code is correct and already shipped (unified in commit `4e6d3b96`
"fix(cli): unify keyfile enrollment across add, replace, and enroll"); only
`add.md` failed to catch up. Two sibling docs already document this correctly
and serve as templates:

- `docs/commands/replace.md#what-happens-under-the-hood` -- dual fresh /
  existing-LUKS path plus the idempotent "already-enrolled disk is a no-op with
  no new backup" note.
- `docs/commands/enroll.md#what-happens-under-the-hood` and
  `docs/commands/enroll.md#safety-checks` -- the slot-1-occupied-by-unknown-key
  refusal with `cryptsetup luksKillSlot` remediation.

This plan adds one behavioral claim to `add.md` (the refusal) that is **not
currently tested on the add path** -- only on the replace path and at the shared
helper. To avoid documenting a safety guarantee that an add-path regression
could silently break while the doc stays green, the plan also adds one
regression test locking that refusal. No production code and no behavior change.

Outcome: `add.md` states all three enrollment outcomes accurately, reaches
parity with `replace.md` and `enroll.md`, and the newly-documented refusal is
locked by an add-path test.

## Verified behavior (the four outcomes)

With `--enroll DIR`, each adopted disk hits exactly one path:

1. **Fresh disk** -- always enrolls slot 1 (freshly formatted, slot 1 always
   empty). `cli/src/add.rs#AddWorkPlan::render_steps` (Fresh arm: enroll step
   between LUKS format and the unconditional header backup).
2. **Returning disk, slot 1 empty** -- `NeedsEnroll`: enrolls slot 1, then backs
   up the header. `cli/src/add.rs#push_returned_disk_enrollment_steps`, called
   from `cli/src/add.rs#AddWorkPlan::render_steps` for both the `OpenRecoverable`
   and `ClosedPresentLuks` arms.
3. **Returning disk, keyfile already authenticates slot 1** -- `AlreadyEnrolled`:
   idempotent skip, no `luksAddKey`, no new header backup.
   `cli/src/add.rs#resolve_existing_luks_enroll` returns `None`.
4. **Returning disk, slot 1 occupied by a different/unknown key** -- hard
   **refusal** with `cryptsetup luksKillSlot` remediation.
   `cli/src/enroll_key_file.rs#check_slot_one_available` returns `Err` (via
   `cli/src/enroll_key_file.rs#plan_single_disk_enrollment`), surfaced as
   `AddError::Validation` through `cli/src/add.rs#resolve_existing_luks_enroll`.
   The message is `slot 1 on <name> (<by-id>) is occupied by an unknown key.
   Remove it first with \`cryptsetup luksKillSlot <by-id> 1\` then re-run
   enrollment.`

Ordering invariant (fresh and returning): `luksAddKey` runs **before** the
header backup so the backup captures slot 1 (doc-comment on
`cli/src/add.rs#push_returned_disk_enrollment_steps`). For returning disks the
header backup only runs when enrollment actually adds a key -- the
`AlreadyEnrolled` skip produces no new backup.

**Existing test coverage:** outcomes 2 and 3 -- `tests/cli/add-enroll-recoverable.py`;
fresh happy-path enroll (outcome 1) -- `tests/cli/braid-add-enroll.py`. Outcome 4
on the add path is **uncovered** -- it is tested only on the replace path
(`tests/cli/replace-enroll-existing-luks-slot-conflict.py`) and at the shared
helper (`cli/src/enroll_key_file.rs#check_slot_one_available` unit tests). See
the test section below.

## Changes -- docs (`docs/commands/add.md`)

### 1. Flag-table row (`docs/commands/add.md#important-flags`)

Replace the "on new disks" scope on the `--enroll <dir>` row. Recommended
wording:

```
| `--enroll <dir>` | Enroll `braid.key` from this directory into LUKS slot 1 on each adopted disk -- fresh or returning -- whose slot 1 is empty; idempotent skip if the keyfile already authenticates slot 1 |
```

(The table already carries long rows, e.g. `--luks-format-arg`, so a fuller row
fits the existing density. Keep the unknown-key refusal out of the row -- it
lives in Safety checks below.)

### 2. `docs/commands/add.md#what-happens-under-the-hood`

Two edits:

**(a) Amend the fresh-disk step** (the "For fresh disks:" numbered step) so the
enroll appears in its true position between format and header backup:

> For fresh disks: pre-generates a LUKS UUID, LUKS-formats the disk with the
> pool passphrase and `braid-<name>` label, **enrolls the `--enroll` keyfile
> into slot 1 if provided,** creates a LUKS header backup, and opens the LUKS
> mapper

**(b) Add a dedicated cross-cutting paragraph** after the numbered list (modeled
on `docs/commands/replace.md#what-happens-under-the-hood`), since enrollment
spans both the fresh path and the returning-disk path that the generic numbered
flow does not separately call out:

> **Keyfile enrollment (`--enroll DIR`):** braid enrolls `braid.key` into LUKS
> slot 1 on every adopted disk -- fresh or returning. On a fresh disk slot 1 is
> always empty, so the keyfile is always added. On a returning braid disk braid
> first probes the keyfile: if it already authenticates slot 1 the enrollment is
> an idempotent skip with no slot change and no new header backup; if slot 1 is
> empty the keyfile is added. (If slot 1 holds a different, unknown key braid
> refuses -- see Safety checks.) The keyfile is added before the header backup so
> the backup captures slot 1.

Do not re-link the header-backup advisory here -- the fresh-disk step already
links `[Pending LUKS header backups](status.md#pending-luks-header-backups)`.

### 3. `docs/commands/add.md#safety-checks--refusal-cases`

Add a refusal bullet immediately after the existing keyfile-warn bullet ("Warns
if existing pool drives have a keyfile but `--enroll` was not passed"), mirroring
`docs/commands/enroll.md#what-happens-under-the-hood` and
`docs/commands/enroll.md#safety-checks`:

> - With `--enroll`, refuses if an adopted disk's LUKS slot 1 is occupied by an
>   unknown key the keyfile does not authenticate -- remove it first with
>   `cryptsetup luksKillSlot`, then retry.

## Changes -- test (lock the documented refusal)

The new Safety-checks bullet asserts a refusal behavior. Across the suite that
refusal is exercised only on the replace path
(`tests/cli/replace-enroll-existing-luks-slot-conflict.py`) and at the shared
helper; the add-path wiring
(`cli/src/add.rs#resolve_existing_luks_enroll` -> `AddError::Validation` ->
nonzero exit, no journal, no pool mutation) has no test. Documenting the
guarantee without locking it lets a future add-path regression break it silently.

**Add the missing case as a new subtest in `tests/cli/add-enroll-recoverable.py`.**
That file is the returning-disk enroll-outcomes test -- it already builds the
returning-disk scenario and owns `make_disk3_missing_then_remove()` and
`add_cmd()`. Appending the refusal completes its outcome matrix (NeedsEnroll /
AlreadyEnrolled / **SlotConflict**) with no new VM boot and no `flake.nix` or
`.nix`-companion change. (A standalone `add-enroll-slot-conflict.py` mirroring the
replace-side file split is a reasonable alternative, but it costs an extra VM
boot and a `flake.nix` `checks` registration for no added coverage.)

New phase, after the existing AlreadyEnrolled phase, with assertions modeled on
`tests/cli/replace-enroll-existing-luks-slot-conflict.py`. Note disk3 returns
from the AlreadyEnrolled phase with slot 1 **still authenticating
`/tmp/braid.key`** -- the header survives `make_disk3_missing_then_remove()` --
so the inherited slot 1 must be cleared before a foreign key can occupy it
(`cryptsetup luksAddKey --key-slot 1` refuses a full slot):

1. `make_disk3_missing_then_remove()` again (disk3 returning; slot 1 still holds
   `/tmp/braid.key` from the prior phase).
2. Clear the inherited slot 1:
   `cryptsetup luksKillSlot --batch-mode /dev/disk/by-id/virtio-disk3 1`; assert
   slot 1 is now absent (otherwise the next `--key-slot 1` add hits a full slot).
3. Poison slot 1 with a foreign key: generate `/tmp/foreign.key` (fresh random,
   distinct from `/tmp/braid.key`) and `cryptsetup luksAddKey --key-slot 1`
   authenticated with the pool passphrase. Sanity-assert slot 1 now present, and
   capture `dump_before = cryptsetup luksDump --dump-json-metadata` of disk3 --
   the poisoned header, for the preservation check below.
4. Run `add_cmd('disk3', '--enroll /tmp')` capturing exit code + combined output
   (`/tmp/braid.key` does not authenticate the foreign slot 1).
5. Assert: exit != 0; output contains `slot 1 on disk3`, `occupied by an unknown
   key`, and `luksKillSlot`; `machine.fail("test -f /var/lib/braid/pending-op.json")`
   (no journal); `btrfs fi show /mnt/storage` does **not** list `braid-disk3`
   (no pool mutation); and -- the safety property the doc actually promises --
   `cryptsetup luksDump --dump-json-metadata` of disk3 **equals `dump_before`**,
   proving the unknown slot-1 key was preserved untouched (not wiped or replaced):
   the contract is a refusal that leaves the operator to clear the slot manually.
   Reuse the file's existing `dump_after == dump_before` idiom (its AlreadyEnrolled
   phase).

This is a regression/characterization test: the behavior already exists via the
shared helper, so it **passes on current code** (it is not the AGENTS.md
write-failing-test-first TDD case -- there is no new behavior to implement). Its
value is catching a future add-path regression -- e.g. a refactor that swallows
the helper's `Err` -- that the replace-side and helper-level tests would miss.

## Out of scope / non-changes

- **`README.md`** -- verified it carries no `add --enroll` scoping claim (only a
  one-line table entry for the `enroll` command). No change needed; the original
  finding's "README sync risk" is hypothetical.
- **`replace.md` / `enroll.md`** -- already correct; they are the templates, not
  instances of the bug. Leave them.
- **No production code, behavior, invariant, or ADR change** -- the docs catch up
  to already-shipped behavior, and the only non-docs edit is one regression test
  that passes against current code. No `principles.md` / `decisions/` update is
  in scope.

## Conventions to honor

- **ASCII only** -- use `--`, `...`, ASCII quotes (global rule; the recommended
  wording above already complies).
- **Cross-links** -- any markdown link must resolve; `mdbook-linkcheck2` fails CI
  on a broken link. The wording above reuses only the existing `status.md`
  anchor. (Code `path#symbol` citations in this plan are deliberately bare code
  spans, never linkified -- `cli/` lives outside the mdBook root; see
  `docs/dev/doc-citations.md#doc-and-adr-file-references`.)
- **Test preamble** -- `tests/cli/add-enroll-recoverable.py` carries a single
  file-level Intent / Why it exists / Scenario block (no per-subtest preambles),
  so extend that block to cover all three returning-disk outcomes -- NeedsEnroll,
  AlreadyEnrolled, and the new SlotConflict refusal -- rather than adding a
  per-subtest preamble. Form per
  `docs/dev/testing.md#preamble-literal--line-comment-form`.

> Note on this plan's own citations: `docs/dev/doc-citations.md` exempts
> `plans/wip/` from the no-line-numbers rule, so line numbers here would not
> violate the convention -- but `path#symbol` / `path#heading-slug` anchors are
> drift-proof and greppable for the implementer, so this plan uses them anyway.

## Verification

1. `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`; confirms
   no broken links introduced.
2. Run the extended VM test and confirm it **passes on current code**:
   `nix build .#checks.aarch64-darwin.add-enroll-recoverable` (or the matching
   `just` recipe -- check `just --list`). A pass proves the documented refusal
   holds end-to-end on the add path today.
3. Re-read the edited `add.md` sections against the four verified outcomes above
   and against `tests/cli/add-enroll-recoverable.py` -- the doc must now state:
   always-enroll on fresh, enroll-if-empty on returning, idempotent skip when the
   keyfile already authenticates, and refusal on an unknown slot-1 key.
4. Eyeball parity: the new under-the-hood paragraph reads consistently with
   `docs/commands/replace.md#what-happens-under-the-hood`, and the new refusal
   bullet with `docs/commands/enroll.md#safety-checks`.
5. Confirm **this plan's diff** touches only `docs/commands/add.md` and
   `tests/cli/add-enroll-recoverable.py` -- no README, no `flake.nix`, no
   production code. The worktree carries a pre-existing, unrelated
   `M cli/src/replace.rs`; leave it as-is -- do not fold it into this change and
   do not revert it.
