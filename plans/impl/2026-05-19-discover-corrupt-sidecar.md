# Plan: corrupt-sidecar preservation for `discover --write`

## Context

`braid discover --write` is the documented rebuild path for a
`Corrupt`-classified `pool.json`
(`MembershipError::Corrupt::Display`, the bare-discover refusal at
`cli/src/main.rs:803-808`, and `docs/luks-unlock.md:146` all funnel
the operator toward this command). `MembershipError::Corrupt` is
broader than "whole-file parse failure" -- its docstring lists "bad
UUID key, stale value-side field, unknown top-level key, etc.", and
the load-time uniqueness sweep also produces
`MembershipError::Conflict` which `classify_pool_json` lumps into
the `Corrupt` arm via its catch-all `Err(_)` branch. A parseable
UUID-keyed file with a stale value-side `luks_uuid` paired with
`devid:1` lands in `Corrupt` despite carrying useful prior-binding
bytes (decision 024 promotes persisted devid to the only authorized
fallback binding for `null_underlying` mappers and btrfs
`missing_devids`).

Current `HEAD` already implements the healthy-UUID protection from
earlier rounds of this plan:

- `DiscoverWriteError::ValidUuidKeyed` variant
  (`cli/src/discover.rs:183-192`).
- Exhaustive `match classify_pool_json(...)` in
  `write_discovered_membership` (`cli/src/discover.rs:592-610`)
  with `Missing | Corrupt => {}` (proceed).
- Unit test `discover_write_refuses_when_pool_json_is_valid_uuid_keyed`
  (`cli/src/discover.rs:1785`).
- Unit test `discover_write_proceeds_when_pool_json_is_corrupt`
  (`cli/src/discover.rs:1843`) -- seeded with `"not-json"`.
- VM subtest in `tests/cli/braid-discover.py:61-67`.
- Reframed VM subtest in
  `tests/module/pool-lock-discover-contention.py:63-69`.
- Manual wording in `manual/commands/discover.md:56, 70`.

The remaining gap: the staged tree's `Corrupt` arm overwrites
`pool.json` **in place with no forensic snapshot**, destroying any
prior-binding bytes the corrupt file might carry. HEAD's
`cli/src/membership.rs` does not contain `write_corrupt_sidecar` or
`CorruptSidecarError` (the earlier `refresh_pool_metadata` removal
took them out), so the helper must be reintroduced as **new shared
code**, not promoted from an existing implementation. Discover is
the first and only consumer.

This plan adds:

1. A new `write_corrupt_sidecar` helper + `CorruptSidecarError`
   struct in `cli/src/membership.rs`.
2. A new `DiscoverWriteError::CorruptSidecarFailed` variant.
3. Sidecar plumbing in `write_discovered_membership` that runs
   **immediately before `save_membership`** (not inside the shape
   match), so corrupt + count-mismatch returns `ExpectCountUnmet`
   without writing a sidecar, and sidecar failure cannot mask the
   intended refusal.
4. Tests for happy-path rebuild-with-sidecar, sidecar-fail refusal,
   ordering (corrupt + count mismatch produces no sidecar), and the
   helper's own contract.
5. Doc updates so `docs/luks-unlock.md` and
   `manual/guides/recovery-scenarios.md` -- both of which still
   carry the old "remove it first if it is wrong" guidance -- agree
   with the four-shape matrix already in `manual/commands/discover.md`,
   and so all three docs mention the sidecar.

## Recommended approach

Defer the sidecar write to the moment of truth: after every other
`write_discovered_membership` gate has passed and `save_membership`
is about to fire. The shape match merely records "I need a sidecar
before save" via a local boolean; sidecar errors convert to
`CorruptSidecarFailed` and short-circuit before any destructive
write. Sidecar failure is fail-closed because the rebuild is
destructive -- a failed snapshot followed by `save_membership`
would destroy the exact bytes the sidecar exists to preserve. The
helper is `pub(crate)`-visible so any future destructive caller can
reuse the same pattern.

## Critical files

- `cli/src/membership.rs` -- add `pub(crate) fn
  write_corrupt_sidecar` and `pub(crate) struct CorruptSidecarError`
  (with `target` / `source` / `into_source` accessors). Place after
  the existing load/save block and before the test module. Add a
  dedicated `format_rfc3339_utc_seconds(SystemTime) -> String`
  helper for the filename suffix; do **not** reuse
  `crate::util::now_iso` (`Iso8601::DEFAULT`), whose subsecond
  precision disagrees with the documented sidecar filename shape.
