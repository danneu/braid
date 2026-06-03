# Plan: document the LUKS UUID-mismatch refusal in `braid unlock`

## Context

`braid unlock` hard-refuses when a present disk's LUKS header reports a
UUID that differs from the UUID recorded in `pool.json` (a swapped,
cloned, or reformatted disk). The refusal happens early, during the
per-member probe loop, before any LUKS mapper opens -- see the
`expected_uuid != uuid` branch in the probe loop in `cli/src/mount.rs`,
which returns `MountError::Failed(...)`. It maps to exit code 1 (not the
degraded code 2): `MountError::DegradedRefused` is the only variant that
exits 2 in the unlock arm of `cli/src/main.rs`.

This refusal is documented in `status.md` (the `LUKS UUID MISMATCH`
state row + `Action:` line), `doctor.md` (the `declared_disks` check
fails on a live-vs-recorded UUID mismatch), and ADR
`017-runtime-disk-membership.md` ("unlock fatally errors"). But the one
page an operator opens after hitting it during `unlock` --
`docs/commands/unlock.md` -- never lists it among its refusal cases. The
page's "What happens under the hood" step 2 mentions the probe *checks*
the UUID, but never states the *refusal consequence*.

The gap matters because the failure sits right next to degraded mode in
the reader's mental model: a disk that is physically *present* gets
refused, so the natural (wrong) guess is to reach for `--allow-degraded`.
It does not help -- the mismatch returns before the degraded gate
(`allow_degraded` check later in the same function), so the flag cannot
reach it.

