# Document `MountedExtras` as single-pass human-only render data

## Context

A code-review finding (Low/Simplicity) flagged that `MountedExtras.devid_names`
is built by `build_status` on every mounted run, including `--json`, where the
JSON path discards it. The proposed fix was to gate `build_devid_names` on
`!json` or rebuild it lazily inside `format_status_human`.

Investigation (`/verify-issue`) showed the headline fix is wrong:

- `devid_names` is one of **three** `MountedExtras` fields (`compact_drives`,
  `human_details`, `devid_names`), all built unconditionally in `build_status`
  and all dropped on the JSON path (`cmd_status` serializes only
  `built.report`). Singling out the cheapest field is misframed.
- This is deliberate. `build_disk_views` runs **one** classification pass whose
  three disk-status surfaces -- the JSON `StatusReport.disks` plus the human
  `human_details`/`compact_drives` in `MountedExtras` -- cannot disagree on a
  disk's status (decision 024, documented on `cli/src/status.rs#DiskViews`
  and `cli/src/status.rs#CompactDrive`). `devid_names` is a *separate*
  human-only map built in the same `build_status` pass from the live pool +
  membership (it labels alert causes, not disk status), so decision 024 does
  not govern it. All of `MountedExtras` is assembled for `--json` too and then
  dropped.
- The proposed lazy-rebuild isn't viable: `format_status_human` receives neither
  `pool` nor `membership`, which `build_devid_names(pool, membership)` requires;
  rebuilding there would duplicate name-resolution logic and *grow* the
  keep-in-sync cost the finding set out to cut. The cost it targets is a few
  `HashMap` inserts over a handful of drives on a command that already shells
  out to btrfs/cryptsetup/df.

**Root cause:** `MountedExtras` and `BuiltStatus` are the only structs in the
"Status assembly" block lacking the boundary `///` their projection-family
siblings carry. That missing rationale is what made the unconditional build read
as accidental waste. The fix is documentation, not a perf gate.

## Change

Add `///` doc comments to two module-private structs in
[`cli/src/status.rs`](../../cli/src/status.rs), matching the house style
([doc-comments.md](../../docs/dev/doc-comments.md): justify why it exists at the
boundary; capture call-site coupling and invariant, not the signature) and the
length norm already set by the sibling `DiskViews`/`CompactDrive`/
`build_devid_names` comments.

### 1. `MountedExtras` (`cli/src/status.rs#MountedExtras`) -- the load-bearing fix

```rust
/// Human-only render data assembled during one `build_status` pass and dropped
/// on the JSON path (`cmd_status` serializes only `report`): `compact_drives`
/// and `human_details` are the disk-view projections from `build_disk_views`,
/// kept status-consistent with the JSON `disks` by that single classifier
/// (decision 024); `devid_names` is the alert-cause devid->name map built here
/// from the same live pool + membership, feeding the human alert banner. All
/// three are built even for `--json`, by design -- not a cache to gate on
/// `!json`.
struct MountedExtras {
```

The clause "All three are built even for `--json`, by design -- not a cache to
gate on `!json`" is what preempts re-filing this finding, and it covers all three
fields rather than just `devid_names`. The comment attributes each field to its
source -- `compact_drives`/`human_details` to `build_disk_views` (where the
decision-024 disk-status-consistency guarantee actually lives) and `devid_names`
to its separate `build_devid_names` assembly, which is *not* a disk-status
projection and is not governed by that guarantee -- but defers per-field
internals to the existing docs (`cli/src/status.rs#build_devid_names`,
`cli/src/status.rs#CompactDrive`, `cli/src/status.rs#DiskViews`) rather than
restating them.

### 2. `BuiltStatus` (`cli/src/status.rs#BuiltStatus`) -- companion, same block

```rust
/// One `build_status` pass split into the two surfaces `cmd_status` renders:
/// the always-built `report` (JSON, and the spine of human output) and the
/// human-only `mounted_extras`, `None` exactly when the pool is not mounted
/// (the human-only render data needs a live pool; see `not_mounted_status`).
struct BuiltStatus {
```

## Explicitly out of scope

- **No** `!json` gate on `build_devid_names`, and **no** lazy rebuild in
  `format_status_human` -- rejected above.
- No behavior change, no new code, no test changes.
- `HumanDisk` (`cli/src/status.rs#HumanDisk`) is left as-is: it's a render-row
  shape already covered by field-level comments and the `DiskViews` doc, not
  part of the JSON-vs-human dispatch confusion this finding is about.

## Notes

- Both structs are module-private (no `pub`), so they are exempt from the
  CI-enforced doc rule (`scripts/docs/check-cmd-doc-comments.py` covers only
  `cmd_*`). Adding the comments is a deliberate clarity/salvage change, not a
  compliance fix.
- Doc comments are exempt from `check-output-ascii.py`; the drafts use ASCII
  `--` anyway, matching the file.

## Verification

Comment-only change, so verification is "still compiles cleanly, nothing
regressed" -- there is no behavior to exercise:

- `just test-rust` (or `cargo build && cargo test` in `cli/`) -- confirms the
  doc comments are well-formed and the crate builds.
- `cargo fmt --check` and `cargo clippy` in `cli/` -- confirm formatting/lints
  stay clean.
