# Plan: docs note on btrfs space-accounting pitfalls

## Context

An audit finding (verify-issue input) flagged that braid's user docs do
not warn operators about two non-obvious btrfs space-accounting
behaviors:

1. **Snapshots hold deleted file blocks.** `rm` does not reclaim a
   file's space while any snapshot still references it. The current
   snapshots subsection at
   [`docs/guides/day-to-day-nas-usage.md:103-111`](../../docs/guides/day-to-day-nas-usage.md)
   only says snapshots "use no extra space until the original data
   changes" -- which is true at creation time but quietly misleads the
   user about deletes.
2. **`df` underreports btrfs.** `df` cannot see btrfs's data/metadata
   pool split, so a pool that looks "full" or "empty" via `df` can be
   the opposite when measured with `btrfs filesystem usage`. `braid
   status` already surfaces capacity from `btrfs filesystem usage`
   internally
   ([`cli/src/status.rs:441-466`](../../cli/src/status.rs)), but no
   user-facing doc names the btrfs command for an operator who reaches
   for `df` directly.

The finding's original recommendation was a brand-new "Understanding
Space Accounting" section in
[`docs/guides/day-to-day-nas-usage.md`](../../docs/guides/day-to-day-nas-usage.md).
That overshoots: it duplicates content already in
[`docs/guides/troubleshooting.md:7-19`](../../docs/guides/troubleshooting.md)
(the `-dusage=0/10` balance fix), and braid intentionally leaves
general btrfs education to upstream docs in
[`reference/btrfs-progs/Documentation/`](../../reference/btrfs-progs/Documentation/).

The right shape is two surgical edits to sections that already exist.
Outcome: an operator who deletes a file and is surprised that space
didn't free, or who runs `df` and is surprised by the numbers, finds
the answer in the doc thread they're already in.

## Changes

### 1. `docs/guides/day-to-day-nas-usage.md` -- snapshots subsection

After the existing last sentence at line 111 ("Snapshots are nearly
instant and use no extra space until the original data changes."), add
roughly two sentences capturing:

- Deleting a file does **not** reclaim its blocks while any snapshot
  still references them.
- To free that space, delete the snapshots holding the data with
  `sudo btrfs subvolume delete /mnt/storage/.snapshots/<name>`.

Style: plain prose, matching the rest of the section. No new heading,
no admonition box -- the file has no admonition pattern (per
exploration: it uses inline bold like `**Independent snapshots**`,
nothing else).

Optional one-liner cross-link to the troubleshooting ENOSPC entry if
it reads naturally; skip if it makes the paragraph cluttered.

### 2. `docs/guides/troubleshooting.md` -- existing balance-ENOSPC section

In the "Balance fails with 'No space left on device'" section
(lines 7-19), add one sentence -- placed before the existing `**Fix:**`
line -- that:

- Tells the operator to first confirm where space went with
  `sudo btrfs filesystem usage /mnt/storage`, because `df`'s "Used" /
  "Available" columns cannot distinguish data, metadata, and snapshot
  references.
- Notes that `braid status` reports the same capacity (so users who
  trust braid output already have the answer; the btrfs command is
  for deeper inspection).

Do **not** add a new sibling section. The existing section already
covers the symptom ("appears to be space available" -- exactly the df
mismatch case); we are augmenting its diagnostic step, not adding a
new one.

### Out of scope

- **No new "Understanding Space Accounting" section.** Duplicates
  existing content; expands braid docs into general btrfs reference.
- **No README.md changes.** README is cookbook-style and doesn't touch
  snapshots/space; adding caveats there breaks its tone.
- **No new admonition pattern.** The guides use plain prose with bold
  prefixes (`**Fix:**`, `**Symptom:**`); don't introduce blockquote /
  `> Note:` / mdBook admonition styles.
- **No code changes.** `braid status` already reports `btrfs
  filesystem usage`-derived capacity; `braid doctor` already has
  `metadata_profile_mismatch`. No CLI surface needs to change.

## Verification

- `mdbook build docs` exits clean -- `mdbook-linkcheck` validates any
  cross-links per [`docs/book.toml`](../../docs/book.toml).
- Visual check: open the rendered snapshots subsection and the
  troubleshooting ENOSPC entry in the built mdBook; confirm the new
  sentences flow with the surrounding prose and the bold-prefix
  pattern is unchanged.
- Walkthrough check: a reader landing on the snapshots subsection
  after `rm`-ing files now sees why space didn't free; a reader
  landing on the balance-ENOSPC section now knows to run `btrfs
  filesystem usage` before applying the `-dusage` fix.
- No tests to run -- pure prose change.
