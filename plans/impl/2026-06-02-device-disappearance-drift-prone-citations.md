# Plan: drop the inline btrfs-progs version + drift-prone line citation in device-disappearance.md

## Context

`docs/internals/tool-behavior/device-disappearance.md` (the "Fully gone" state,
one sentence in the section body) tells a reader debugging probe/monitor/alert
code that braid's pinned btrfs-progs is **v6.17.1**. That is wrong:

- `reference/btrfs-progs/VERSION` is `v6.19.1`.
- ADR 027 (`docs/design/decisions/027-mkfs-block-group-tree.md`, status
  **Active**, the record that owns the toolchain pin) states the pin is
  **6.19.1** and names **6.17.1** as the *old* nixos-25.11 version.

Root cause: commit `9d237f7b chore: bump nixpkgs pin to nixos-26.05` swept
`6.17.1 -> 6.19.1` through ADR 027, `cli/src/cmd.rs` doc comments, fixtures, and
the mkfs nix/py tests, but its changed-files set did not include
`device-disappearance.md`. The doc was simply missed; no later commit corrected
it. The impact is exactly the kind the file's own intro warns about -- a reader
chasing btrfs output-format details against the wrong btrfs-progs source.

The same sentence also cites the btrfs source as `device.c:625-634` -- a
line-number reference in `docs/` prose. This is the **same root cause** as the
version error: a drift-prone reference into vendored btrfs-progs. The version
*number* already rotted; the line *numbers* are next (they shift whenever
`just fetch-references` pulls a new btrfs-progs). The line citation also
violates the project's File References rule in `AGENTS.md` ("In ADRs, decision
docs, and `docs/` prose, never reference another file by line number ... use
`path#symbol`").

The ideal fix closes that shared root cause completely rather than resetting its
clock. `git grep` confirms line 54 is the **sole** doc outside ADR 027 that
asserts the *current pinned* btrfs-progs version as a bare inline number. (ADR
027 names both 6.19.1 and the historical 6.17.1; `balance-soft.md` names 6.19.1
only inside a `version + tag + commit` provenance triple for a quoted RST
passage -- a commit-pinned snapshot citation that does not rot when
`just fetch-references` advances, so it is a different surface and stays.) So the
number on line 54 is a lone duplicate of ADR 027's authoritative value -- and
precisely the copy the bump commit missed. Re-typing `6.19.1` would re-plant that exact
drift surface for the next bump to miss again. Instead, remove the inline number
and let the symbol citation into `reference/btrfs-progs/` (which **is** the
pinned version by construction) carry "which version," with ADR 027 left as the
single authoritative statement of the number.

Intended outcome: the sentence names **no** btrfs-progs version and **no** line
range -- both drift-prone tokens are gone -- and cites the btrfs source by a
stable, greppable symbol. The doc defers "which version" to ADR 027 and the
reference tree, matching every other internals doc.

## The fix

Two edits, both inside the single sentence at the end of the **Fully gone**
section of `docs/internals/tool-behavior/device-disappearance.md`. Current text:

> Pinned btrfs-progs v6.17.1 renders the missing-device stats path as
> `[devid:N]` (`device.c:625-634`); `[<missing disk>]` is an older btrfs
> rendering.