- `cli/src/discover.rs:155-200` -- add a `CorruptSidecarFailed
  { sidecar: String, #[source] source: std::io::Error }` variant
  beside `ValidUuidKeyed`.
- `cli/src/discover.rs:592-625` -- modify
  `write_discovered_membership` so the `Corrupt` arm of the shape
  match only records a `needs_corrupt_sidecar` flag, the existing
  `expected_count` gate runs unchanged, and the new sidecar call
  runs immediately before `save_membership`.
- `cli/src/discover.rs:567-579` -- extend
  `write_discovered_membership`'s doc-comment gates list from three
  items to four; add gate 4 for the sidecar precondition.
- `cli/src/discover.rs:1843` -- evolve the existing
  `discover_write_proceeds_when_pool_json_is_corrupt` test from a
  bare "rebuild succeeds" check into a forensic-preservation
  check: seed with a stale-`luks_uuid`-plus-`devid:1` blob and
  assert the sidecar exists + has RFC3339 shape + bytes equal the
  seed. Add three new tests beside it (sidecar-fail refusal,
  ordering-vs-count-mismatch, helper-level coverage in the
  `membership.rs` test module).
- `manual/commands/discover.md:56, 70` -- light edits to the
  already-landed wording so it mentions the sidecar and the
  fail-closed behavior.
- `docs/luks-unlock.md:144-160` -- split the corrupt-vs-legacy
  remediation paragraph; the current single paragraph lumps both
  shapes together. Add the sidecar caveat.
- `manual/guides/recovery-scenarios.md:73` -- replace the "remove
  it first if it is wrong" bullet with a four-shape split.

## Implementation steps

### 1. Add the new error variant

In `cli/src/discover.rs` after the existing `ValidUuidKeyed` variant
(currently at lines 183-192), append:

```rust
/// Existing `pool.json` is `Corrupt` and would normally be rebuilt
/// in place, but the forensic snapshot to
/// `pool.json.corrupt-<RFC3339-UTC>` could not be written
/// (ENOSPC, EACCES on the state directory, etc.). `discover
/// --write` refuses rather than destroying the original bytes --
/// a corrupt file can still carry prior-binding `devid` data, so
/// overwriting without a snapshot is data loss. The sidecar is a
/// hard precondition because the rebuild is destructive.
#[error(
    "discover refusing to write pool.json: failed to snapshot existing corrupt file to {sidecar}: {source} -- refusing to overwrite the corrupt original without a forensic copy; free disk space or fix permissions on the state directory and retry"
)]
CorruptSidecarFailed {
    sidecar: String,
    #[source]
    source: std::io::Error,
},
```

### 2. Add the sidecar helper in `membership.rs`

Place after the existing load/save block (before
`#[cfg(test)] mod tests`):

