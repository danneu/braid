# Plan: document drift-as-member cleanup in `lock.md` step 5

## Context

A Low-severity "project fit" finding flagged that `docs/commands/lock.md`'s
"What happens under the hood" steps 5-6 never state the central post-ADR-024
behavior: a `braid-*` mapper opened under a *drifted* name that resolves to a
member's LUKS UUID is closed (and reported) as that member, not skipped or
reconstructed as the expected `braid-<name>`.

Investigation outcome (the finding's headline is overstated, the underlying gap
is real but small):

- **Code is correct.** `cli/src/lock.rs` `build_close_sets_full` Pass 1 iterates
  `pool.devices`, looks up each by `dev.luks_uuid` via `membership.by_uuid()`,
  and on a match closes the *observed* `dev.mapper` (e.g. `braid-WRONG`) as
  `MemberOwned { display_name: member.name }`. This is exactly ADR-024
  "Cleanup follows observed ownership."
- **Behavior is pinned.** `tests/cli/luks-mapper-drift.py` opens `disk1` as
  `braid-WRONG`, runs `braid lock`, and asserts `disk disk1: locking`/`locked`
  (operator name) while `braid-WRONG` is closed and no `already closed` row
  appears. No test change is needed.
- **Doc is already accurate, just terse.** Step 5 already reads "Classifies live
  mappers by LUKS UUID/devid ownership, then closes member-owned *observed
  mapper names*" -- it already names the UUID/devid axis and the observed-mapper
  close, so it does **not** read as the pre-migration "member by name, else
  orphan" model the finding's Impact paragraph claims. The only genuine gap is
  that the operator-facing *consequence* of the drift case is never spelled out
  for someone debugging a `braid-WRONG` mapper.

The intended outcome: bring `lock.md`'s cleanup narrative to the same explicit,
ADR-cross-referenced standard the read-path doc (`status.md`) already meets for
mapper-drift tolerance -- a one-clause clarification, no behavior change.

## Scope decision: step 5, not step 6

The finding proposed "step 5/6," but member classification (and the
drift-as-member close) happens in the *member* pass, which the doc narrates in
**step 5**. Step 6 ("Scans for orphaned `braid-*` mappers not owned by
UUID-keyed membership") is the *non-member* orphan pass and is accurate as-is;
adding member-drift language there would be wrong. Once step 5 states that
drifted-name members are still classified as members by UUID, step 6's "not
owned by UUID-keyed membership" correctly reads as "genuinely not a member."
**Step 6 is left unchanged.**

## Change

Single file: `docs/commands/lock.md`, step 5 (currently line 42).

Append one sentence making the drift consequence explicit and cross-referencing
ADR-024, matching the sentence-plus-ADR-link style the same doc already uses in
its Error-handling bullet (`lock.md:65`, which cites
`024-luks-uuid-identity.md#runtime-handles-and-labels`).

Recommended wording (exact phrasing at implementer's discretion; keep it ASCII
per house style -- `--`, `'`/`"`, `...`):

> 5. Classifies live mappers by LUKS UUID/devid ownership, then closes
>    member-owned observed mapper names, retrying up to 3 times if the device is
>    busy. Because classification is by backing UUID and not by mapper name, a
>    member opened under a drifted name (e.g. `braid-WRONG` backed by `disk1`)
>    is still closed -- and its progress row reported -- as that member
>    (`disk1`), not skipped or reconstructed as `braid-disk1`. See
>    [ADR-024](../design/decisions/024-luks-uuid-identity.md#concrete-improvements)
>    ("Cleanup follows observed ownership").

Anchor choice: `#concrete-improvements` lands the reader on the
"Cleanup follows observed ownership" bullet, which carries the exact
`braid-WRONG`/`disk1` example. (`#runtime-handles-and-labels` -- the
authoritative numbered lock spec, point 7, and the anchor `lock.md:65` already
uses -- is an equally valid alternative; the two cites then point at the two
distinct sub-topics, which is fine.)

## Reuse / consistency anchors

- **Precedent in the same file:** `lock.md:65` already embeds a one-sentence
  behavior note with a trailing `See [ADR-024](...)` link -- match that shape.
- **Sibling pattern to mirror:** `docs/commands/status.md` (around the
  LUKS-UUID membership-join notes, lines ~396 and ~471-475) already spells out
  mapper-drift tolerance explicitly for the *read* path and defers the model to
  decision 024. This change makes the *cleanup* path symmetric.
- **Single source of truth:** ADR-024 remains the authority; no new shared doc
  is introduced.

## Out of scope

- No `cli/src/lock.rs` change (code already implements the behavior).
- No test change (`tests/cli/luks-mapper-drift.py` already pins it).
- No `README.md` change (README mentions `braid lock` only at a high level,
  `README.md:124`, with no cleanup/drift detail).
- No `docs/guides/**` change (no guide documents the close-classification model).

## Verification

1. `just docs-build` -- builds the mdBook and runs `mdbook-linkcheck2`, which
   validates the new `024-luks-uuid-identity.md#concrete-improvements` anchor
   link; a bad slug fails the build (and CI).
2. Read the rendered `commands/lock.md` "What happens under the hood" section
   and confirm step 5 reads cleanly and step 6 is unchanged.
3. No Rust/VM run required: the documented behavior is unchanged and already
   covered by `tests/cli/luks-mapper-drift.py`.
