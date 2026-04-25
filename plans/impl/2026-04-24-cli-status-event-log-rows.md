# Plan: Reshape braid CLI status rows to event-log style

## Context

`braid lock` / `braid unlock` (and the shared preview/probe renderers
they share with other commands) emit per-item status rows pretending
to be a table:

```
[ok  ]  disk: example-disk-aaafound
[ok  ]  disk: example-disk-aaaunlocked
```

The format strings use fixed-width column padding (`{:<7}`, `{:<10}`,
`{:<14}`) for the disk-name column. Disk names are validated up to
**32 characters** (`cli/src/membership.rs:143-160`), so any name longer
than the column width slams into the action with no whitespace. This
is unreadable and looks broken.

The fix: stop pretending it's a table. Treat these rows as event-log
lines with a fixed-width bracketed status tag (so they're scannable in
a terminal) and a free-form, delimiter-safe body. Disk names are
already constrained to `[A-Za-z][A-Za-z0-9_-]*`, so a literal `:` is a
safe delimiter between name and action.

Target shape (every prefix is 7 visible columns wide):

```
[ok]   pool: unmounted /mnt/storage
[warn] disk example-disk-aaa: locked
[fail] disk example-disk-bbb: locked

[ok]   disk example-disk-aaa: found
[ok]   disk example-disk-bbb: found
[ok]   disk example-disk-aaa: unlocked
[ok]   pool: mounted /mnt/storage
```

## Files to modify

- `cli/src/status_tag.rs` -- helper changes
- `cli/src/lock.rs` -- 9 status rows + retry warn at `:74-77` +
  free-form `[tag] body` lines
- `cli/src/mount.rs` -- 3 status rows + `render_probe_events` test
- `cli/src/preview.rs` -- shared `format_per_disk_line` (Bracketed arm),
  warn renderer, docstrings, 4 unit-test expectations
- `cli/src/doctor.rs` -- 1 status row + 2 unit-test expectations
- Rust tests in `cli/src/{unlock,recover,enroll_key_file,add,replace,remove,remove_missing}.rs`
  that pin the old `[ok  ]` / `[warn]` byte sequences
- VM/integration tests under `tests/cli/`: `braid-unlock.py`,
  `braid-recover.py`, `braid-enroll-generate.py`, `braid-enroll.py`,
  `braid-add-enroll.py`, `braid-add-warnings.py`,
  `replace-preview-warnings.py`, `braid-remove-softwarn.py`,
  `braid-remove-missing-softwarn.py`, plus `braid-lock.py`
  (regression strengthening, see Step 5)
- Manual docs: `manual/commands/doctor.md:22-29` (sample output block
  pins `[ok  ]  <label:14>...` for the doctor command), plus
  committed `manual/book/**` generated output if this repo tracks it

## Step 1: helper API in `status_tag.rs`

Change `render_status_tag(tag, color_enabled)` to return the *bare*
bracketed tag -- no internal padding -- for both color variants:
- `Ok` -> `"[ok]"` / `"\x1b[32m[ok]\x1b[0m"`
- `Warn` -> `"[warn]"` / `"\x1b[33m[warn]\x1b[0m"`
- `Fail` -> `"[fail]"` / `"\x1b[31m[fail]\x1b[0m"`
- `Skip` -> `"[skip]"` / `"\x1b[90m[skip]\x1b[0m"`

Add one public helper plus a private padding lookup:

```rust
fn status_tag_pad(tag: StatusTag) -> &'static str {
    match tag {
        StatusTag::Ok => "   ",          // 3 spaces -> 4+3 = 7 cols
        StatusTag::Warn | StatusTag::Fail | StatusTag::Skip => " ", // 1 space -> 6+1 = 7 cols
    }
}

pub fn status_line(tag: StatusTag, color_enabled: bool, body: &str) -> String {
    format!(
        "{}{}{body}\n",
        render_status_tag(tag, color_enabled),
        status_tag_pad(tag),
    )
}
```

`status_line` is the **sole public writer** of the 7-column prefix
contract. `status_tag_pad` stays private so the padding split is an
implementation detail; tests pin `status_line`'s output, not the
internal table. Padding is computed from the enum, not from the
rendered string, so ANSI bytes never pollute width math. Update the
doc comment at `status_tag.rs:3-10` to describe a "7-column visible
prefix" produced by `status_line`, and note that `render_status_tag`
returns the bare tag.

## Step 2: rewrite callsites

### Convention

- Disk subject: `disk <name>: <action> [<details>]`
- Pool subject: `pool: <action> [<details>]`
- Free-form (no subject/name): just `<body>` -- still routed through
  `status_line` so the prefix stays uniform.

