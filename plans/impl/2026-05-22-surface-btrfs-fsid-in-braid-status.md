# Surface btrfs FSID in `braid status`

## Context

`manual/guides/mounting-subvolumes.md` (currently only on
`worktree-subvol-mount-lifecycle`, commit `bc8ee8c`) tells users to run
`sudo btrfs filesystem show /mnt/storage` and copy the `uuid:` line for
systemd-mount snippets like `what = "/dev/disk/by-uuid/<btrfs-fs-uuid>";`.
The guide notes -- twice -- that `braid status` does not yet show this
UUID, and a "Future enhancement" block at the bottom of the guide
explicitly flags this as the cleanup task.

`PoolState.fsid` (`cli/src/types.rs:423`) is already populated reliably
on the mounted path (see audit below). The work is render wiring: pipe
the existing field through `StatusReport` into both the human formatter
and the JSON serializer, then update the guide so users get the UUID
from a single `braid status` invocation.

## Branch strategy

The guide lives only on `worktree-subvol-mount-lifecycle`, and master is
otherwise busy. To keep the code change, tests, and guide update
shippable as one logical feature, do **all** the work in the
`subvol-mount-lifecycle` worktree (or a fresh worktree branched off
`worktree-subvol-mount-lifecycle`) -- not on master. Concretely:

- Work in `/Users/dan/Code/braid/.claude/worktrees/subvol-mount-lifecycle`
  (or `git worktree add` a new branch based on that one).
- Land code + tests + guide edits in commits on that branch.
- When `worktree-subvol-mount-lifecycle` eventually merges to master,
  this feature merges with it.

Do not split the change across master and the worktree; that would risk
the guide referencing a `braid status` field that does not yet exist on
master.

## Audit: is there a simpler approach?

I considered three alternatives and rejected each:

1. **Dedicated `braid uuid` / `braid pool-id` command.** New command
   surface, new tests, new help text -- strictly more work for one value
   that already wants to live next to `Pool:` and `Status:`.
2. **Wrapper struct (`PoolIdentity { mount, fsid, ... }`) on
   StatusReport.** Premature abstraction -- there is one identity field
   today. Add the wrapper if/when a second pool-level identifier arrives.
3. **Pretty-printing the UUID (hyphens normalized, lowercased, etc.).**
   `parse_btrfs_filesystem_show` already returns the canonical lowercase
   hyphenated form from btrfs-progs; nothing to do.

Recommended approach: add `fsid: Option<String>` to `StatusReport`,
populate from `pool.fsid.clone()`, render conditionally. This is the
minimum viable change and matches every other optional pool-level field
already on `StatusReport` (`profile`, `capacity`, etc.).

## FSID population audit (no live-output tests needed)

`PoolState.fsid` flows from `probe_pool()` in `cli/src/probe.rs:384-497`:

- **Unmounted** (`probe.rs:390-399`): explicit `fsid: None`.
- **Mounted / degraded** (`probe.rs:410-421, 488-496`): parsed from
  `btrfs filesystem show` via `parse_btrfs_filesystem_show`
  (`cli/src/parse/btrfs_filesystem_show.rs:131`). The probe treats a
  missing UUID as a hard `ProbeError::PoolDevice`, not as silent `None`
  -- so any non-error `PoolState { mounted: true, .. }` already has
  `fsid = Some(_)`.
- **Error states**: propagated as `ProbeError`, never as a fabricated
  `PoolState`. Status command's existing error handling covers these.

This is already golden-tested at the parser layer
(`cli/tests/fixtures/nixos-25.11/btrfs/` + the unstable mirror), and the
CLI parser canary (`just test-parsers`) exercises it on live VM output.
**No new VM/parser test is required** to prove population -- the
existing canary contract is exactly what would catch a regression here.

## Implementation

All Rust changes live in **`cli/src/status.rs`** and **`cli/src/probe.rs`**
is unchanged.

### 1. Add field to `StatusReport` (`cli/src/status.rs:47-82`)

Insert after `profile`, before `capacity`:

```rust
#[serde(skip_serializing_if = "Option::is_none")]
pub fsid: Option<String>,
```

JSON shape: `"fsid": "12345678-1234-1234-1234-123456789012"` when
mounted; key absent when not mounted. snake_case matches the rest of the
struct.

### 2. Wire data through both `StatusReport` construction sites

- **Mounted path** (`cli/src/status.rs:509`): add `fsid: pool.fsid.clone(),`
  next to `profile`.
- **Not-mounted path** (`cli/src/status.rs:360-379`, function
  `not_mounted_status`): add `fsid: None,`.

### 3. Update existing in-file test fixtures

There are ~20 `let report = StatusReport { ... };` literals inside the
`#[cfg(test)] mod tests` block of `cli/src/status.rs` (greppable: see
lines 1320, 1423, 1474, 1553, 1619, 1645, 1674, 1723, 1749, 1814, 1888,
1941, 1994, 2040, 2093, 2156, 2212, ...). Default each to `fsid: None`
to keep them compiling; only the "healthy mounted" cases below get a
real value.

### 4. Render in human output (`cli/src/status.rs:1046-1056`)

Today:

```rust
out.push_str(&format!("Pool:     {}\n", report.mount_point));
out.push_str(&format!("Status:   {}\n", ...));

if report.status == StatusCode::NotMounted {
    return out;
}
```

Insert one block between the `Status:` line and the `NotMounted` early
return:

