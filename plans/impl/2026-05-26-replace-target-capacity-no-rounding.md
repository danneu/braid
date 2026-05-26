# Plan: document the no-rounding invariant in replace target-capacity preflight

## Context

A code-review finding (Low / Correctness) claimed `mapper_capacity_from_dynamic_segment`
(`cli/src/preflight.rs:514`) is optimistic because it computes dynamic-segment mapper
capacity as `raw - offset` without rounding down to the LUKS2 segment `sector_size`, and
proposed rounding down before comparing against the source btrfs `total_bytes`.

Investigation showed the finding's premise is **false** and the proposed code change must
**not** be made:

- cryptsetup sizes the dynamic dm-crypt device as `real_size - data_offset` in 512-byte
  sectors with **no** rounding to the segment `sector_size` (`device_block_adjust` in
  `reference/cryptsetup/lib/utils_device.c`; the value flows straight into
  `dm_create_device` via `reference/cryptsetup/lib/setup.c`).
- The kernel dm-crypt constructor *rejects* activation (`-EINVAL`, "Device size is not
  multiple of sector_size feature") when the length is not a `sector_size` multiple -- it
  does **not** round down (`crypt_ctr` in `reference/linux/drivers/md/dm-crypt.c`).
- Therefore `bdev_nr_bytes(mapper)` equals `raw - offset` to the byte whenever the open
  succeeds -- exactly what braid computes and exactly what `btrfs replace start` compares
  (`reference/linux/fs/btrfs/dev-replace.c:285`). There is **zero optimism gap**.
- The only case where rounding would change the number (sector_size > 512 and
  `raw - offset` not a multiple) is exactly the case where `cryptsetup open` fails at the
  kernel *before* `btrfs replace start` ever runs. Rounding in preflight would not predict
  it and would mislabel an alignment failure as "too small".
- In practice the 16 MiB offset is a multiple of 4096 and disks are whole-MiB, so the
  proposed rounding is a literal no-op. braid already guards alignment by rejecting
  `--sector-size` and offset-changing luksFormat flags (`cli/src/types.rs:310,313`),
  keeping the default offset invariant.

The real problem is that this invariant ("`raw - offset` is exact; no `sector_size`
rounding is needed") is implicit, so reviewers re-derive it incorrectly. Separately,
`mapper_capacity_from_dynamic_segment` currently lacks the `///` doc comment that
`AGENTS.md` requires for new top-level functions. The ideal update is **documentation plus
a regression guard, not a production-code change**: make the invariant explicit so this
class of finding stops recurring, satisfy the doc-comment rule, and pin the no-rounding
behavior with a focused unit test so it cannot be silently undone.

## Change

### 1. Add a `///` doc comment to `mapper_capacity_from_dynamic_segment` (`cli/src/preflight.rs:514`)

Capture the no-rounding invariant (the one fact a reader cannot recover from the code).
The helper is shared by both callers, so the wording must keep the two safety arguments
separate (see finding F1): the no-rounding exactness comes from cryptsetup/dm-crypt
sizing-or-rejecting the mapper and holds for an *existing* container at any `sector_size`
(braid did not format it); braid's `--sector-size`/offset rejection only justifies the
*fresh*-target default-offset assumption. Draft wording:

```rust
/// Mapper capacity btrfs compares against the source `total_bytes`, computed as
/// `raw - offset` with no sector_size rounding: cryptsetup sizes the dm-crypt device
/// that way in 512B sectors exactly (`device_block_adjust`), and dm-crypt rejects --
/// never rounds -- a mapper whose length is not a sector_size multiple (`crypt_ctr`),
/// so an existing container is exact at any `sector_size`. The offset is the caller's:
/// existing LUKS targets pass the real luksDump segment offset; fresh targets pass the
/// default 16 MiB, which holds because braid rejects offset/sector-size format flags.
```

Constraints:
- ASCII only (`--`, never an em-dash) per the writing-style and CLI-output rules.
- Cite reference behavior by **function name** (`device_block_adjust`, `crypt_ctr`), not
  line number -- `reference/` is refreshed by `just fetch-references` and line numbers
  drift.
- ~6-7 lines is acceptable here despite the "prefer 1-3 lines" guidance, because the
  invariant is external-tool behavior that cannot be recovered from the code and the two
  callers' offset sources must be distinguished (F1).

### 2. Add one clause to the internals doc (`docs/internals/luks-unlock.md`, ~line 210)

The "Replace Target Size Preflight" section already says `dynamic` segments use
`raw - offset`. Extend it to state why no rounding is required, keeping the two arguments
split (F1) -- e.g.: "... with no sector_size rounding: cryptsetup sizes the dm-crypt
device that way exactly and the kernel rejects (rather than rounds) a non-sector_size-
multiple mapper, so an existing container's capacity is exact at any sector_size. Fresh
targets instead assume the default 16 MiB offset, which holds because braid rejects
`--sector-size` and offset-changing format flags."

### 3. Add a no-rounding regression unit test (`cli/src/preflight.rs` tests module)

The existing `check_replace_target_capacity_existing_dynamic_segment` (`preflight.rs:816`)
uses 4096-aligned whole-MiB values, so it would still pass if someone reintroduced
sector_size rounding -- the very change this plan argues against. Add a test that pins the
invariant through the public accept/reject observable, with no dependency on the private
helper (F2):

- Target: existing dynamic LUKS, luksDump reporting `sector_size: 4096`, segment offset
  16 MiB. Extend the `luks_dump_json` helper (`preflight.rs:740`) to take a `sector_size`
  (pass 4096 in the new test; 512 for the existing callers) so the fixture models the
  realistic externally-formatted 4Kn target from F1.
- Sizes: `raw = 16 MiB + 4608`, source `total_bytes = 4608`. 4608 is not a multiple of
  1024, 2048, or 4096, so any round-down of `raw - offset` to the segment sector_size
  drops it below 4608 and would flip accept -> refuse.
- Assert `check_replace_target_capacity(.. PresentLuks { by_id: TARGET })` returns `Ok`.

Reuse `dev_info_with_total` and `runner_with_target_size_and_luks_dump`. Add the
Intent / Why it exists / Scenario preamble per Test Conventions, naming the rounding
regression it guards and why whole-MiB VM/`existing_dynamic_segment` fixtures cannot.

## Explicitly NOT doing

- **Not** implementing the proposed `sector_size` rounding. It is a no-op in every
  realistic case and would refuse for the wrong reason in the misaligned case (which fails
  at luksOpen anyway).
- **Not** extending the parser to deserialize `sector_size`. It is unused; the finding's
  "already available" claim refers to the raw JSON, but `RawSegment`
  (`cli/src/parse/cryptsetup_luks_dump.rs:39-44`) deliberately drops it.
- **Not** changing the VM test `tests/cli/replace-rejects-smaller-target.{nix,py}`. It
  still covers the realistic capacity-refusal path, but its whole-MiB sizes are
  4096-aligned and cannot catch a rounding regression -- that gap is why change item 3
  adds a focused unit test rather than touching the VM test.

## Verification

- `just test-rust` -- builds the crate with the new doc comment and runs the new
  no-rounding unit test (change item 3). To confirm the test actually pins the invariant,
  temporarily round `raw - offset` down to 4096 in `mapper_capacity_from_dynamic_segment`
  and verify the new test fails; revert. No parser tests should regress.
- `mdbook build docs` -- validates the internals-doc edit (cross-link check per `AGENTS.md`
  / `docs/book.toml`); the added clause introduces no new links.
- Re-read both edits to confirm they state the invariant in ASCII and cite cryptsetup /
  kernel behavior by function name.
