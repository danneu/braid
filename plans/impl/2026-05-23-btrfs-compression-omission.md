# Plan: Document why braid omits btrfs compression by default

## Context

An external review flagged that braid's `base_mount_options()` does not
enable transparent btrfs compression and that no design decision
documents the omission. The verify-issue investigation confirmed both
halves of the gap:

- `cli/src/cmd.rs:429` returns `["noatime", "skip_balance",
  "subvolid=5"]`.
- A repo-wide grep for `compress` and `zstd` returns zero hits in
  `cli/`, `modules/`, `docs/design/`, `docs/guides/`, `docs/internals/`,
  and `README.md`.

The reviewer's "add `compress=zstd:1`" half is not the right action --
braid's HDD-bulk-storage target (per ADR 015) inverts the trade-off
that motivates the Fedora-on-SSD precedent. But the "document the
rationale" half is correct and fixable: ADR 015 (HDD defaults) already
documents one HDD-target-driven omission (`--allow-discards`) and is
the natural home for compression too.

Goal: when a future reviewer (human or LLM) reaches `base_mount_options()`
or browses `docs/design/decisions/`, the compression decision is
discoverable from both the code and the design tree, and pinned by
one negative test assertion so the documented invariant cannot
silently drift. The runtime behavior of `base_mount_options()` does
not change -- the assertion just makes the existing omission a
contract.

## Approach

Three edits: two docs, one one-line test assertion.

1. **Amend ADR 015** -- add an `## Alternatives considered` section
   matching ADR 005's house style, containing one rejected entry for
   default-on btrfs compression.
2. **Update the `base_mount_options()` doc comment** in
   `cli/src/cmd.rs` -- append one short paragraph noting compression
   is intentionally omitted, with a cross-reference to ADR 015.
3. **Pin the omission as behavior** -- extend the existing mount-options
   assertion in `tests/cli/braid-unlock.py` with one negative check so
   a future change that flips `base_mount_options()` to include
   `compress=...` fails a test instead of silently leaving ADR 015 and
   the code comment stale.

Rejected scope expansions:

- A standalone ADR (e.g. `028-btrfs-compression.md`). The decision is
  one paragraph and is a direct consequence of the HDD-NAS-bulk-storage
  target; a separate ADR would misframe it as an independent
  compression policy.
- Editing `docs/design/principles.md` principle 11. Current text
  ("Mount options... are chosen for HDD NAS deployments") already
  covers omissions; no rewording needed.
- Editing `docs/SUMMARY.md`. ADR 015 is already at line 60; adding a
  subsection doesn't change the TOC entry.
- Editing user-facing docs (`docs/guides/`, `docs/commands/unlock.md`,
  `README.md`). Mount options are not a user-configurable surface in
  braid today, so "how to enable compression yourself" would be a
  feature request, not a doc gap.
- Editing ADR 005 (Sane defaults). Its "Defaults applied" table is for
  things braid does enable; compression's absence belongs in the
  omissions doc (015), not the inclusions doc.

## Files to modify

### `docs/design/decisions/015-hdd-defaults.md`

Insert a new `## Alternatives considered` section between the existing
`## Tradeoffs accepted` and `## See` sections. Match the heading +
"Rejected" + rationale-paragraph pattern from ADR 005's "Alternatives
considered" (e.g. its `### Don't enable scrub by default` entry).

One rejected entry:

- **Heading:** `### Default-on btrfs compression (compress=zstd:1)`
- **Verdict line:** `Rejected.`
- **Rationale paragraph(s)** covering, in roughly this order:
  1. Target workload (HDD-bulk-storage NAS) is dominated by media
     content (video, audio, photos, archives) that is already
     compressed at the application layer. Compression yields ~zero
     space saving on that mix.
  2. The btrfs heuristic that skips compressing incompressible extents
     still costs CPU per write; on low-power NAS hardware that cost is
     not free.
  3. Reversal is partial, not free. Removing the mount option affects
     future writes only; extents already written compressed stay that
     way until the data is rewritten or explicitly defragged. Making
     compression the default in unreleased software bakes that
     conversion cost in for anyone who later discovers their workload
     differs.
  4. Fedora's `compress=zstd:1` precedent is workstation-root on SSD
     (binaries, logs, configs) -- a different workload than HDD
     bulk-storage NAS. The precedent does not transfer.
  5. Escape hatch is already in place: users whose workload is
     compression-friendly (text, code, document servers) can opt
     specific paths into compression today with
     `btrfs property set <path> compression zstd`. This is the
     modern, per-inode interface documented in
     `reference/btrfs-progs/Documentation/btrfs-property.rst`
     (`chattr +c` is the legacy ext2-style interface and defaults to
     zlib, so don't recommend it). No braid feature gate is needed
     for users to do this today.

Mention `cli/src/cmd.rs` `base_mount_options` in the new section's
prose or add a bullet to the existing `## See` list -- one or the
other, not both.

### `cli/src/cmd.rs`

Existing `///` doc comment on `base_mount_options()` (around lines
418-428) currently documents each option that is present. Append one
short paragraph at the end of the doc comment, before the `fn` line,
noting that compression is intentionally omitted and pointing at ADR
015. Match the comment's existing style (short, intent-focused, no
restatement of code). One to two lines.

No code change inside the function body.

### `tests/cli/braid-unlock.py`

The existing post-unlock mount-options check at lines 152-155
already asserts `skip_balance` and `subvolid=5` appear in
`findmnt -o OPTIONS -n /mnt/storage`. Add one negative assertion in
the same block:

```python
assert "compress" not in opts, (
    f"Expected no compression option in mount options "
    f"(see ADR 015), got: {opts}"
)
```

Rationale: exact-argv unit tests at `cli/src/cmd.rs:2707-2751`
(`mount_includes_skip_balance` and
`mount_with_options_includes_skip_balance`) already pin
`base_mount_options()`'s output to the literal
`"noatime,skip_balance,subvolid=5"` (and the `,degraded` variant
for `MountWithOptions`), so a direct addition of `compress=...`
inside `base_mount_options()` will fail unit tests before reaching
VM tests. The new VM assertion is not closing that gap; it
complements those unit tests by pinning the live `findmnt` mount
state, which catches a different class of regression -- e.g.
compression injected at a wrapper or systemd-mount layer outside
`base_mount_options()`, or a future refactor that routes mount
options through a new helper not covered by the existing argv
tests. The `see ADR 015` hint in the failure message routes a
future implementer to the ratified rationale before they
re-litigate the decision in the diff.

No new test case, no new preamble -- this is one assertion added to
an existing `# skip_balance and subvolid=5 must appear in mount
options` block. Update the section's comment to reflect the third
check (e.g. `# Mount options pinned by ADR 015: skip_balance,
subvolid=5, no compression`).

## Verification

- `mdbook build docs` from the repo root. ADR 015 is reachable via
  `docs/SUMMARY.md`, and any cross-link to it that the edit introduces
  is validated by `mdbook-linkcheck` per Decision 5. A broken anchor
  fails the build.
- Visual read-through of ADR 015 to confirm the new section flows from
  Context -> Decision -> Tradeoffs accepted -> Alternatives considered
  -> See without restating itself.
- Visual read-through of the updated `base_mount_options()` doc
  comment to confirm the new line is consistent with the existing
  per-option paragraphs and does not duplicate the ADR text.
- `just test-rust` -- not strictly required for a docs/comment change,
  but cheap and confirms the comment doesn't break doc-test parsing.
- `just test-vm braid-unlock` -- runs the existing unlock VM test
  (now extended with the negative compression assertion). This is the
  one VM test the plan touches; full-suite reruns are not warranted
  for a docs + single-assertion change.
- No fixture refresh, no parser canary -- behavior is unchanged.

## Out of scope

- Any change to `base_mount_options()` return value.
- Any new braid configuration option to make mount options
  user-overridable. That is a separate feature; this plan only
  documents the current omission.
- Any change to `docs/design/principles.md`.
- Any change to user-facing guides or `README.md`.
- Any new VM test file or new test case in `braid-unlock.py` -- the
  pin is one assertion added to the existing post-unlock
  mount-options check, not a new scenario.