1. **Remove the inline version number.** `Pinned btrfs-progs v6.17.1 renders`
   -> `The pinned btrfs-progs renders`. Drop the `v6.17.1` token entirely; keep
   the stable descriptor word "pinned" (it is not a drift surface, and it
   preserves the pinned-vs-older contrast that the next clause -- "an older
   btrfs rendering" -- depends on). ADR 027 stays the single source of truth for
   the number; the symbol citation (edit 2) lands the reader directly in the
   pinned `reference/btrfs-progs/` tree, so the sentence needs no number to be
   actionable.

2. **Replace the line-range citation with a symbol.** `(`device.c:625-634`)`
   -> `(`cmds/device.c#print_device_stat_string`)`.
   - Keep it a plain code span (backticks), **not** a Markdown link --
     `reference/` lives outside the mdBook root, so per `AGENTS.md` a link would
     404 in the rendered book and dodge linkcheck. A plain code span is the
     prescribed form and is correctly ignored by `mdbook-linkcheck2`.
   - `cmds/device.c` matches how `AGENTS.md` refers to btrfs-progs source files
     (e.g. `cmds/scrub.c`); `print_device_stat_string` is the drift-proof,
     greppable half (`rg print_device_stat_string reference/btrfs-progs/` finds
     the definition). The symbol is also *more* precise than the line range: it
     is the plain-text/JSON renderer braid parses, distinct from
     `print_device_stat_tabular` (the `-T` mode renderer, which braid does not
     use).

Resulting sentence:

> The pinned btrfs-progs renders the missing-device stats path as `[devid:N]`
> (`cmds/device.c#print_device_stat_string`); `[<missing disk>]` is an older
> btrfs rendering. braid does not depend on either string: the parser ignores
> the device field and keeps the row's `devid` and counters.

The technical claim itself ("renders ... as `[devid:N]`", and "braid does not
depend on either string") stays as-is -- it holds at 6.19.1 and is out of scope
for this fix.

## Files to modify

- `docs/internals/tool-behavior/device-disappearance.md` -- the one sentence
  described above. No other file changes; ADR 027's `6.17.1`/`6.19.1` mentions
  are the correct, authoritative references and must stay.

## Out of scope (intentionally)

- The plain-text-vs-JSON nuance of `print_device_stat_string` (the JSON branch
  prints `device`; plain text does not). The doc disclaims dependence on the
  string and braid keys on `devid`; re-auditing that claim is a separate concern.
- The em-dashes in the sentence. The file already uses the Unicode form, so the
  global ASCII-punctuation style rule exempts them -- leave them untouched;
  ASCII-ifying would be unrelated churn, and the two edits above do not touch
  the em-dash anyway.

## Verification

1. `git grep -nE "6\.(1[0-9]|2[0-9])\.[0-9]" -- docs/internals/tool-behavior/device-disappearance.md`
   -> **empty**. No inline btrfs-progs version remains in this doc (the DRY
   invariant the fix establishes; also covers the reviewer's gap -- with no
   number there is nothing to assert correct, so absence *is* the assertion).
2. `git grep -nE "6\.(1[0-9]|2[0-9])\.[0-9]" -- docs` ->
   `device-disappearance.md` no longer appears. The remaining matches are all
   intentional and out of scope: `027-mkfs-block-group-tree.md` (current +
   historical pin), `balance-soft.md` (`6.19.1` inside `version + tag + commit`
   provenance citations), and `luks-sector-size.md` (a `6.18.x` *Linux kernel*
   version, not btrfs-progs).
3. `git grep -n "device\.c:" -- docs` -> **empty** (no line-number citation
   remains).
4. `rg "print_device_stat_string" reference/btrfs-progs/cmds/device.c` ->
   resolves, proving the cited symbol is greppable/stable.
5. `mdbook build docs` -> still builds clean; the changed code span is not a
   link, so linkcheck is unaffected (sanity check that nothing else broke).
6. Read the edited sentence in context to confirm it scans correctly and the
   pinned-vs-older contrast still reads.

## Implementation notes

- The CONTEXT "sole doc" claim and verification step 2 originally stated
  `git grep -nE "6\.(1[0-9]|2[0-9])\.[0-9]" -- docs` returns only ADR 027.
  It actually also matches `balance-soft.md` (`6.19.1` inside `version + tag +
  commit` provenance triples) and `luks-sector-size.md` (a `6.18.x` Linux
  kernel version). Both are intentional and out of scope -- balance-soft's are
  commit-pinned snapshot citations that do not rot on a references refresh, and
  luks-sector-size's is a kernel version, not btrfs-progs -- so the plan's
  "No other file changes" boundary is correct. Corrected both passages to match
  reality; the impl (device-disappearance.md only) is unchanged, and the step's
  real assertion (device-disappearance.md drops out of the grep) holds.
