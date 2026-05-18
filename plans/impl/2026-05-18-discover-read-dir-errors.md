# Plan: propagate per-entry read_dir errors in discover

## Context

`discover_from_dir` at `cli/src/discover.rs:302` iterates the `/dev/disk/by-id`
directory with `for entry in entries.flatten()`. `.flatten()` silently drops
any `Err` items the `ReadDir` iterator yields, so a per-entry `getdents64`
failure mid-scan produces a quiet under-report ("no braid-labeled LUKS
devices found" or a missing member) instead of a diagnostic.

This is the only non-display `entries.flatten()` in `cli/src/` and the only
remaining silent-drop hole in a code path that has otherwise been hardened
toward structured visibility (commits `a479197`, `e3a2b94`, `aa67db2`).
The sibling `recover.rs` already takes the opposite (strict-propagating)
stance for the exact same directory.

The original finding (in `findings-*.md` / verify-issue output) proposed a
new `DiscoverWarning::ReadDirEntry { detail }` variant plus a read_dir test
seam. That fix is overbuilt for the failure shape:

- `/dev/disk/by-id/` is devtmpfs; per-entry `getdents64` errors are
  stream-level, not per-symlink. The cited examples (EIO / ENAMETOOLONG /
  EACCES on a single symlink) all fire later inside `canonicalize`, which
  already routes through `CannotCanonicalize` (`discover.rs:316-325`).
- Adding a read_dir seam to `ByIdResolver` (recover.rs:108) widens an
  abstraction the codebase deliberately keeps narrow.

The ideal pivot reuses what is already in scope: the existing
`DiscoverError::ReadDir` variant, already returned by the wholesale
`read_dir(by_id_dir)` failure handler nine lines above. Per-entry `Err` is
the same class of failure (directory stream disruption) and should fail
the scan the same way -- mirroring `recover.rs:RealByIdResolver::list_by_id_entries`
(`recover.rs:120-127`), which uses `.collect::<Result<Vec<_>, _>>()` to
short-circuit on the first per-entry `Err`.

## Approach

Collect the `ReadDir` iterator upfront via
`.collect::<Result<Vec<_>, _>>().map_err(DiscoverError::ReadDir)?`, then loop
over the materialised `Vec<DirEntry>`. No new variant, no new Display arm,
no new test seam.

Collecting first (rather than propagating lazily inside the loop) is the
correct shape because:

- A lazy match would still let the loop body run `CryptsetupIsLuks` and
  `CryptsetupLuksDumpText` probes against the entries that arrived before
  the iterator's `Err`. Failing the scan upfront avoids those side effects.
- A subsequent hard probe error inside the loop could surface before the
  iterator yields its `Err`, masking the original enumeration failure.
  Upfront collection ensures the enumeration error always wins when both
  conditions exist.
- This mirrors `recover.rs:RealByIdResolver::list_by_id_entries`
  (`recover.rs:120-127`) exactly, instead of just approximating it.

### Change

`cli/src/discover.rs` -- materialise the `ReadDir` iterator immediately
after the existing wholesale-failure handler at line 285, then iterate
the `Vec`.

Before:

```rust
let entries = match std::fs::read_dir(by_id_dir) {
    Ok(entries) => entries,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* empty outcome */ }
    Err(e) => return Err(DiscoverError::ReadDir(e)),
};

// ... members + warnings init ...

for entry in entries.flatten() {
    let name = entry.file_name();
```

After:

```rust
let entries = match std::fs::read_dir(by_id_dir) {
    Ok(entries) => entries,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => { /* empty outcome */ }
    Err(e) => return Err(DiscoverError::ReadDir(e)),
};
let entries: Vec<std::fs::DirEntry> = entries
    .collect::<Result<Vec<_>, _>>()
    .map_err(DiscoverError::ReadDir)?;

// ... members + warnings init ...

for entry in entries {
    let name = entry.file_name();
```

That is the entire functional change. The shadowed `entries` binding moves
from `ReadDir` (an iterator of `io::Result<DirEntry>`) to `Vec<DirEntry>`;
the loop body inside the scan does not change.

### What stays the same

- `DiscoverError::ReadDir` (`discover.rs:18-19`) -- already the variant
  used for the wholesale `read_dir(by_id_dir)` failure at `discover.rs:293`.
  Its Display message (`failed to read /dev/disk/by-id: {0}`) reads
  identically for per-entry errors -- both are "we couldn't enumerate
  by-id".
- The early `NotFound` short-circuit at `discover.rs:287-292` is unchanged;
  it triggers from `read_dir()` itself, before iteration begins.
- All existing per-disk warnings (`CannotCanonicalize`, `LuksDumpFailed`,
  `LuksDumpUnparseable`, `UnsupportedLuksVersion`, `InvalidDiskName`,
  `MissingLuksUuid`, `InvalidLuksUuid`) remain on the warning path -- this
  change only affects directory-stream-level errors.

### Out of scope

- The four `entries.flatten()` calls in `cli/src/tui/probe.rs` are
  display-layer approximations whose surrounding comments explicitly
  endorse silent skipping ("broken symlinks and anything that
  canonicalizes outside dev_root are silently skipped -- this is a
  display-layer approximation of what the daemon sees, not parity"). Do
  not change them:
  - `resolve_pwm_dir` (`tui/probe.rs:495`) -- hwmon directory probe for a
    PWM platform device.
  - `resolve_rpm_path` (`tui/probe.rs:556`) -- fan-input file enumeration
    inside a PWM sysfs dir.
  - `enumerate_ata_drives` (`tui/probe.rs:591`) -- ATA drive enumeration
    for the Fans section.
  - `read_drivetemp` (`tui/probe.rs:649`) -- hwmon subdirectory walk for
    drivetemp.
- No new `DiscoverWarning` variant.
- No new `ByIdResolver::read_dir` seam.
- No test for the new per-entry path -- it would require either a real
  faulting directory or a new seam, both of which are too much weight for
  this fix. The pre-existing `DiscoverError::ReadDir` variant is also
  untested today (the wholesale handler at `:293` has no test either);
  this change adopts the same status quo rather than expanding it.

## Files to modify

- `cli/src/discover.rs` -- one loop header at line 302 (the only change).

## Verification

- `just test-rust` -- all existing discover tests must still pass. The
  change cannot affect any current test, since:
  - The dangling-symlink test (`discover_warns_on_dangling_symlink_with_no_luks_device`,
    `discover.rs:1219`) routes failure through `canonicalize`, not through
    iteration.
  - No test exercises per-entry `read_dir` `Err`.
- Skip full `just test-vm` by default -- it is disproportionate for this
  one-line iterator error-propagation change. If an end-to-end smoke is
  desired, run `just test-vm braid-discover` instead of the full suite.
- Manual sanity-check: confirm `cargo check -p braid-cli` is clean -- the
  shadowed `entries` binding moves from `ReadDir` to `Vec<DirEntry>`, so a
  miswritten turbofish on the `collect()` would surface as a type
  mismatch.
