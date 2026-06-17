# Plan: bare `braid-` mapper must not be an orphan cleanup candidate

## Context

`braid lock` scans `/dev/mapper/braid-*` for stray mappers left by a prior
crash and closes any whose backing LUKS UUID is not a pool member ("orphans").
The candidacy filter and the orphan display-label both derive the disk name via
`name_from_mapper`, which is just `mapper.strip_prefix("braid-")`. For the bare
string `"braid-"` that returns `Some("")`, not `None`.

Consequence: a dm device named exactly `braid-` (empty suffix) passes the
candidate filter (`name_from_mapper(&entry).is_none()` is `false`), and if
`cryptsetup status` succeeds with a non-member UUID it is closed as an orphan
with a **blank** disk label -- emitting a degenerate `disk : locking
(orphan)...` row and `orphaned mapper braid- (...)` warn.

This is a Low-severity, operator-created edge (braid never creates a bare
`braid-`; `DiskName` is never empty). It does **not** violate the UUID-identity
invariant -- member vs orphan is still decided by backing LUKS UUID. The blast
radius is cosmetic (a blank label), but the bare `braid-` should simply never be
treated as a braid mapper. The ideal fix dissolves the whole "blank disk label"
class, not just the one reachable path, while keeping a single source of truth
for "what is a braid mapper's usable disk name."

Root cause and intended outcome: `name_from_mapper` conflates two notions --
"raw prefix strip" (display-only) and "usable disk name" (candidacy + labels).
The bare-prefix case is the only input where they diverge. Split the notions so
candidacy and labels use the non-empty one.

## Why not change `name_from_mapper` itself

Tempting one-liner: make `name_from_mapper("braid-")` return `None`. **Rejected**
-- it regresses `discover`. `cli/src/discover.rs#discover_from_dir_inner`
strips with `name_from_mapper` and *then* runs `DiskName::parse`, so a bare
`braid-` label currently surfaces a helpful `DiscoverWarning::InvalidDiskName`
("relabel this") instead of a silent skip. Collapsing the helper to `None` would
turn that warning into a silent `continue`. The `Some("")` return is therefore
intentional and load-bearing for discover; the fix belongs at the lock-side
consumers.

Also rejected: tightening the filter to require `DiskName::parse` success (as
discover does). That over-tightens candidacy against ADR 024's documented
contract -- "lock may use the `braid-*` prefix only to discover cleanup
candidates; member identity still requires UUID/devid evidence"
(`docs/design/decisions/024-luks-uuid-identity.md`). Keeping candidacy loose
(any non-empty `braid-` suffix) preserves robustness to future `DiskName`
contract changes; the UUID gate remains the real owner of identity. The empty
suffix is the unique case that is never valid in *any* contract version and the
only one that renders a blank label, so it is the only thing to special-case.

## Design

Introduce the "usable braid disk name" notion once, and route both the candidacy
filter and the orphan label through it.

### 1. `cli/src/config.rs` -- split the two notions

- **Augment** `name_from_mapper`'s doc comment to record that `Some("")` for a
  bare `braid-` is intentional and that `discover` depends on it (so no future
  reviewer "simplifies" it and breaks discover). Behavior unchanged.
