# Fix: luks-sector-size.md is wrong about braid's LUKS sector size

Status: ready to implement
Scope: documentation only (one file rewrite). No code, test, fixture, or ADR changes.

## Context

`docs/internals/btrfs/luks-sector-size.md` tells maintainers "We use the
default 512-byte LUKS sector size." That is false on the target hardware.
braid omits `--sector-size` on `cryptsetup luksFormat`, so cryptsetup
**auto-detects** the encryption sector size from the device. On the NAS's
8TB+ SATA HDDs (4Kn or 512e) that yields **4096-byte** LUKS sectors; only
512-byte-physical-sector disks (the test USB sticks / VM virtio disks) get
512-byte LUKS sectors. The doc's central claim, and the framing of several
sections that lean on it, are wrong.

The outcome we want: a maintainer reading this doc comes away knowing braid
delegates sector size to cryptsetup auto-detect (optimal per device), why
braid neither sets nor allows overriding it, and that the btrfs
read-modify-write analysis is a worst-case reassurance, not a description of
the NAS configuration.

This was raised as a High/Accuracy review finding. The finding's headline is
correct and its proposed Summary wording is a good base, but two things must
not be carried over:
- The finding claims "the doc's own captured luksDump fixtures show
  `sector_size:4096`." They show **512** (VM disks report 512-byte physical
  sectors). The fixtures *confirm* auto-detect; they are not evidence of 4096.
- The finding proposed editing only Summary lines 9-12. The misconception is
  reinforced in "Our hardware" and "Decision" too, so a Summary-only edit
  leaves the doc self-contradictory.

## Verified facts (authoritative sources)

(The `reference/` paths below are the planner's verification trail for this wip
plan. The shipped doc must NOT carry `reference/...` path pointers -- it
inlines + captions the decisive excerpt instead. See Citation method below.)

- braid never sets `--sector-size`: `cli/src/cmd.rs#CryptsetupLuksFormat`
  builds `luksFormat --type luks2 --batch-mode --key-file=- --uuid <u>
  --label <l> [user extras] <device>` -- no `--sector-size`. User extras come
  from `LuksFormatExtraOpts`.
- braid also *rejects* operator `--sector-size`:
  `cli/src/types.rs#MANAGED_LUKS_FORMAT_LONG_FLAGS` lists `--sector-size`, and
  `cli/src/types.rs#LuksFormatExtraOpts::parse` rejects it. Rationale (test
  `luks_format_extra_opts_rejects_sector_size`): a non-default sector size can
  shift cryptsetup alignment and make braid's fresh-LUKS payload-offset
  capacity estimate unsafe.
- cryptsetup auto-detects when `--sector-size` is omitted:
  `reference/cryptsetup/man/common_options.adoc` (LUKSFORMAT branch):
  "set based on the underlying data device if not specified explicitly ...
  native 4096-byte physical sector devices -> 4096 ... 4096/512e -> 4096 ...
  drives reporting only a 512-byte physical sector -> 512." Source path:
  `reference/cryptsetup/lib/setup.c` autodetect branch ->
  `reference/cryptsetup/lib/utils_device.c` returns the device physical block
  size (4096 on 4Kn/512e).
- Fixtures `cli/tests/fixtures/nixos-26.05/cryptsetup-luks-dump.json` and the
  `nixos-unstable` mirror both record `"sector_size":512` -- correct for the
  512-byte VM disks; nothing to change.

## What is NOT changing (and why)

- **Code**: braid's behavior is correct. Omitting `--sector-size` plus
  rejecting overrides is exactly right.
- **`docs/commands/replace.md`** (refusal cases, the `--sector-size` bullet):
  accurate as written -- it lists `--sector-size` among *rejected*
  `--luks-format-arg` overrides, which is true. Leave it. (Evaluated; do not
  re-flag.)
- **Tests / fixtures**: the `"sector_size":512` fixtures are correct;
  `cli/src/preflight.rs` already parametrizes a 4096 sector size, proving the
  code assumes no fixed value. No test asserts a contradictory claim.
- **ADRs**: this decision lives only in this internals doc, which is its
  appropriate home (rationale note, not a cross-cutting invariant). No ADR
  needed.
- **`docs/SUMMARY.md`** line linking `internals/btrfs/luks-sector-size.md`:
  unchanged (filename stays the same).
