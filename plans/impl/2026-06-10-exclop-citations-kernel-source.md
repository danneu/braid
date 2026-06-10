# Fix exclop string/set source-of-truth citations: kernel, not btrfs-progs

## Context

`braid` reads `/sys/fs/btrfs/<fsid>/exclusive_operation` and parses its string
value (`balance`, `device replace`, ...) in `cli/src/preflight.rs#ExclusiveOp`
(`ExclusiveOp::parse`). Several doc-comments and ADR-016 prose attribute the
**source** of those strings to btrfs-progs `exclop_def[]`
(`reference/btrfs-progs/common/utils.c`). That attribution is backwards:

- The kernel **emits** the strings in `btrfs_exclusive_operation_show`
  (`reference/linux/fs/btrfs/sysfs.c`); the set itself is
  `enum btrfs_exclusive_operation` (`reference/linux/fs/btrfs/fs.h`).
- btrfs-progs `exclop_def[]` is a **fellow parser** -- `get_fs_exclop()` reads
  the same sysfs file and `strcmp`s against that table, exactly analogous to
  braid's own `ExclusiveOp::parse`. It is a cross-reference, not the authority.

This surfaced while verifying a (redundant) test-coverage finding whose stated
worry was "a kernel that exposes the attribute differently." The correct, cheap
answer to that worry is to pin the citations to the kernel -- the thing that
could actually drift -- rather than to a sibling consumer. No behavior changes;
this is doc-comment/prose accuracy plus a citation-convention cleanup.

Outcome: every live site that tells a reader *where these strings come from*
points at the kernel, btrfs-progs is retained only as a clearly-labeled fellow
parser, and the touched `reference/` citations follow the no-line-number
convention.

## Convention (authority: `docs/dev/reference-source.md#citing-reference-code`)

- **Never use line numbers** for `reference/` code (gitignored, refreshed by
  `just fetch-references`; line numbers drift, nothing in CI validates them).