```rust
if let Some(fsid) = report.fsid.as_deref() {
    out.push_str(&format!("FSID:     {fsid}\n"));
}
```

Placement next to `Pool:`/`Status:` keeps all pool-identity fields
together. The `if let` (rather than `is_some()`-then-unwrap) is the
existing style in this file (see the `balance`, `allocation`, etc.
blocks just below). When the pool is not mounted, `fsid` will be
`None` and the line is omitted; the early-return path is unaffected.
Spacing of `"FSID:     "` uses the same column width as `"Pool:     "`
and `"Status:   "` so values align.

### 5. Tests

**Rust unit tests in `cli/src/status.rs`:**

- Extend `status_human_healthy_raid1` (~line 1812): set
  `fsid: Some("12345678-1234-1234-1234-123456789012".into())` and
  assert the rendered output contains `"FSID:     12345678-..."`. The
  test already snapshots the full human block, so the assertion is a
  natural fit.
- Extend `status_human_healthy_single` (~line 1747) the same way -- one
  more proof the line renders next to `Status:`.
- Extend `status_human_not_mounted` (~line 1721): leave `fsid: None` and
  assert `"FSID:"` does **not** appear in the output. This pins the
  "mounted only" contract.
- Extend `status_json_healthy` (~line 1411): include `"fsid": "..."` in
  the expected JSON.
- Extend `status_json_not_mounted` (~line 1313): assert the serialized
  JSON does **not** contain the key `fsid` (proves
  `skip_serializing_if` works).

These are structure-insensitive behavioral tests -- they assert what an
operator or downstream JSON consumer would see, not internal call
ordering.

**Integration / VM tests:**

- Extend `tests/cli/braid-status.py` (lines 45-102): after the existing
  pool setup, parse `braid status --json` and assert
  `"fsid"` is present and matches `/^[0-9a-f-]{36}$/`. For the human
  output capture in the same test, assert the line starts with
  `FSID:` and contains a UUID. One small addition, no new test file.
- **No new VM test** for population: the parser canary
  (`just test-parsers`) already exercises `parse_btrfs_filesystem_show`
  against live `btrfs filesystem show` output. Duplicating that here
  would be redundant.

### 6. Manual updates (same branch as the code change)

Per the branch strategy above, all of these edits land in the
`subvol-mount-lifecycle` worktree alongside the Rust changes.

**6a. Replace the `btrfs filesystem show` block**

`manual/guides/mounting-subvolumes.md` -- replace the current block
(lines 34-41):

```
Find the btrfs filesystem UUID:

```sh
sudo btrfs filesystem show /mnt/storage
```

Use the `uuid:` line from that output. `braid status` does not currently show
the btrfs filesystem UUID.
```

with:

```
Find the btrfs filesystem UUID from `braid status` (look for the `FSID:`
line; the JSON form is `braid status --json` and the field is `fsid`):

```sh
sudo braid status
```
```

**6b. Remove the "Future enhancement" block**

Delete lines 173-177 (`braid already tracks the btrfs FSID internally
...`) entirely -- the enhancement is now done.

**6c. Fix the broken ADR 018 escape link**

Line 100 currently reads:

```
mounts. See [ADR 018](../../docs/decisions/018-systemd-lifecycle.md) for the
lifecycle model.
```

mdBook cannot resolve relative escapes out of `manual/` -- this is what
breaks `just check-docs` today and blocks the docs verification step
below. Replace with the absolute GitHub URL (the convention already used
in `manual/guides/ups.md:171,179`):

```
mounts. See [ADR 018](https://github.com/danneu/braid/blob/master/docs/decisions/018-systemd-lifecycle.md)
for the lifecycle model.
```

This is a one-line edit and is in-scope because (a) we are already
editing this same file in this change, and (b) the docs verification
step is part of this plan's acceptance criteria.

No other manual page references `btrfs filesystem show` for UUID lookup
(verified across `manual/`, `docs/`, and `README.md`).

### 7. Verification

Per AGENTS.md, prefer focused runs.

**Rust:**

```sh
just test-rust
```

This runs the unit-test suite that exercises every `StatusReport`
construction site touched above plus the new human/JSON assertions.

**CLI integration:**

```sh
just test-vm braid-status
```

Runs the single Python test extended above (~5 min vs ~25 min for the
full suite). Add `-v` only on failure.

**Parser canary (already covered, run for belt-and-braces):**

```sh
just test-parsers
```

**Manual build check** (run from the worktree root, after the doc
edits including the ADR 018 link fix):

```sh
just check-docs
```

This is the existing link/SUMMARY-parity check (`justfile:214`); it
must pass clean after step 6c. For visual review, `just docs` serves
the manual locally and is the place to confirm the ADR 018 link
resolves and the new `braid status` instructions render correctly.

## Out of scope

- Backwards-compatibility shims around the JSON key (per AGENTS.md
  "No backwards compatibility" -- braid is unreleased).
- TUI display of the FSID. The TUI consumes `PoolState` directly, not
  `StatusReport`; if/when it grows a "pool identity" panel, it can pull
  `pool.fsid` itself. Not required by the mounting-subvolumes guide.
- Exposing per-disk UUIDs (LUKS or btrfs device UUIDs) -- the guide
  needs the filesystem-level FSID only.

## Implementation notes

- Added `fsid: None` to shared `StatusReport` fixture builders in `cli/src/test_fixtures/status.rs`; the plan listed only in-file `cli/src/status.rs` literals, but the new required field must compile everywhere reports are constructed.
