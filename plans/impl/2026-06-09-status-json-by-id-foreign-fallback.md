# Plan: document the foreign-device `by_id` fallback in `status --json`

## Context

`braid status --json` emits a `disks[]` array. For a **foreign live pool
device** -- a device live in the btrfs pool whose LUKS UUID is *not* in
`pool.json` membership -- the per-disk `by_id` field falls back to
`/dev/mapper/<observed-mapper>` instead of a `/dev/disk/by-id/...` path:

```rust
// cli/src/status.rs, present-device loop (~1076-1078)
let by_id = matched_member
    .map(|member| member.by_id.as_str().to_owned())
    .unwrap_or_else(|| format!("/dev/mapper/{}", pd.mapper.0));
```

The JSON field doc (`docs/commands/status.md`, the `by_id` bullet) describes
the field only as a "stable `/dev/disk/by-id/...` hardware path -- a runtime
handle, not identity," with no note about this fallback. Its sibling fields
already document their foreign / non-present exceptions: `luks_uuid` ("a
foreign live pool device carries an observed UUID that is **not** in
membership"), `name` ("for a foreign present device it falls back to the
mapper basename"), and `mapper` (present vs non-present). `by_id` is the lone
field whose foreign-device behavior is undocumented, so a `--json` consumer
reading `by_id` on a foreign present row gets a value whose shape contradicts
the doc.

**Direction chosen (of three considered): document the fallback.** The
mapper-path fallback is correct and intentional under braid's own framing:
`by_id` is explicitly "a runtime handle, not identity," and a `/dev/mapper/...`
path is a working handle. It is the twin of `name`, which braid already
decided (decision 024, `present_display_name` at `cli/src/status.rs#present_display_name`)
falls back to a mapper-derived handle for foreign rows -- so the consistent,
honest value here is the mapper path, and the only defect is that the doc
under-specifies the field's shape. (Rejected: making `by_id` nullable -- a
breaking schema change that discards a usable handle and breaks the
`name`/`by_id` symmetry; and resolving a real by-id from the underlying device
-- fallible, ambiguous across multiple by-id symlinks, and over-investing in a
row whose entire message is "run doctor.")

**Outcome:** the documented `by_id` contract matches code reality for every
row type, and a test pins the foreign-row value so code cannot silently drift
from the doc.

## Scope

Confined to two files. Confirmed by exploration:

- **README does not need syncing** -- it mentions by-id paths only in CLI
  examples, never the `--json` disks schema.
- **No include/snippet mechanism** -- the field list in `status.md` is the
  single source.
- **No existing test asserts `by_id` for a foreign live device** -- the row is
  built by `build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`
  but only its `name`/`status`/`luks_uuid` are asserted.

## Change 1 -- doc note (`docs/commands/status.md`, the `by_id` JSON field bullet)

Replace the current two-line bullet:

```markdown
  - `by_id`: stable `/dev/disk/by-id/...` hardware path -- a runtime
    handle, not identity.
```

with a three-case description paralleling the `name` field's structure and
reusing the `luks_uuid` field's "(paralleling its mapper-basename `name`)"
phrasing:

```markdown
  - `by_id`: stable `/dev/disk/by-id/...` hardware path -- a runtime
    handle, not identity. For a matched present member it is the member's
    recorded by-id path; for a non-present disk it is the configured by-id
    path; but a foreign present device has no membership join, so it falls
    back to `/dev/mapper/<observed-mapper>` (paralleling its mapper-basename
    `name`) -- the only row whose `by_id` is not a by-id path.
```

Notes:
- The "non-present disk -> configured by-id path" clause is verified at
  `cli/src/status.rs` (~1219 and ~1260): both unpooled branches set
  `by_id: cd.by_id_path` / `failure.by_id`, real by-id paths. This closes a
  second, smaller gap (non-present rows had no explicit `by_id` note either).
- Keep ASCII (`--`, plain backticks); match the file's ~70-col wrap.
- No change to the JSON example block or the diagnostic-row note block: the
  foreign row is a `present` row, and per-field notes are where the sibling
  fields document their foreign behavior -- adding it elsewhere would
  over-document.

## Change 2 -- pin the contract (`cli/src/status.rs`)

In `build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`
(~`status.rs:5452`), which already builds the foreign row (mapper
`braid-disk1`, observed UUID not in membership) and asserts `disks[0]`'s
`name`/`status`/`luks_uuid`, add one assertion completing that row's
fingerprint:

```rust
// Foreign live device has no membership join, so `by_id` falls back to the
// observed mapper path -- pins the JSON contract documented for the by_id
// field in docs/commands/status.md.
assert_eq!(ctx.disks[0].by_id, "/dev/mapper/braid-disk1");
```

This asserts another field of a row the test already inspects, so it does not
dilute the test's intent. (Alternative if a single-purpose test is preferred:
a dedicated `build_disk_reports_foreign_device_by_id_falls_back_to_mapper_path`
with its own Intent/Why/Scenario preamble -- more boilerplate, same coverage.)

## Verification

- `just test-rust` (or `cargo test -p <cli> build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`)
  -- the new assertion passes against current code, proving the doc now
  matches reality.
- `just docs-build` -- mdBook build + `mdbook-linkcheck2` confirm no broken
  links from the edited bullet.
- `git diff HEAD -- docs/commands/status.md cli/src/status.rs | LC_ALL=C rg -n '[^\x00-\x7F]'`
  -- expect no output. Both edits fall *outside* `check-output-ascii.py`'s
  surface (it scans `cli/src/**/*.rs` with `#[cfg(test)]` spans skipped and
  plain comments exempt, plus `modules/**/*.nix` echo lines -- never `docs/`),
  so a diff-scoped byte scan is what actually catches a stray non-ASCII char in
  the doc prose or the test comment. (`HEAD`, not a bare `git diff`, so it
  still fires once the edits are staged.)
- Manual cross-read: the four field bullets (`luks_uuid`, `name`, `by_id`,
  `mapper`) now each state their foreign / non-present behavior consistently.

## Implementation notes

- The plan cited the target test as `build_disk_reports_foreign_mapper_name_does_not_hide_missing_member`,
  but the repo's actual name is `build_disk_views_foreign_mapper_name_does_not_hide_missing_member`
  (`cli/src/status.rs`). The `build_disk_reports` -> `build_disk_views` rename
  landed with the "fold status compact drives into the single disk-views pass"
  refactor. The assertion was added to the correctly-named test; behavior and
  intent are unchanged.