```rust
/// Forensic copy of a state file to a timestamped sidecar adjacent
/// to it (`<path>.corrupt-<RFC3339-UTC>`, with `.1` / `.2` / ...
/// collision suffixes). Destructive callers must gate their
/// overwrite on a successful sidecar write so the original bytes
/// survive; read-side callers may choose warn-and-continue. Never
/// overwrites an existing sidecar -- the whole point is to preserve
/// prior forensic bytes -- and fsyncs both the new file and the
/// parent directory before returning so a crash after this helper
/// returns leaves the sidecar durable on disk.
pub(crate) fn write_corrupt_sidecar(path: &Path) -> Result<(), CorruptSidecarError> {
    write_corrupt_sidecar_at(path, std::time::SystemTime::now())
}

/// Inner entry point parameterized on the `SystemTime` used for the
/// filename suffix. Exists so tests can preseed a known collision
/// and pass the same instant to the helper, eliminating the
/// second-rollover race that a `SystemTime::now()` call site would
/// otherwise expose.
pub(crate) fn write_corrupt_sidecar_at(
    path: &Path,
    now: std::time::SystemTime,
) -> Result<(), CorruptSidecarError> {
    use std::fs::OpenOptions;
    use std::io::{ErrorKind, Write};

    let raw = std::fs::read(path).map_err(|e| CorruptSidecarError {
        target: path.to_path_buf(),
        source: e,
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("pool.json");
    let ts = format_rfc3339_utc_seconds(now);
    let base = format!("{file_name}.corrupt-{ts}");

    // Atomic no-clobber create: each iteration opens with
    // `create_new(true)`, which is the kernel-level guarantee that
    // exactly one writer wins if two callers race on the same name.
    // An `AlreadyExists` from the kernel is the only error we
    // retry; any other I/O error (ENOSPC, EACCES, EROFS, etc.)
    // surfaces as `CorruptSidecarError` immediately. Mirrors the
    // pattern at `cli/src/alert.rs:346` (`hard_link` no-clobber)
    // and `cli/src/enroll_key_file.rs:337` (`create_new(true)`).
    const MAX_COLLISIONS: u32 = 1000;
    for n in 0..MAX_COLLISIONS {
        let candidate = if n == 0 {
            parent.join(&base)
        } else {
            parent.join(format!("{base}.{n}"))
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                f.write_all(&raw).map_err(|e| CorruptSidecarError {
                    target: candidate.clone(),
                    source: e,
                })?;
                // Durability: fsync the file then fsync the parent
                // directory before returning so a crash between
                // `write_corrupt_sidecar` and the destructive
                // `save_membership` that follows cannot leave the
                // new `pool.json` durable while losing the
                // forensic snapshot. Mirrors the file+parent fsync
                // pattern in `crate::state_io::atomic_write`
                // (`cli/src/state_io.rs:40-71`).
                f.sync_all().map_err(|e| CorruptSidecarError {
                    target: candidate.clone(),
                    source: e,
                })?;
                crate::state_io::sync_dir(parent).map_err(|e| {
                    CorruptSidecarError {
                        target: candidate.clone(),
                        source: e,
                    }
                })?;
                return Ok(());
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(CorruptSidecarError {
                    target: candidate,
                    source: e,
                });
            }
        }
    }
    // Realistically unreachable -- 1000 corruption events in the
    // same RFC3339 second is not a real operational mode -- but
    // failing closed beats clobbering an existing sidecar. The
    // caller surfaces the error so an operator who somehow hits
    // this can investigate.
    Err(CorruptSidecarError {
        target: parent.join(format!("{base}.{MAX_COLLISIONS}")),
        source: std::io::Error::new(
            ErrorKind::AlreadyExists,
            format!("exhausted {MAX_COLLISIONS} sidecar candidates -- refusing to overwrite an existing forensic snapshot"),
        ),
    })
}

/// Failure surface for `write_corrupt_sidecar`. Carries the target
/// path the helper attempted to write (`<pool.json>.corrupt-<RFC3339>`
/// or a `.N` retry) so callers can name the file the operator would
/// expect to find -- and the underlying `std::io::Error` so the
/// caller can either borrow it (read-side warn-and-continue) or
/// move it out (destructive `--write` `#[source]` chaining).
#[derive(Debug)]
pub(crate) struct CorruptSidecarError {
    target: PathBuf,
    source: std::io::Error,
}

impl CorruptSidecarError {
    pub(crate) fn target(&self) -> &Path { &self.target }
    pub(crate) fn source(&self) -> &std::io::Error { &self.source }
    pub(crate) fn into_source(self) -> std::io::Error { self.source }
}