- **`docs/internals/luks-unlock.md`** (replace-target capacity section,
  ~`offset`/`size` discussion): already names the same `--sector-size` rejection
  and the fresh-LUKS 16 MiB offset assumption, and it's consistent -- it scopes its
  "exact at any sector_size" claim to *existing* containers, while this rewrite's
  rationale targets *fresh* targets. Evaluated; no change. (Section 3 below must
  stay fresh-scoped so the two docs don't read as contradictory.)

## Target rewrite (full thesis reframe)

Rewrite `docs/internals/btrfs/luks-sector-size.md` so the thesis is
"cryptsetup auto-detects the optimal sector size, so braid neither sets nor
allows overriding it," and the btrfs amplification analysis is demoted to a
supporting aside. Keep the front-matter (`status: Active`); refresh the
`intent:` line to match the new thesis. Use ASCII `--`, not em-dashes.

New section structure:

1. **`## Summary`** -- braid does not pass `--sector-size` to
   `cryptsetup luksFormat`, and rejects operator attempts to set it.
   cryptsetup therefore auto-detects the encryption sector size from each
   device, which is already the optimal value, so braid never chooses one.

2. **`## What auto-detect picks`** -- state the per-device rule. Back it with an
   inlined, captioned excerpt of the man-page contract (per Citation method
   below) -- the LUKSFORMAT `--sector-size` paragraph captioned
   `cryptsetup 2.8.6, man/common_options.adoc`; optionally also inline the
   decisive source line from `device_optimal_encryption_sector_size`. Do NOT
   emit a `reference/` path:
   - native-4K (4Kn) and 512e drives -> 4096-byte LUKS sectors
   - drives reporting a 512-byte physical sector -> 512-byte LUKS sectors

   Then "On our hardware":
   - NAS drives: 8TB+ SATA HDDs (4Kn or 512e) -> **4096-byte** LUKS sectors,
     matching the physical sector.
   - Test drives: USB sticks / VM virtio disks reporting 512-byte sectors ->
     **512-byte** LUKS sectors. Note the committed `luksDump` fixtures show
     `"sector_size":512` for exactly this reason (turns the finding's
     backwards claim into a correct one).

3. **`## Why braid doesn't override it`** -- two reasons:
   - Auto-detect already yields the optimal value per device, so passing
     `--sector-size` gains nothing and only adds a format-time parameter that
     can't change without re-encryption.
   - braid additionally rejects `--sector-size` as a `--luks-format-arg`
     override -- reference `cli/src/types.rs#LuksFormatExtraOpts::parse` as a
     code span -- because a non-default sector size can shift the fresh-LUKS
     payload offset and make braid's capacity estimate unsafe. Keep this
     rationale scoped to *fresh* targets: `docs/internals/luks-unlock.md` already
     covers the existing-container case (capacity is exact at any sector_size),
     so naming "fresh" here keeps the two docs from reading as contradictory.
     Optionally link the refusal-cases heading in `docs/commands/replace.md`
     (verify the exact mdbook anchor slug at implementation; mdbook-linkcheck2
     gates it -- if in doubt, keep only the code-span reference, which is not
     linkchecked).

4. **`## Aside: even 512-byte LUKS sectors are harmless under btrfs`** --
   demote and reframe the existing correct material. Preserve, with intros
   making clear this covers the 512-sector case (test drives / the historical
   worry behind `--sector-size 4096`), NOT what the NAS uses:
   - the three-layers diagram (btrfs -> LUKS -> disk),
   - "Why --sector-size 4096 exists" (read-modify-write at the physical disk),
   - the dm-crypt walkthrough: dm-crypt encrypts a 4096-byte btrfs write in
     8x512-byte crypto sectors internally but allocates one clone bio for the
     whole write and submits it downstream as a single bio, so the disk sees a
     full-sector write and there is no read-modify-write penalty; overhead is
     CPU-only and negligible with AES-NI. **Back this with an inlined, captioned
     kernel excerpt** (not a `reference/` path -- see Citation method below).
     Drop-in:

     > dm-crypt does not split the write -- it allocates one clone bio for the
     > entire write and submits it downstream as a single bio:
     >
     > ```c
     > clone = crypt_alloc_buffer(io, io->base_bio->bi_iter.bi_size);
     > ```
     >
     > -- Linux 6.18.33, `drivers/md/dm-crypt.c` (`kcryptd_crypt_write_convert`)
   - "When --sector-size 4096 would matter" (ext4 1K blocks, raw dd, 512-byte
     DB writes -- btrfs is not one of them).

Drop the standalone `## Decision` section: its content (don't set
`--sector-size`) is now the thesis in Summary + "Why braid doesn't override
it."

## Citation method (reference/ is gitignored)

`reference/` is gitignored (`.gitignore` `/reference/`, confirmed via
`git check-ignore`): it holds shallow upstream fetches from
`just fetch-references`, absent from the repo for any reader who hasn't run it
and invisible to git history. So the shipped doc must **never** point at a
`reference/...#symbol` path -- that pointer is dead for most readers. For each
upstream claim the doc makes:

- Inline the decisive excerpt (one line is usually enough) as a
  fenced/blockquoted snippet.
- Caption it with tool + version, plus a tag/commit only where the source is a
  real git checkout. The kernel is pinned by release version: caption
  `Linux 6.18.33, drivers/md/dm-crypt.c (<function>)` -- the version is the
  immutable pin, no SHA needed. cryptsetup is pinned at `2.8.6`: caption
  `cryptsetup 2.8.6, lib/utils_device.c (device_optimal_encryption_sector_size)`.
- Use the function name as the in-file locator, never a line number -- e.g.
  `crypt_alloc_buffer(io, io->base_bio->bi_iter.bi_size)` appears twice in
  `dm-crypt.c` (lines 1909 and 2130); only `kcryptd_crypt_write_convert`
  disambiguates the write-path one.

In-tree `cli/src/...#symbol` code spans stay as-is -- those files are in git,
greppable, and mandated by AGENTS.md's File References rule. This
inline+caption rule is specifically for the gitignored `reference/` tree.

Versions verified for this plan: kernel **6.18.33** (`reference/linux/Makefile`),
cryptsetup **2.8.6** (`reference/cryptsetup/configure.ac`).

## Verification

- `mdbook build docs` succeeds and `mdbook-linkcheck2` passes (catches any
  bad cross-link if the optional replace.md anchor is used).
- Read-through check: no remaining sentence claims or implies braid uses
  512-byte LUKS sectors universally; "Our hardware" states 4096 for NAS and
  512 for test disks; the fixture note says 512 (not 4096).
- Grep guard (multiline, file-scoped): `rg -U "We use the default|irrelevant for
  braid" docs/internals/btrfs/luks-sector-size.md` returns nothing -- both
  load-bearing phrasings of the false thesis are gone. The plan's original
  line-based `rg -n "default 512-byte|sector_size.*512.*default" docs/` was
  vacuous: the false claim straddles a line break ("We use the default" / "512-byte
  LUKS sector size"), so `rg` matched nothing even against the current *wrong* doc
  and could not tell "rewrite landed" from "rewrite never happened." Do NOT guard
  on a generic `512-byte LUKS` pattern -- the reframed Aside legitimately still
  discusses 512-byte LUKS sectors (the test-drive / worst-case path), so that would
  false-positive on correct content. The read-through check is the real gate; this
  is just a functioning automated backstop.
- Citation guard: `rg -n "reference/" docs/internals/btrfs/luks-sector-size.md`
  returns nothing -- upstream sources are cited via inlined excerpt + caption
  (tool + version), never a gitignored `reference/` path.
- No code/test run needed (docs-only); behavior is unchanged.

## Implementation notes

- Kept both optional cross-links (the `replace.md` refusal-cases anchor and the
  `luks-unlock.md` replace-target preflight anchor). The plan flagged the
  `replace.md` link as optional ("if in doubt, keep only the code-span
  reference"); the exact mdbook slugs were verified against the rendered
  per-file pages (`safety-checks--refusal-cases`,
  `replace-target-size-preflight`) and then confirmed clean by
  `mdbook build docs` (linkcheck2 exit 0), so both links were kept rather than
  dropped.
- Cited only the man-page contract for "What auto-detect picks"; skipped the
  optional `device_optimal_encryption_sector_size` source-line inline. One
  authoritative citation for the auto-detect rule reads cleaner than two for the
  same fact.
- Quoted the fixture value verbatim as `"sector_size":512` (no space after the
  colon) to match the actual bytes in the committed fixtures.