### `cli/src/lock.rs`

Replace the 8 padded `disk: {:<7}...` rows, the padded
`{:<14}"pool"` row, and the `close_mapper_with_retry` warn at
`:74-77` (which calls `render_status_tag` directly with a hardcoded
two-space separator) with `status_line` calls. After this migration
no callsite in `lock.rs` should reach for `render_status_tag` --
`status_line` owns the prefix.

| Line | Before | After (body argument) |
|------|--------|------------------------|
| 74-77 | `render_status_tag(Warn, color_enabled)` + `"  "` + retry message | `status_line(Warn, color_enabled, &format!("cryptsetup close {mapper} busy, retrying ({attempt}/{CLOSE_RETRY_ATTEMPTS})..."))` |
| 240  | `eprintln!("{}  {body}", tag(Warn))` | body unchanged (free-form) |
| 263  | `eprintln!("{}  {err}", tag(Fail))` | body = `format!("{err}")` (free-form) |
| 266  | follow-up warn | free-form, unchanged |
| 270-275 | `"{}  {:<14}unmounted {}"` + `"pool"` | `format!("pool: unmounted {mount_point}")` |
| 290-301 | forget-failure warns | free-form, unchanged |
| 317  | `disk: {:<7}locked` | `format!("disk {name}: locked")` |
| 321-325 | `disk: {:<7}close failed (umount was stuck): {msg}` | `format!("disk {name}: close failed (umount was stuck): {msg}")` |
| 328  | `disk: {:<7}{e}` | `format!("disk {name}: {e}")` |
| 336  | `disk: {:<7}already closed` | `format!("disk {name}: already closed")` |
| 351-353 | orphan-mapper warn | free-form, unchanged |
| 357-360 | `disk: {:<7}locked (orphan)` | `format!("disk {disk_name}: locked (orphan)")` |
| 364-367 | `disk: {:<7}orphan close failed (umount was stuck): {msg}` | same with `disk {disk_name}:` prefix |
| 372-375 | `disk: {:<7}orphan: {e}` | `format!("disk {disk_name}: orphan: {e}")` |

The local `let tag = |t| render_status_tag(t, color_enabled);` closure
at `lock.rs:233` becomes `let line = |t, body| status_line(t, color_enabled, body);`.

### `cli/src/mount.rs`

| Line | After |
|------|-------|
| 563-567 | `status_line(Ok, color_enabled, &format!("disk {name}: unlocked"))` |
| 702-706 | same |
| 761-766 | `status_line(Ok, color_enabled, &format!("pool: mounted {}", mount_point))` (drops the `{:<10}"pool"` literal) |

### `cli/src/preview.rs`

`format_per_disk_line` (`preview.rs:95-115`) is the shared renderer
used by `Preview::render`, `render_per_disk_notes`, and
`render_notes_for_stderr`. Rewrite the `Bracketed` arm:

```rust
PerDiskStyle::Bracketed => status_line(
    level.to_status_tag(),
    color_enabled,
    &format!("disk {name}: {message}"),
),
```

Leave `Plain` (`enroll`'s `skip: <name> ...` shape) untouched.

Route the Warn handlers at `preview.rs:170-175` and `:219-224`
through `status_line` too, so all `[warn] <body>` lines come from a
single helper.

Update the docstrings:
- `preview.rs:65-68` -- example currently shows `[ok  ]  disk: <name>    <msg>`.
  Replace with the new shape.
- `preview.rs:150, 199` -- mention the new `[warn] <body>` width.

### `cli/src/doctor.rs:923-928`

Build the body inline (labels are pre-mapped, max 14 chars; the
mini-table inside the body still aligns under the new prefix):

```rust
out.push_str(&status_line(
    tag,
    color_enabled,
    &format!("{label:<14}  {message}", label = label, message = c.message),
));
```

This keeps doctor's table-like inner layout but normalizes the prefix
to 7 cols.

## Step 3: migration order

Land as **one atomic PR**. Inside that PR:

1. Update `status_tag.rs` (new helpers, shorten `render_status_tag`,
   update docstring).
2. Update producer callsites (`lock.rs`, `mount.rs`, `preview.rs`,
   `doctor.rs`).
3. Update tests/snapshots in the same commit.

Splitting risks a compile-green-but-visually-broken intermediate state:
the moment `render_status_tag` shortens, `format!("{tag}  disk: {name:<7}...")`
silently produces `[ok] disk: name...` (one space). One red-green
cycle is safer than two staged ones.

## Step 4: tests, docs, and integration scripts to update

Authoritative grep before starting:
`rg "\[ok  \]|\[skip\]  disk:|\[warn\]  |\[fail\]  "` from the
`braid` repo root. Rewrite each hit to the new shape. Today's hits:

### Rust unit/integration tests in `cli/src/`

- `cli/src/status_tag.rs:80-112` -- 4 plain + 4 colored pins:
  `[ok  ]` -> `[ok]`, others unchanged structurally.
- `cli/src/preview.rs:415, 469, 474, 500-505, 527-532` --
  multi-line expected strings. `:469` is Plain-style and stays as-is.
- `cli/src/mount.rs:1889-1896` (and any sibling per-variant
  `to_preview_note` tests) -- `render_probe_events` snapshot.
- `cli/src/doctor.rs:1156, 1176-1177, 1216-1219` -- substring asserts
  and the colored expected block.
- `cli/src/unlock.rs:975-976` -- `"[ok]   disk disk1: found\n"`.
- `cli/src/recover.rs:3684, 3687, 3690` -- substring finds become
  `"[ok]   disk disk1"` etc.
- `cli/src/enroll_key_file.rs:997` --
  `contains("[skip] disk disk1: not present\n")`.
- `cli/src/add.rs:3623, 3668, 3671, 3677` -- `[warn] ...` (single
  space) and the per-disk line.
- `cli/src/replace.rs:1113, 1119, 3879, 3885, 3938` -- `[warn] ...`.
- `cli/src/remove.rs:2280, 2313` and
  `cli/src/remove_missing.rs:1696, 1734` -- `[warn] ...`.

### VM/integration tests under `tests/cli/` (`pytest`-style scripts run
under `nixos-test`; these go stale silently from the Rust suite's
perspective)