/// Build a sidecar filename suffix in the exact
/// `YYYY-MM-DDTHH:MM:SSZ` shape the operator-facing artifact
/// (`pool.json.corrupt-<RFC3339-UTC>`) requires. Distinct from
/// `crate::util::now_iso`, which uses `Iso8601::DEFAULT` and
/// emits subsecond precision (`...:56.789012345Z`) that would
/// disagree with the documented filename convention and the test
/// assertions.
fn format_rfc3339_utc_seconds(now: std::time::SystemTime) -> String {
    let odt: time::OffsetDateTime = now.into();
    let format = time::format_description::parse(
        "[year]-[month]-[day]T[hour]:[minute]:[second]Z",
    )
    .expect("static format description must parse");
    odt.to_offset(time::UtcOffset::UTC)
        .format(&format)
        .expect("formatting OffsetDateTime as RFC3339 seconds must not fail")
}
```

Key invariants this helper pins:

- **No clobbering.** `OpenOptions::create_new(true)` is the
  kernel-level no-clobber primitive. There is no `exists()`
  check, so there is no TOCTOU window between the "is this
  name free" decision and the write. If two `discover --write`
  invocations race (e.g. a hung wrapper-lock holder and a
  parallel one), exactly one wins on each candidate name and the
  other retries on the next suffix.
- **Exhaustion fails closed.** Reaching the 1000-collision cap
  returns `CorruptSidecarError` rather than silently overwriting
  a `.999` slot. The cap is high enough that real operator
  workflows never hit it.
- **Timestamp shape is dedicated.** `format_rfc3339_utc_seconds`
  intentionally does NOT reuse `crate::util::now_iso`, which uses
  `Iso8601::DEFAULT` (subsecond precision, e.g.
  `2026-05-19T12:34:56.789012345Z`). The sidecar filename
  convention -- documented in `manual/commands/discover.md`,
  `docs/luks-unlock.md`, and `manual/guides/recovery-scenarios.md`
  -- is seconds-only with a literal `Z`. The dedicated helper
  pins that shape and gets its own unit test against a fixed
  `SystemTime` (see step 5e).
- **Durability matches `save_membership`.** The helper fsyncs the
  new sidecar file (`f.sync_all`) and the parent directory
  (`crate::state_io::sync_dir`) before returning. Without those
  fsyncs, a crash between the sidecar's `write_all` returning and
  the subsequent `save_membership` (which uses
  `crate::state_io::atomic_write` -- itself a file+parent fsync
  per `cli/src/state_io.rs:40-71`) could leave the new
  `pool.json` durable on disk while the sidecar bytes were
  cached-but-unflushed -- defeating the entire purpose of the
  gate. The two writes now have matching durability semantics.
- **Time injection for tests.** The public
  `write_corrupt_sidecar` takes no time parameter and uses
  `SystemTime::now()` internally. A `pub(crate)`
  `write_corrupt_sidecar_at(path, now)` variant exists so tests
  can pass a fixed `SystemTime` and preseed a collision at the
  exact name the helper will pick -- eliminating the
  second-rollover race a `now()` call site would otherwise
  expose. Production callers (the discover Corrupt arm) use the
  public entry point.

### 3. Plumb the sidecar into `write_discovered_membership` with correct ordering

The current `write_discovered_membership` runs:

1. pending-op check
2. shape match (refuses `LegacyNameKeyed` / `ValidUuidKeyed`,
   proceeds on `Missing | Corrupt`)
3. `expected_count` check
4. `save_membership`

Modify step 2 to record `needs_corrupt_sidecar` without performing
any I/O, and insert a sidecar step between (3) and (4):

```rust
let pool_json_path = paths.pool_json();
let needs_corrupt_sidecar = match classify_pool_json(&pool_json_path) {
    PoolJsonShape::LegacyNameKeyed => {
        return Err(DiscoverWriteError::NameKeyedPoolJson {
            path: pool_json_path.display().to_string(),
        });
    }
    PoolJsonShape::ValidUuidKeyed => {
        return Err(DiscoverWriteError::ValidUuidKeyed {
            path: pool_json_path.display().to_string(),
        });
    }
    // `Missing` is the normal first-write path; no prior file to
    // preserve. `Corrupt` is the documented rebuild remediation,
    // but the corrupt file may still hold prior-binding bytes
    // (stale value-side `luks_uuid` paired with `devid:1`, etc.) --
    // we record the intent to snapshot it and defer the actual
    // write until just before `save_membership` so the snapshot
    // only happens on the success path.
    PoolJsonShape::Corrupt => true,
    PoolJsonShape::Missing => false,
};

if let Some(expected) = expected_count {
    let actual = members.len();
    if actual != expected {
        return Err(DiscoverWriteError::ExpectCountUnmet { expected, actual });
    }
}

if needs_corrupt_sidecar {
    // Last gate before the destructive write. Sidecar failure is
    // fail-closed because a failed snapshot followed by
    // `save_membership` would destroy the bytes the sidecar
    // exists to preserve.
    crate::membership::write_corrupt_sidecar(&pool_json_path)
        .map_err(|e| DiscoverWriteError::CorruptSidecarFailed {
            sidecar: e.target().display().to_string(),
            source: e.into_source(),
        })?;
}