- Cite vendored code by **shape**: a `(fn name)`/symbol pointer, or an inline
  excerpt fenced ` ```c ` / ` ```text ` (never ` ```rust ` or untagged -- those
  run as doctests and fail `cargo test --doc`).
- `pkg <version>` stamp = `git -C reference/<pkg> describe --tags`. btrfs-progs
  is `v6.19.1` (clean), so its cross-reference carries the version. `reference/linux`
  has **no real kernel tag** (returns a junk `v0.0.2-...`), so cite the kernel
  **by symbol/paraphrase only, no version stamp**.
- Docs prose: cite code paths as **code spans, not links** (`cli/` and
  `reference/` are outside the mdBook root; a link 404s and dodges linkcheck) --
  `docs/dev/doc-citations.md`.

## Changes

All edits change only the citation clause; preserve each comment's existing
intent/invariant prose (the `///` boundary-doc requirement in `AGENTS.md`).

### Core -- the misattribution class

1. **`cli/src/preflight.rs#ExclusiveOp`** (enum doc-comment, ~L79-80). Replace
   "String values follow `exclop_def[]` in btrfs-progs `common/utils.c:1186-1194`..."
   with a kernel-as-authority sentence that keeps btrfs-progs as a fellow parser:
   > The kernel emits these strings from `btrfs_exclusive_operation_show`
   > (`reference/linux/fs/btrfs/sysfs.c`) -- that switch is the authority for
   > what this file can contain. btrfs-progs is a fellow parser of the same file
   > (`btrfs-progs v6.19.1, reference/btrfs-progs/common/utils.c (get_fs_exclop,
   > exclop_def[])`), not the source.

   Do **not** inline the kernel's 8-arm switch: the strings already live
   directly below in `ExclusiveOp::parse`'s match arms; a pointer avoids
   duplicating them.

2. **`cli/src/preflight.rs#ExclusiveOp` -> `DeviceRemove` variant** (~L86-88).
   It already asserts "The kernel writes 'device remove'" then cites btrfs-progs.
   Re-point to the kernel case label:
   > ...The string is the `BTRFS_EXCLOP_DEV_REMOVE` arm of
   > `btrfs_exclusive_operation_show` (`reference/linux/fs/btrfs/sysfs.c`).

3. **`cli/src/idle.rs#BusyReason`** (scrub variant doc, ~L30-31). "not in the
   kernel exclusive-operation set (see `reference/btrfs-progs/common/utils.c:1188-1197`)"
   -> cite the kernel set:
   > not in the kernel exclusive-operation set (`enum btrfs_exclusive_operation`,
   > `reference/linux/fs/btrfs/fs.h`)

4. **`docs/design/decisions/016-auto-suspend.md`** (L32 and L75). Both cite
   btrfs-progs `common/utils.c`/`exclop_def[]` for "scrub is not in the kernel's
   exclusive-operation set." Re-point both to `enum btrfs_exclusive_operation`,
   `reference/linux/fs/btrfs/fs.h` (code span, no link, no line numbers); drop
   the `exclop_def[]` framing.

5. **Pseudo-dir path-table citation -- `reference/linux/fs/btrfs/sysfs.c:29-47`,
   the same comment-block table cited in THREE places.** It is a path-mapping
   comment block, not a symbol. Rewrite each by shape: drop `:29-47`, keep
   `reference/linux/fs/btrfs/sysfs.c` + a one-line paraphrase (its sysfs path
   table lists `features`/`debug` as the only non-`<uuid>` entries). No version
   stamp (kernel has no clean tag). The surrounding code/prose already names
   `features`/`debug`, so a pointer + paraphrase suffices; no inline excerpt.
   Sites:
   - `cli/src/preflight.rs#BTRFS_SYSFS_NON_FSID_ENTRIES` (~L224).
   - `cli/src/idle.rs` test `idle_skips_features_and_debug_pseudo_dirs` (~L518)
     -- a `/* */` block comment; the path is currently bare, so also wrap it in
     a code span.
   - `docs/design/decisions/016-auto-suspend.md` pseudo-dir paragraph (~L79;
     code span, no link).

   `idle.rs` and ADR-016 are files Core already edits (items 3-4), so delining
   here is required for those files to be internally consistent -- not optional.
   Leaving any of the three also silently defeats the verification gate below.

### Polish -- same root cause in test prose

6. **Test-comment shorthand** in `cli/src/preflight.rs` (~L1095, L1133, L1137,
   L1726) uses `exclop_def[]` as a name for the recognized set -- the same
   misattribution in test prose. Reframe to "the kernel exclop set" / "a value
   the kernel's `btrfs_exclusive_operation_show` would not emit". Mechanical.

### Out of scope

- **`plans/impl/*.md`** (`2026-04-02-sysfs-exclusive-op-preflight.md`,
  `2026-04-28-idle-sysfs-exclusive-operation.md`,
  `2026-05-11-type-encode-exclusive-op-idle.md`,
  `2026-05-07-replace-scrub-collision-hint.md`). Frozen historical records;
  rewriting their citations misrepresents what was known at authoring time.

## Reuse / reference

- Kernel emitter: `reference/linux/fs/btrfs/sysfs.c` (`btrfs_exclusive_operation_show`,
  switch ~L1269-1297). Kernel set: `reference/linux/fs/btrfs/fs.h`
  (`enum btrfs_exclusive_operation`, ~L425).
- btrfs-progs fellow parser (cross-ref): `btrfs-progs v6.19.1,
  reference/btrfs-progs/common/utils.c (get_fs_exclop, exclop_def[])`.
- Inline-excerpt style precedent (if ever needed):
  `cli/src/parse/cryptsetup_luks_version.rs#parse_cryptsetup_luks_version`.

## Verification

- `just test-rust` (or `cargo test -p braid-cli`): confirms no `///` got detached
  from its item and nothing structural broke (comment-only change).
- `cargo test -p braid-cli --doc`: guards against an accidental doctest -- only
  relevant if anyone takes the optional inline-excerpt path; any fenced block
  must be ` ```c ` / ` ```text `.
- `just docs-build`: runs `mdbook-linkcheck2`; confirms the ADR-016 edits add no
  broken links and that code paths are cited as code spans, not links.
- Grep sweep (btrfs-progs source claims): `rg "exclop_def|common/utils\.c" cli/ docs/`
  should leave **only** the deliberate fellow-parser cross-reference in
  `ExclusiveOp` (item 1). Every other live hit should be gone; remaining hits are
  expected only under `plans/impl/`.
- Grep sweep (kernel line-numbers): `rg "reference/linux/fs/btrfs/sysfs\.c:[0-9]" cli/ docs/`
  must return **nothing** -- all three pseudo-dir citations (item 5) are delined.
  Sibling `reference/linux/*.c:NN` citations in untouched files (`scrub.c`,
  `backref.c`, `block-group.c`) are deliberately left alone; the convention only
  triggers on files we edit.
- Review gate: no CI validates `reference/` citations (absent on clean
  checkout), so correctness is by eye against
  `docs/dev/reference-source.md#citing-reference-code` -- confirm no line numbers
  on any edited `reference/` citation and the kernel is named as the authority.