- `tests/cli/braid-unlock.py:76, 231, 242, 327, 373` -- `[ok  ]  disk: <name>`
  and `[skip]  disk: disk3` markers.
- `tests/cli/braid-recover.py:153, 157, 165, 205, 225` -- same shape.
- `tests/cli/braid-enroll-generate.py:184-189` -- includes a code
  comment that documents the `disk: <name:<10>>` shape; rewrite the
  comment too.
- `tests/cli/braid-enroll.py:308` -- `[skip]  disk: disk3` marker.
- `tests/cli/braid-add-enroll.py:100, 134, 188, 203, 221, 229` --
  `[warn]  ...` markers and prose comments.
- `tests/cli/braid-add-warnings.py:47, 50, 61, 62, 79, 100, 105, 120, 173, 179`
  and `tests/cli/braid-add-warnings.nix:11` -- `[warn]  ...`.
- `tests/cli/replace-preview-warnings.py:158, 191, 242, 285, 317` --
  `[warn]  ...`.
- `tests/cli/braid-remove-softwarn.py:5, 7, 13, 121, 132, 143, 144` and
  `tests/cli/braid-remove-missing-softwarn.py:5, 7, 12, 141, 152, 164, 165`
  plus their sibling `.nix` headers -- `[warn]  ...`.

### Manual docs

- `manual/commands/doctor.md:22-29` -- the doctor sample output block.
  Rewrite each line from `[ok  ]  <label:<14>>  <message>` to
  `[ok]   <label:<14>>  <message>`. The inner label/message column
  stays as-is; only the prefix changes. Regenerate/update committed
  `manual/book/**` HTML if those generated files are tracked.

### Things to drop, not rewrite

- The "structure-sensitive padding test" (`status_tag_pad_widths`) is
  intentionally **not** added (see Step 5). The padding split is an
  implementation detail of `status_line`.

## Step 5: new behavioral tests

### Rust (`cli/src/status_tag.rs`)

All three tests are structure-insensitive and pin behavior, not
bytes:

1. **`status_line_prefix_is_seven_visible_columns`** (in
   `cli/src/status_tag.rs`) -- for each `StatusTag`, render
   `status_line(tag, false, "x")`, then assert
   `result.find('x') == Some(7)` (body byte sits at index 7, i.e. the
   8th character) and `&result[..7]` is the visible prefix. For the
   colored case, ANSI-strip first and make the same assertion. This
   owns the prefix-width invariant.

2. **`status_line_passes_body_through_unchanged`** (in
   `cli/src/status_tag.rs`) -- pin that `status_line(Ok, false, "hello")`
   ends with `"hello\n"`. Cheap, catches any regression where
   `status_line` starts mangling its body argument.

3. **`format_per_disk_line_long_name_keeps_action_separated`** (in
   `cli/src/preview.rs`) -- this is the real regression test for the
   reported bug. Render a `PreviewNote::PerDisk { name: "<30-char
   name>", level: NoteLevel::Ok, message: "locked" }` through
   `render_per_disk_notes(&[note], PerDiskStyle::Bracketed)`. Assert
   the output contains `format!("disk {long_name}: locked")` (the full
   subject + colon + space + action substring). This catches drift in
   the shared `format_per_disk_line` renderer and any other callsite
   that runs through it -- exactly the place where the pre-fix
   `disk: {name:<10}{message}` shape lived. A pure
   `status_line(...)`-targeted test wouldn't catch a reversion of the
   body template.