save_membership(&members, paths)?;
Ok(members)
```

Ordering invariants this captures:

- A corrupt `pool.json` with a count mismatch returns
  `ExpectCountUnmet` and writes no sidecar (no destructive write
  follows, so no sidecar is needed).
- A corrupt `pool.json` with the right count and an unwritable
  state directory returns `CorruptSidecarFailed` and writes no
  `pool.json` (the original corrupt bytes survive).
- A corrupt `pool.json` with the right count and a writable state
  directory writes the sidecar, then writes the new `pool.json`,
  in that order.

### 4. Update the gate doc comment

`write_discovered_membership`'s doc comment currently lists three
gates. Extend to four:

> 4. When `pool.json` is `Corrupt`, the forensic snapshot to
>    `pool.json.corrupt-<RFC3339-UTC>` must succeed before the
>    rebuild proceeds (covered by `CorruptSidecarFailed`). The
>    sidecar is written *after* `expected_count` validates, so a
>    count-mismatch refusal does not leave behind a sidecar of a
>    file that was not going to be overwritten.

### 5. Update + add Rust unit tests

**5a. Evolve `discover_write_proceeds_when_pool_json_is_corrupt`**
(currently at `cli/src/discover.rs:1843`):

- Replace the `"not-json"` seed with a UUID-keyed JSON blob whose
  value-side `luks_uuid` is stale (does not match its UUID key) and
  whose entry carries `devid:1`. This triggers
  `classify_pool_json` -> `Corrupt` through
  `MembershipError::Corrupt` ("stale value-side field") and
  exercises the prior-binding-bytes case directly.
- Capture the seed bytes into `pool_json_pre` before the call.
- Keep the existing "rebuild succeeded" assertion (loadable
  membership, contains the discovered member).
- Add: directory scan -- assert exactly one entry matches
  `pool.json.corrupt-<RFC3339-UTC>` with a `YYYY-MM-DDTHH:MM:SSZ`
  suffix.
- Add: assert the sidecar's bytes equal `pool_json_pre`.

Rename the test to
`discover_write_rebuilds_and_snapshots_when_pool_json_is_corrupt`
so the name reflects both invariants. Update the test's preamble
to match.

**5b. New test
`discover_write_refuses_when_corrupt_sidecar_cannot_be_written`**:

- Seed `pool.json` with the same stale-`luks_uuid`-plus-`devid:1`
  blob; capture `pool_json_pre`.
- Make the state directory read-only via
  `std::os::unix::fs::PermissionsExt::set_mode(0o500)`. Use an
  RAII guard that restores `0o700` before `TempDir::drop` (or a
  helper that does the same via `Drop`) so the cleanup always
  runs, even on panic.
- Build the same single-member `DiscoverOutcome`.
- Call `write_discovered_membership(outcome, &paths, None)`;
  expect `DiscoverWriteError::CorruptSidecarFailed`.
- Assert `std::fs::read(paths.pool_json()).unwrap() ==
  pool_json_pre` (corrupt original survives byte-for-byte).
- Assert no `pool.json.corrupt-*` entry exists in the parent dir.
- Gate behind `#[cfg(target_family = "unix")]`.

**5c. New test
`discover_write_returns_expect_count_before_sidecar_when_corrupt_and_count_mismatches`**:

- Seed `pool.json` with the same corrupt blob; capture
  `pool_json_pre`.
- Build a two-member `DiscoverOutcome` (or any count not equal to
  `expected_count`).
- Call `write_discovered_membership(outcome, &paths, Some(1))`;
  expect `DiscoverWriteError::ExpectCountUnmet { expected: 1,
  actual: 2 }`.
- Assert `std::fs::read(paths.pool_json()).unwrap() ==
  pool_json_pre` (no overwrite).
- Assert no `pool.json.corrupt-*` entry exists -- the sidecar must
  not fire when the rebuild is going to be refused. This pins the
  ordering invariant from step 3.

**5d. New helper-level test in
`cli/src/membership.rs`'s `#[cfg(test)] mod tests`:**
`write_corrupt_sidecar_preserves_existing_snapshot_and_appends_suffix`:

- Seed a tempdir with `pool.json` containing arbitrary bytes
  including a `devid:1` field; capture into `seed_bytes`.
- **Pick a fixed `SystemTime`** -- e.g. `SystemTime::UNIX_EPOCH +
  Duration::from_secs(1_700_000_000)`. Compute the expected
  primary sidecar name via
  `format_rfc3339_utc_seconds(t)` (the helper is private to the
  module so the test can call it directly). Write a sentinel
  value (`b"DO NOT CLOBBER"`) at
  `<tempdir>/pool.json.corrupt-<ts>`. This guarantees the
  preseed and the helper invocation agree on the timestamp;
  there is no `SystemTime::now()` call between them, so there is
  no second-rollover race.
- Call `write_corrupt_sidecar_at(&pool_json, t)`; expect
  `Ok(())`.
- Assert the preseeded primary sidecar is **byte-for-byte
  unchanged** (`b"DO NOT CLOBBER"`). This is the core
  forensic-preservation invariant: a regression that broke
  no-clobber would clobber this file and the assertion would
  fail.
- Assert a new sidecar exists at the `.1` suffix and its bytes
  equal `seed_bytes`.