There is a second, subtler trap. The runtime hint
(`luks::luks_uuid_mismatch_guidance`) tells an operator who swapped the
disk *intentionally* to "run `braid replace`". But `braid replace`
hard-requires a mounted pool (`replace.rs`: returns "pool is not mounted.
Cannot replace." when `!pool.mounted`), and the very mismatch refusal
blocks the mount. Following that hint literally dead-ends. The reachable
path is: detach the foreign disk so the member reads as *missing*, mount
degraded, then replace -- a multi-step recovery the one-line hint cannot
carry.

Intended outcome: a reader who hits the UUID-mismatch error and opens
`unlock.md` finds it in the refusal list, learns it is a hard probe-time
error that `--allow-degraded` does not bypass, and -- for the intentional
case -- is routed to a recovery procedure that actually works rather than
a dead-end command. A regression test locks the load-bearing
"`--allow-degraded` does not bypass it" claim.

## Approach

Three coordinated touches, each at the surface that owns it:

1. **`docs/commands/unlock.md`** -- one terse bullet in the refusal list
   (the page that owns unlock refusals). It states the refusal and routes
   the multi-step intentional-swap case to the recovery guide.
2. **`docs/guides/recovery-scenarios.md`** -- a short new section carrying
   the detach -> degraded -> replace procedure (the guide that owns
   multi-step recovery recipes), plus a new branch in the page's "Recovery
   decision tree" so the index routes a mismatch to that section instead of
   to the (wrong-for-it) degraded advice. It delegates the replace step to
   the existing "Missing disk (drive failure)" recipe rather than
   re-documenting replace.
3. **`cli/src/mount.rs` (test module)** -- one regression test that locks
   the doc's central claim that `--allow-degraded` does not bypass the
   mismatch.

Why a single bullet in `unlock.md` (not a degraded-section note or a
dedicated subsection *there*): the refusal list is the structurally
correct home; `status.md` / `doctor.md` / ADR 017 already own the richer
glance surfaces; and a UUID mismatch is a hard stop, not an operational
"mode" warranting its own `##` on the command page. The multi-step
*procedure* belongs in `recovery-scenarios.md`, where every other recovery
recipe lives -- keeping `unlock.md` terse is the thesis, and delegating the
recipe upholds it rather than contradicting it.

### Change 1: `docs/commands/unlock.md` refusal bullet

In "Safety checks / refusal cases", insert immediately **after** the
`- Refuses to mount degraded without explicit `--allow-degraded`` bullet
(adjacency puts the contrast where `--allow-degraded` is introduced):

```markdown
- Refuses if a present disk's LUKS UUID does not match the UUID recorded in `pool.json` -- the disk was swapped, cloned, or reformatted out of band. The error names the disk, its by-id path, and the expected vs found UUID. This is a hard error caught during the initial probe, before any mapper opens; `--allow-degraded` does not bypass it (that flag only covers *missing* disks). If unintended, detach the foreign disk and reattach the original; if the swap was intentional, see [Unlock refused by a foreign or mismatched disk](../guides/recovery-scenarios.md#unlock-refused-by-a-foreign-or-mismatched-disk).
```

Wording notes:

- This is a *semantic* mirror of the runtime hint
  (`luks::luks_uuid_mismatch_guidance`), not a byte copy. The runtime
  string is `disk was swapped, cloned, or reformatted; detach the foreign
  disk and reattach the original, or run 'braid replace' if the swap was
  intentional`. The bullet renders the "swapped, cloned, or reformatted"
  phrasing into an earlier sentence, keeps "detach the foreign disk and
  reattach the original" for the unintended case, and -- because bare
  "run `braid replace`" dead-ends (see Context) -- routes the intentional
  case to the recovery section instead of repeating the bare command.
- "names the disk, its by-id path, and the expected vs found UUID" is
  exact: the format string in the `MountError::Failed` branch
  (`cli/src/mount.rs`) prints `at {member.by_id}:` plus `expected` /
  `found` lines.
- The recovery pointer is a real Markdown link matching unlock.md's
  existing guide links (e.g. `../guides/sharing-and-permissions.md`).
  `mdbook-linkcheck2` validates the *file* half (that recovery-scenarios.md
  exists) but **not** the `#...` anchor (see Verification), so the slug
  match is a manual obligation. Command names stay inline code
  (`` `braid replace` ``), matching sibling bullets.
- Use `--` not em-dash, per the repo CLI/doc style rule.

### Change 2: `docs/guides/recovery-scenarios.md` new section

Add a short section (suggested placement: just before "## Missing disk
(drive failure)", so the link target sits next to the recipe it delegates
to). The heading below yields the GitHub-style slug
`unlock-refused-by-a-foreign-or-mismatched-disk`, which the unlock.md link
in Change 1 must match. Confirm the slug by rendering the book and clicking
the link, not via linkcheck -- `mdbook-linkcheck2` does not validate
`#fragment` anchors (see Verification).

```markdown
## Unlock refused by a foreign or mismatched disk

**Symptom:** `braid unlock` exits with `LUKS UUID mismatch`. A disk at a recorded by-id slot reports a LUKS UUID that differs from the one in `pool.json`; the error names the disk, its by-id path, and the expected vs found UUID.

**Cause:** The disk was swapped, cloned, or reformatted out of band, so its LUKS identity no longer matches the recorded member. This is a hard refusal during probing, before any mapper opens. `--allow-degraded` does not bypass it -- that flag only covers *missing* disks, and this disk is present.

### If the swap was unintended

Detach the foreign disk and reattach the original. `braid unlock` then succeeds.

### If the swap was intentional

`braid replace` requires the pool mounted, but the present mismatched disk blocks the mount. Make the slot read as *missing* first, then replace:

1. Detach the foreign disk so the member reads as absent.
2. Mount the pool degraded:
   ```sh
   sudo braid unlock --allow-degraded
   ```
3. Replace the now-missing member following [Missing disk -> Option A: Replace the disk](#option-a-replace-the-disk). `braid replace` prepares its own `--new` disk; see [`braid replace`](../commands/replace.md) for how it handles a disk that already carries a LUKS header.
```

Recipe is fully code-backed: a detached member probes as `Absent`
(`cli/src/mount.rs`), so `--allow-degraded` mounts it degraded (the same
path as the existing "Unlock with a missing disk" recipe); `braid replace`
then runs against the mounted pool with the member resolved as
`ReplaceSource::Missing`. The recipe deliberately does **not** prescribe
reusing the swapped-in foreign disk as the `--new` target. replace's
`--new` preparation is purely state-based
(`replace.rs#build_replace_work_plan`): a target with no readable LUKS
header (`PresentNotLuks`) is reformatted (`FreshLuks`); a target that
already carries a readable header (`PresentLuks`, *no* braid-label gate) is
*adopted* (`ExistingLuks`) when the pool credential opens it. The foreign
disk has a readable header -- that is precisely why unlock reported a UUID
*mismatch* rather than a *missing* disk -- so it routes to the adoption
path, contingent on its container opening with the pool credential. That
adoption-vs-reformat behavior, and the fact that the outcome varies with
the foreign container's credential, is `braid replace`'s contract to own
and document. So the recipe points at the existing fresh-drive replace
recipe and defers foreign-disk reuse to the `braid replace` command doc --
no step whose outcome the recipe cannot guarantee is prescribed.

**Also update the "Recovery decision tree"** in the same file (the page's
primary navigation index). Today it sends a pool that "won't mount"
straight to `--allow-degraded` -- the dead-end for a mismatch -- and never
points at the new section. Add a mismatch branch *above* the `missing
device / won't mount` branch so the mismatch case is caught before the
(wrong-for-it) degraded advice:

```
braid command fails
├── "pending operation" error
│   └── braid recover [--allow-degraded]
├── pool.json missing
│   └── braid discover --write → braid unlock
├── "LUKS UUID mismatch" error
│   └── see "Unlock refused by a foreign or mismatched disk"
├── missing device / won't mount
│   ├── braid unlock --allow-degraded
│   └── then: braid replace or braid remove-missing
└── something else
    └── braid doctor → check troubleshooting guide
```

Optional polish: add a one-line "see also" between this section and the
existing "Out-of-band reformat during recovery" section (which covers the
*recover-journal* path, a different trigger) so a reader who lands on one
finds the other.

### Change 3: `cli/src/mount.rs` regression test

The bullet elevates a control-flow detail ("`--allow-degraded` does not
bypass it") into a user-facing contract, but no test exercises a UUID
mismatch with `allow_degraded = true` -- both existing mismatch tests
(`mount_luks_uuid_mismatch_closed`, `mount_luks_uuid_mismatch_already_open`)
pass `false`. Add one sibling test, mirroring
`mount_luks_uuid_mismatch_closed` but flipping the `allow_degraded`
argument of `open_and_mount_for_test` to `true`, asserting the result is
still `Err(MountError::Failed(_))` with a message containing `LUKS UUID
mismatch`, and explicitly **not** `MountError::DegradedRefused` and **not**
`Ok`. The fixture already supports `true` (a sibling degraded test passes
`true`), so this is a one-argument change plus assertions. Behavioral and
structure-insensitive: it asserts on the public mount-result variant and
message, not on the internal probe/gate call sequence, so a future gate
reorder that broke the doc's claim would fail this test instead of
silently passing.

## Out of scope (explicit guards)

- **Do not** edit `status.md`, `doctor.md`, or
  `017-runtime-disk-membership.md` -- they already document the concept
  and do not contradict the new bullet.
- **Do not** touch "What happens under the hood" step 2 in unlock.md -- it
  already names the UUID-match check; the new bullet supplies the refusal
  consequence. Clean division, no redundancy.
- **Do not** prescribe reusing the swapped-in foreign disk as the replace
  `--new` target, or any `wipefs`/`cryptsetup erase` step -- replace's own
  adoption guards govern that, and it is outside this fix.
- **No** `docs/SUMMARY.md` change -- Change 2 adds a heading within an
  existing page, not a new page.
- **No production code change.** The sole code touch is the one regression
  test in Change 3.

## Verification

- **Docs build / linkcheck:** `mdbook build docs` (runs
  `mdbook-linkcheck2` per `book.toml`) confirms the new link *targets*
  exist as files -- unlock.md -> recovery-scenarios.md (Change 1) and
  recovery-scenarios.md -> `../commands/replace.md` (Change 2). It does
  **not** validate `#fragment` anchors: the pinned `mdbook-linkcheck2`
  0.12.0 `validate.rs` emits only file-not-found, incomplete-reference, and
  absolute-link diagnostics -- there is no slug/anchor resolution in
  `validate.rs`, `links.rs`, or `context.rs` (verified against the
  nix-store source), and a same-file `(#...)`-only link (the new section ->
  Option A) is not anchor-checked either. A wrong slug therefore ships
  silently through CI. **Anchor correctness is a manual gate:** render the
  book (`mdbook serve docs`) and click the new `#...` links, or hand-compute
  the GitHub-style slugs, confirming
  `#unlock-refused-by-a-foreign-or-mismatched-disk` (Change 1 -> new
  section) and `#option-a-replace-the-disk` (new section -> Option A)
  resolve.
- **Test:** `just test-rust` -- the new `mount.rs` test must pass; run the
  existing `mount_luks_uuid_mismatch_*` tests alongside it to confirm no
  regression. (No VM test needed; the behavior is already covered by
  `tests/cli/unlock-uuid-mismatch.py`, which is unchanged.)
- **Manual semantic diff** (the prose correctness gate; there is no
  automated prose test):
  - The bullet's "expected / found" + by-id wording corresponds to the
    runtime format string in the `expected_uuid != uuid` branch of
    `cli/src/mount.rs`.
  - The bullet's remediation prose semantically mirrors
    `luks::luks_uuid_mismatch_guidance` (sentence-cased, `` `braid
    replace` `` as inline code, intentional case rerouted to the recovery
    section) -- a rendered correspondence, not a byte copy.
  - The "`--allow-degraded` does not bypass it" and exit-1 claims hold:
    the mismatch returns inside the probe loop, before the
    `allow_degraded` gate later in the same function (`cli/src/mount.rs`);
    only `MountError::DegradedRefused` exits 2 in the unlock arm of
    `cli/src/main.rs`.
- **Durable anchors already in place** (cite, do not re-derive): the
  runtime guidance string is locked by
  `luks_uuid_mismatch_guidance_includes_canonical_remediation` in
  `cli/src/luks.rs`; the refusal's named disk, both UUIDs, the remediation
  hint, and "no mappers opened before refusal" are locked by
  `tests/cli/unlock-uuid-mismatch.py`. The new Change 3 test adds the one
  missing anchor (the `allow_degraded = true` path).

## Implementation notes

- Included the Change 2 "optional polish" see-also: a bidirectional one-line
  cross-link between the new "Unlock refused by a foreign or mismatched disk"
  section and the existing "Out-of-band reformat during recovery" section, so a
  reader who lands on either reformat/swap recipe finds the other (the two share
  the same live-vs-recorded LUKS UUID check on different triggers: unlock vs
  `braid recover`).
- Named the Change 3 test `mount_luks_uuid_mismatch_refused_even_with_allow_degraded`
  and placed it directly after `mount_luks_uuid_mismatch_already_open` so all
  three mismatch tests are contiguous. It asserts `matches!(err, MountError::Failed(_))`
  (which excludes `DegradedRefused`), `expect_err` (excludes `Ok`), and a message
  containing `LUKS UUID mismatch` -- covering the plan's three required assertions.