The existing `colored_status_tags_strip_to_plain_tags`
(`status_tag.rs:122-134`) keeps working unchanged -- it's a
structural equivalence between color and plain renderings.

### VM regression in `tests/cli/braid-lock.py`

The Rust suite covers shared preview/mount/doctor renderers but not
`LockPlan::execute`'s rows directly, so reverting many `lock.rs`
callsites could pass the proposed Rust unit tests. Strengthen the
existing `braid-lock.py` (which already asserts only that the
substring `"unmounted /mnt/storage"` appears at `:63`) to pin exact
live stderr rows from a real `braid lock` run:

- `"[ok]   pool: unmounted /mnt/storage"` substring on a clean line.
- At least one `"[ok]   disk <name>: locked"` row -- ideally with a
  disk whose name exceeds 7 chars to also exercise the long-name
  case in the live path.

Keep the existing `"\x1b[" not in live_stderr` assertion -- the test
runs without a TTY, so plain output is required.

## Step 6: scope / non-goals

- **JSON output is out of scope.** This change reshapes human status-line
  rendering only. `doctor --json`, `status --json`, and any serialized
  preview data are not intentionally changed. Note: `PreviewNote` *does*
  derive `Serialize` (`preview.rs:44`), but no current command serializes
  `Preview` to a user; this change only edits the human renderers and
  leaves the `PreviewNote` data shape unchanged.

- **No legacy compatibility shim.** Old human output shapes are replaced
  wherever they are pinned in tests or docs; do not preserve duplicate
  render paths for the old table-like format.

- **Dry-run risk tag** (`cmd.rs:266` -- `[{:<11}] {}` for `safe` /
  `destructive`) is a **separate contract** documented in
  `status_tag.rs:9-10`. Leave alone. Keep the "distinct from" note in
  the docstring.

- **In-repo grep for `[ok  ]` / `[skip]  disk:` / `[warn]  ` / `[fail]  `**:
  hits live under `cli/src/`, `tests/cli/`, and `manual/commands/`.
  The repo-wide list is enumerated in Step 4. Re-run the same grep
  after the rewrite -- the only remaining hits should be inside this
  plan file and any historical `plans/impl/*.md` records (safe to
  leave; they describe past state).

- Commit body should call out the user-visible stderr change. No
  CHANGELOG file in the repo today; flag in PR description.

## Verification

1. `cargo test -p braid-cli` passes after the test rewrites.
2. New `format_per_disk_line_long_name_keeps_action_separated`
   (`cli/src/preview.rs`) fails on `master` and passes on the branch
   -- this is the regression test for the reported bug. The
   `status_line_prefix_is_seven_visible_columns` helper invariant test
   in `cli/src/status_tag.rs` should also pass on the branch.
3. VM tests under `tests/cli/` pass via the project's runner
   (`just test-vm braid-lock`, `just test-vm braid-unlock`,
   `just test-vm braid-recover`, `just test-vm braid-enroll-generate`,
   `just test-vm braid-enroll`, `just test-vm braid-add-enroll`,
   `just test-vm braid-add-warnings`, `just test-vm replace-preview-warnings`,
   `just test-vm braid-remove-softwarn`, `just test-vm braid-remove-missing-softwarn`).
   The strengthened `braid-lock.py` (Step 5) is the live regression
   for the original bug.
4. Manual smoke (in a NixOS VM or against a test pool):
   - `braid lock` with at least one disk whose name exceeds 7 chars
     (e.g. 20+ chars). Confirm the row reads
     `[ok]   disk <name>: locked` with a real space + colon between
     name and action.
   - `braid unlock` with the same disk; confirm the
     `[ok]   disk <name>: found` and `[ok]   disk <name>: unlocked`
     rows look like the target shape.
   - `braid doctor` (no --json); confirm the prefix is `[ok]   ` /
     `[warn] ` / `[fail] ` / `[skip] ` and the inner label column is
     still aligned. Compare against the updated
     `manual/commands/doctor.md` sample.
5. Manual docs build/check if committed `manual/book/**` output is
   tracked and regenerated.
6. `cargo fmt --check` and `cargo clippy -p braid-cli -- -D warnings`.
7. Final repo-wide
   `rg "\[ok  \]|\[skip\]  disk:|\[warn\]  |\[fail\]  "` returns
   only historical `plans/impl/*` matches and this plan file.