- Assert the original `pool.json` is unchanged.
- Call `write_corrupt_sidecar_at(&pool_json, t)` a second time
  with the **same** fixed `t`; expect `Ok(())`. Assert the
  preseeded primary and the first retry sidecar are both
  unchanged, and a third sidecar exists at `.2` with bytes
  equal to `seed_bytes`.

This helper-level test gives `write_corrupt_sidecar` standalone
regression coverage independent of its first caller, with
explicit no-clobber assertions. A future refactor that
reintroduced an `exists()` + `write()` race -- or restored a
clobber-on-exhaustion fallback -- would fail the
`DO NOT CLOBBER` assertion. The test uses the `_at` entry point
specifically so the assertion is deterministic; the public
`write_corrupt_sidecar` is exercised end-to-end by 5a / 5b in
the discover-level tests.

**5e. New helper-level test
`format_rfc3339_utc_seconds_emits_seconds_only_with_z_suffix`:**

- Build a fixed `SystemTime` -- e.g. `SystemTime::UNIX_EPOCH +
  Duration::from_secs(1_700_000_000)` (a stable, picked-once
  Unix timestamp that maps to a known UTC instant).
- Call `format_rfc3339_utc_seconds(t)`; assert the returned
  string equals exactly the precomputed
  `YYYY-MM-DDTHH:MM:SSZ` for that instant (no subsecond, no
  offset, literal `Z`).
- Add a second case with a different fixed timestamp to catch a
  regression that hardcoded the first.

This pins the filename convention against the
`crate::util::now_iso` shape (`Iso8601::DEFAULT`, subsecond
precision) so a future "DRY" refactor that aliases the two
formatters would fail the assertion.

### 6. Touch up `manual/commands/discover.md`

The four-shape matrix landed in HEAD, but the lines about
`discover --write` proceeding on corrupt don't mention the sidecar
or the fail-closed behavior. Edit step 2 in "What happens under
the hood" (`manual/commands/discover.md:56`):

> Refuses on a healthy UUID-keyed `pool.json` (bare and `--write`).
> Refuses on a legacy name-keyed `pool.json` for `--write`; bare
> `discover` prints a migration hint and continues as a preview. A
> corrupt or off-schema `pool.json` is the documented rebuild
> path: bare `discover` prints the rebuild remediation, and
> `discover --write` writes a forensic
> `pool.json.corrupt-<RFC3339-UTC>` snapshot adjacent to the new
> file, then rebuilds. If the snapshot cannot be written (full
> disk, read-only state directory), `discover --write` refuses
> rather than destroy the corrupt original.

And the "Safety checks" bullet (`manual/commands/discover.md:70`):

> Refuses any operation on a healthy UUID-keyed `pool.json`.
> Corrupt or off-schema files are allowed for `--write` rebuild
> only; the original is copied to
> `pool.json.corrupt-<RFC3339-UTC>` before overwrite, and
> `--write` refuses if that snapshot cannot be written (full disk,
> read-only state directory). (Run with all intended pool members
> attached; see `docs/luks-unlock.md`.) Legacy name-keyed files
> are allowed only for read-only preview.

### 7. Split the corrupt-vs-legacy guidance in `docs/luks-unlock.md`

`docs/luks-unlock.md:144-160` currently lumps "corrupt or
old-shape" together and tells the operator to "Move the old
`pool.json` aside". After the fix, move-aside is required for
legacy (and healthy UUID-keyed), but corrupt files are rebuilt in
place with a sidecar. Replace the single paragraph with three:

> For a **corrupt or off-schema** `pool.json`, the remediation
> phrase is unchanged: `run 'braid discover --write' to rebuild
> from existing disks (with all intended pool members attached;
> see docs/luks-unlock.md)`. Confirm the attached disks are the
> intended pool members, then run `braid discover --write` -- the
> corrupt file is overwritten in place and the original bytes are
> preserved at `pool.json.corrupt-<RFC3339-UTC>` next to it. The
> snapshot is a hard precondition for the rebuild: if it cannot be
> written (full disk, read-only state directory), `discover
> --write` refuses with `failed to snapshot existing corrupt file
> to ...` so the corrupt original is not destroyed; free disk
> space or fix permissions and retry. The sidecar is safe to
> remove once you have manually copied any still-relevant
> prior-binding bytes (e.g. `devid` for a `null_underlying`
> member). During a single-user cutover, pass `--expect-count`
> with the member count from the old file so a temporarily
> detached disk cannot silently produce a smaller membership and
> an unrelated braid-labeled disk cannot be silently admitted.
>
> For a **legacy name-keyed** `pool.json` (pre-UUID-identity
> migration), `discover --write` refuses with an explicit
> move-aside message. Back up the file and move it aside
> (`mv /var/lib/braid/pool.json /var/lib/braid/pool.json.legacy`)
> before running `braid discover --write`.
>
> For a **healthy UUID-keyed** `pool.json`, do not run
> `discover --write` at all -- use `braid add` / `braid remove` /
> `braid replace` to mutate membership. `discover --write` is a
> repair tool, not a refresh; running it against a healthy file
> refuses (`is already a healthy UUID-keyed membership`) so it
> does not drop persisted devid bindings (decision 024).