- **Add** a sibling parser (place it directly below `name_from_mapper`):

  ```rust
  /// The usable disk name of a braid mapper: the `braid-` suffix when it is
  /// non-empty, else `None`. Unlike `name_from_mapper` (raw strip, which keeps
  /// the empty suffix for discover's invalid-name warning), a bare `braid-` has
  /// no disk name -- braid never creates one (`DiskName` is never empty), so it
  /// is neither a cleanup candidate nor a renderable label.
  pub(crate) fn braid_disk_name(mapper: &str) -> Option<&str> {
      name_from_mapper(mapper).filter(|name| !name.is_empty())
  }
  ```

  (`pub(crate)` is enough; it is consumed only by `lock.rs`. Match
  `name_from_mapper`'s visibility if clippy prefers.)

### 2. `cli/src/lock.rs` -- candidacy filter + non-blank label helper

- Import `braid_disk_name` alongside the existing `name_from_mapper` import
  (`use crate::config::{Config, name_from_mapper, braid_disk_name};`).
  `name_from_mapper` is no longer referenced in `lock.rs` after the reroute
  below -- drop it from the import if so.
- **Candidacy filter** in `cli/src/lock.rs#scan_braid_mapper_candidates` (the
  `name_from_mapper(&entry).is_none()` filter):

  ```rust
  // Keep only `braid-<name>` mappers with a non-empty disk name. A bare
  // `braid-` is never braid-created (DiskName is never empty), so it is not a
  // cleanup candidate; ownership stays UUID-gated downstream.
  if braid_disk_name(&entry).is_none() {
      continue;
  }
  ```

- **Add** a private label helper near the other `*_warn_body` formatters:

  ```rust
  /// Display label for an orphan mapper close: the braid disk name when
  /// present, else the full mapper basename. Guarantees an orphan status row
  /// (`disk <label>: locking (orphan)...`) never renders a blank label, even
  /// for a degenerate bare `braid-` mapper.
  fn orphan_disk_label(mapper: &MapperName) -> String {
      braid_disk_name(mapper.as_str())
          .unwrap_or(mapper.as_str())
          .to_owned()
  }
  ```

- **Reroute** the three identical `name_from_mapper(x).unwrap_or(x).to_owned()`
  orphan-label derivations through it:
  - `cli/src/lock.rs#classify_candidate_mapper` (orphan branch):
    `disk_name: orphan_disk_label(mapper)`
  - `cli/src/lock.rs#build_close_sets_full` Pass 1 (`pool.devices` loop):
    `let disk_name = orphan_disk_label(&dev.mapper);`
  - `cli/src/lock.rs#build_close_sets_full` Pass 2 (`pool.null_underlying` loop):
    `let disk_name = orphan_disk_label(&nu.mapper);`

This is behavior-preserving for every non-empty suffix (`braid-ccc` -> `ccc`,
exactly as today), so all existing orphan assertions are untouched. The only
behavioral deltas are the intended ones: the bare `braid-` is no longer a
candidate (filter), and were it ever to reach a label site (the more-degenerate
Pass 1/2 path: a btrfs pool backed by a `braid-`-named dm device) it renders the
full `braid-` instead of a blank.

## Tests

All new tests carry the neighbors' `Intent / Why it exists / Scenario` preamble.

- **`cli/src/config.rs`** (unit): pin the sharp edge and the new parser.
  - Characterize `name_from_mapper("braid-") == Some("")` (the bug's hinge; also
    documents why discover keeps working).
  - `braid_disk_name`: `"braid-aaa" -> Some("aaa")`, `"braid-" -> None`,
    `"luks-x" -> None`, `"" -> None`.

- **`cli/src/lock.rs`** (unit, in the existing `mod tests`):
  - **Filter regression (candidacy fix)** -- call `scan_braid_mapper_candidates` directly:
    ```rust
    let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-", "/dev/mapper/braid-bbb"]);
    let candidates = scan_braid_mapper_candidates(&fs, &HashSet::new()).unwrap();
    let names: Vec<&str> = candidates.iter().map(|m| m.as_str()).collect();
    assert_eq!(names, vec!["braid-aaa", "braid-bbb"]); // basenames; bare `braid-` excluded
    ```
    (`cli/src/types.rs#MapperName::as_str` returns the basename, e.g.
    `braid-aaa`, not the `/dev/mapper/...` path.)
  - **Pass 1 label (behavioral)** -- prove `cli/src/lock.rs#build_close_sets_full`
    routes the Pass 1 orphan label through `orphan_disk_label` and never renders
    blank. Mirror `full_arm_pass1_unknown_uuid_classifies_as_orphan_and_warns`:
    build a `PoolState` with a `PoolDevice { mapper: "braid-", luks_uuid:
    <non-member> }`, call `build_close_sets_full(&runner, &fs, &pool, &membership,
    &mut acc)`, and assert `orphan_summaries(&close_set)` contains
    `("braid-".to_owned(), "braid-".to_owned())` -- mapper basename plus the
    non-blank full-name label.
  - **Pass 2 label (behavioral)** -- same for the `pool.null_underlying` path.
    Mirror `full_arm_pass2_null_underlying_unknown_devid_classifies_as_orphan_and_warns`
    using `synthetic_pool_state_with_null_underlying("braid-aaa", "braid-",
    Devid::new(99))` (a devid absent from membership), and assert
    `orphan_summaries(&close_set)` contains `("braid-".to_owned(),
    "braid-".to_owned())`.

The Pass 1/2 tests are behavioral -- they drive `build_close_sets_full`, not the
private helper in isolation -- so they prove the reroute is actually wired at
both label sites; a passing helper-only test could not. With the candidacy fix a
bare `braid-` can no longer reach the Pass 3 scan path, so the non-blank-label
guarantee is exercised exactly where it stays reachable. No end-to-end
`plan_lock` test is needed.

## Out of scope (no change required) -- with rationale

- **`cli/src/discover.rs`**: keeps using `name_from_mapper` + `DiskName::parse`;
  its bare-`braid-` -> `InvalidDiskName` warning is intentional and must stay.
- **VM tests** (`tests/cli/braid-lock-orphan.py`,
  `tests/module/lock-tolerates-missing-pool-json.py`): exercise *named* orphans
  (`braid-orphan`), unaffected by an empty-suffix fix. No new VM test -- the
  edge is operator-created and fully covered by the Rust unit tests.
- **Docs** (`docs/commands/lock.md`, ADR 024): describe scanning named
  `braid-*` mappers; the fix reinforces ADR 024's candidacy-vs-identity boundary
  rather than changing it. No doc edit; this is not an invariant change.

## Verification

```sh
just test-rust                        # full cli crate suite; justfile prefers this over `cargo test -p`
cargo test -p braid-cli lock::tests   # targeted: new filter + Pass 1/2 label tests
cargo test -p braid-cli config::tests # targeted: name_from_mapper / braid_disk_name
just clippy                           # cargo clippy --manifest-path cli/Cargo.toml --tests
cargo fmt --check
python3 scripts/docs/check-output-ascii.py   # `braid-` label is ASCII; sanity
```

Expected: new tests fail before the `lock.rs`/`config.rs` edits (write them
first, TDD per AGENTS.md), pass after; all existing orphan tests stay green
(non-empty suffixes are behavior-preserved).