Preserve the byte-identical remediation phrase
(`run 'braid discover --write' to rebuild from existing disks ...`)
because it appears verbatim in
`MembershipError::Corrupt::Display`; doc and code message families
must stay aligned.

### 8. Reconcile `manual/guides/recovery-scenarios.md`

`manual/guides/recovery-scenarios.md:73` currently has a single
bullet:

> For a new UUID-keyed or otherwise unrecognized existing file,
> remove it first if it is wrong.

Replace with two bullets that match the four-shape matrix in
`manual/commands/discover.md`:

> - Bare `discover` previews when the existing `pool.json` is the
>   legacy name-keyed shape. For a healthy UUID-keyed
>   `pool.json`, `discover --write` refuses -- use `braid add` /
>   `braid remove` / `braid replace` to mutate membership instead.
> - For a corrupt or off-schema existing `pool.json`,
>   `discover --write` rebuilds in place; no manual remove step is
>   needed. The original bytes are preserved at
>   `pool.json.corrupt-<RFC3339-UTC>` adjacent to the new file in
>   case manual forensic recovery is needed (e.g. extracting a
>   `devid` for a `null_underlying` member). The snapshot is a
>   hard precondition: if it cannot be written (full disk,
>   read-only state directory), `discover --write` refuses rather
>   than destroy the corrupt original; free disk space or fix
>   permissions and retry.

## Status against current HEAD (already done -- do not redo)

These pieces landed in earlier incremental commits and are already
in `HEAD`. The plan does **not** reintroduce them; the implementer
should run the relevant `git grep` checks below to confirm they're
still present before starting:

- `DiscoverWriteError::ValidUuidKeyed` variant
  (`cli/src/discover.rs:183-192`).
- `match classify_pool_json(...)` block in
  `write_discovered_membership` with the `ValidUuidKeyed` refusal
  arm (`cli/src/discover.rs:592-610`).
- `discover_write_refuses_when_pool_json_is_valid_uuid_keyed`
  unit test (`cli/src/discover.rs:1785`).
- VM subtest in `tests/cli/braid-discover.py:61-67`
  ("discover --write also refuses healthy UUID-keyed pool.json").
- Reframed final subtest in
  `tests/module/pool-lock-discover-contention.py:63-69`
  ("discover reaches CLI after lock release and refuses healthy
  UUID-keyed pool.json").
- Manual wording in `manual/commands/discover.md:56, 70` (the
  four-shape matrix, minus the sidecar mentions -- step 6 above
  adds those).

## What is intentionally NOT changed

- **`Corrupt` is still allowed for `--write`** -- but now with a
  fail-closed forensic sidecar. The documented rebuild remediation
  is unchanged from the operator's perspective on a healthy host;
  the new refusal only fires when the host is itself unwell and
  forensic preservation cannot be guaranteed.
- **No new pool-lock acquisition inside the Rust CLI.** Installed
  `braid discover` is already serialized at the wrapper level by
  `modules/braid/braid-wrapper.sh:36-57` (Principle 12 / decision
  018). The gate added here is orthogonal: it protects against a
  single operator's own destructive command, not against
  concurrent braid processes.
- **`--expect-count` semantics** unchanged. Order of gates is
  rearranged in step 3 so a count refusal short-circuits before
  the sidecar runs, but the error contract is the same.

## Existing patterns reused

- `classify_pool_json` four-shape return
  (`cli/src/discover.rs:218-233`) -- already discriminates the
  states the new sidecar path needs.
- `DiscoverWriteError` variant style and the `discover refusing to
  write pool.json: ...` template
  (`cli/src/discover.rs:159-179`).
- Test fixture pattern from
  `classify_pool_json_returns_corrupt_for_value_side_conflict`
  for the stale-`luks_uuid` seed.
- Sidecar helper shape and naming convention recovered from the
  previous HEAD-era `write_corrupt_sidecar` implementation
  (removed earlier alongside `refresh_pool_metadata`); reintroduced
  with **stronger no-clobber semantics** so the operator-facing
  artifact is the same one prior workflow docs and reviews
  reference.
- Atomic no-clobber file creation pattern: `OpenOptions::create_new(true)`
  with `ErrorKind::AlreadyExists` retry, modeled on the existing
  uses at `cli/src/alert.rs:346` (`std::fs::hard_link` for the
  alert-latch corruption sidecar) and
  `cli/src/enroll_key_file.rs:337` (`create_new(true)` for
  keyfile generation). The shared invariant across all three
  sites: never overwrite a forensic artifact whose whole purpose
  is to preserve prior bytes.
- File-plus-parent fsync pattern: `f.sync_all()` followed by
  `crate::state_io::sync_dir(parent)` -- the same durability
  shape `crate::state_io::atomic_write` already enforces for
  `pool.json` (`cli/src/state_io.rs:40-71`). The sidecar matches
  that pattern so a crash between the sidecar write and the
  `save_membership` overwrite cannot leave the new `pool.json`
  durable while the forensic snapshot is lost.

## Verification

1. `just test-rust` -- runs:
   - the evolved
     `discover_write_rebuilds_and_snapshots_when_pool_json_is_corrupt`
     (was `discover_write_proceeds_when_pool_json_is_corrupt`),
   - the new
     `discover_write_refuses_when_corrupt_sidecar_cannot_be_written`,
   - the new
     `discover_write_returns_expect_count_before_sidecar_when_corrupt_and_count_mismatches`,
   - the new helper-level
     `write_corrupt_sidecar_preserves_existing_snapshot_and_appends_suffix`,
   - the new helper-level
     `format_rfc3339_utc_seconds_emits_seconds_only_with_z_suffix`,
   - the existing `discover_write_proceeds_when_no_gates_fire`
     (Missing pool.json),
     `discover_write_refuses_when_pool_json_is_name_keyed`,
     `discover_write_refuses_when_pending_op_exists`, and
     `discover_write_refuses_when_pool_json_is_valid_uuid_keyed`.
2. `just test-vm braid-discover` -- existing healthy-UUID subtest
   continues to pass; no new VM-level test is added for the
   sidecar (the contract is unit-level and the helper is exercised
   on every corrupt rebuild that VM tests already cover).
3. `just test-vm braid-discover-migration` -- should pass
   unchanged; the legacy-shape, missing-pool.json, and
   count-mismatch scenarios are orthogonal.
4. `just test-vm pool-lock-discover-contention` -- existing
   reframed final subtest continues to pass.
5. Pre-implementation `git grep` checks to confirm the landed
   pieces are still there:
   - `git grep "ValidUuidKeyed" cli/src/discover.rs` -- expect
     hits at the enum variant, the match arm, the doc comment,
     and the unit test.
   - `git grep "is already a healthy UUID-keyed" cli/src/
     manual/ tests/` -- expect hits in the error string, the
     bare-discover refusal, both VM tests, and the manual.
6. Post-implementation `git grep` checks:
   - `git grep "write_corrupt_sidecar\|CorruptSidecarError"
     cli/src/` -- expect helper + struct in `membership.rs`,
     and one call site in `discover.rs`.
   - `git grep "CorruptSidecarFailed"` -- expect the enum
     variant, the call site, and the unit test.
   - `git grep "pool.json.corrupt-"` -- expect mentions in
     `manual/commands/discover.md`, `docs/luks-unlock.md`,
     `manual/guides/recovery-scenarios.md`, and the unit tests.
7. Manual sanity check on a dev VM:
   - `printf '%s' '{"disks":{"11111111-1111-1111-1111-111111111111":{"name":"disk1","by_id":"/dev/disk/by-id/foo","luks_uuid":"22222222-2222-2222-2222-222222222222","devid":1}}}' > /var/lib/braid/pool.json` (stale value-side `luks_uuid`)
   - Run `braid discover --write`; confirm rebuild succeeds and
     `ls /var/lib/braid/pool.json.corrupt-*` shows the snapshot
     containing the original `devid:1` bytes.
   - Re-seed the corrupt blob, `chmod 500 /var/lib/braid`, run
     `braid discover --write`, and confirm refusal with
     `failed to snapshot existing corrupt file to ...` plus the
     corrupt original untouched. Restore with `chmod 700
     /var/lib/braid` afterwards.
