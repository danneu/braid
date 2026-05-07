# Split `cli/src/test_fixtures.rs` into facade + per-scope submodules

## Context

`cli/src/test_fixtures.rs` is ~1340 lines and growing. Recent commits (`361245a`, `f313dc0`, `7d5739b`) extended it for the in-progress `add` migration; follow-ups for `remove`, `recover`, `doctor` will add more. Without a split it becomes a mixed-purpose blob.

This commit does only the structural refactor -- a mechanical move into a facade plus per-scope submodules. No behavior changes, no fixture-data changes, no production-code changes, no `add.rs` test migration.

Sole current consumer: `cli/src/replace.rs:2549`:

```rust
use crate::test_fixtures::{MockFs, PoolFixture, ReplacementPool, mock_ok};
```

That import line stays exactly as-is after the refactor.

## Target layout

```
cli/src/
  test_fixtures.rs           # rewritten in place: facade only (mod decls + re-exports)
  test_fixtures/
    shared.rs                # NEW: mock_ok, MockFs, PoolFixture struct + cross-scope ctors
    replace.rs               # NEW: ReplacementPool, ReplaceParamsBuilder, replace `impl PoolFixture`
    add.rs                   # NEW: existing add-specific items (relocation, no new add fixtures)
```

`lib.rs:50-51` (`#[cfg(test)] pub(crate) mod test_fixtures;`) is unchanged. Rust resolves `mod test_fixtures;` to either `test_fixtures.rs` or `test_fixtures/mod.rs`; with both `test_fixtures.rs` and `test_fixtures/` present it uses the file as the module root and the directory for submodules. No `mod.rs` needed. `#[cfg(test)]` propagates to all submodules.

## Facade contents (new `cli/src/test_fixtures.rs`)

```rust
//! <preserve current lines 1-24 module-root //! header verbatim>
//!
//! Layout: this file is a facade. `shared` holds cross-scope items;
//! `replace` and `add` hold their per-scope topologies, builders, and
//! `PoolFixture` constructors.

mod add;
mod replace;
mod shared;

pub(crate) use shared::{MockFs, PoolFixture, mock_ok};
pub(crate) use replace::ReplacementPool;
```

`mod add;` is declared (default visibility, not `pub`) so the compiler type-checks `add.rs` even though no consumer imports it yet -- prevents bit-rot. Per-item `#[allow(dead_code)]` keeps unused-warnings silent. No re-exports from `add` until the add-test migration commit needs them.

## What moves where (line ranges from current `test_fixtures.rs`)

### `cli/src/test_fixtures/shared.rs`

- `mock_ok` (43-53)
- `MockFs` struct + inherent impl + `Filesystem` impl (55-119)
- `PoolFixture` struct (340-347)
- `impl PoolFixture { empty_inner, two_disk_healthy, one_live_one_missing, empty }` (352-365, 367-389, 391-413, 459-472)

### `cli/src/test_fixtures/replace.rs`

- Six replace-specific consts (125-171)
- `ReplacementPool` struct + impl (173-330)
- `impl PoolFixture { one_live_only, replace_params }` (442-457, 477-493) -- second inherent impl block on `PoolFixture`
- `ReplaceParamsBuilder` struct + impl (520-606)

### `cli/src/test_fixtures/add.rs`

- `impl PoolFixture { live_one_disk, add_params }` (415-437, 495-517) -- third inherent impl block; both methods keep `#[allow(dead_code)]`
- `AddParamsBuilder` struct + impl (608-688) -- preserve struct-level + impl-level `#[allow(dead_code)]`
- `ADD_POOL_FSID` const (~697)
- `AddTopology` struct + impl (~700-844) -- preserve `#[allow(dead_code)]`
- `AddPoolMode` enum, `AddStatefulPool`, `AddPoolHandle`, `AddDynFs` + `Filesystem for AddDynFs` (~855-1136) -- preserve `#[allow(dead_code)]`
- Private helpers `mapper_devid`, `mapper_underlying`, `luks_uuid_for_underlying` (~1138-1163)
- `AddPlanKeyfileProbe`, `AddPlanTopology` + impl (~1174-1336) -- preserve `#[allow(dead_code)]`
- Private helper `pool_underlying_for_index` (~1338-1340)

Rust permits multiple inherent `impl` blocks on the same type across files in the same crate; rust-analyzer follows them all.

## Visibility scheme

Strictly mechanical: every item that is `pub(crate)` today stays `pub(crate)` at its new declaration site. Three widenings, all `pub(in crate::test_fixtures)`, are required so the sibling-submodule `impl PoolFixture` blocks in `replace.rs` (`one_live_only`) and `add.rs` (`live_one_disk`) can call/construct from outside `shared`:

1. `PoolFixture::empty_inner` (associated fn) -- so sibling submodules can call it.
2. `PoolFixture::_state_tmp` (RAII field) -- so sibling-submodule constructors can populate `Self { _state_tmp: ..., ... }`.
3. `PoolFixture::_config_tmp` (RAII field) -- ditto.

All other defaults-private helpers stay default-private.

Two reasons to keep `pub(crate)` at the source rather than narrowing to `pub(in crate::test_fixtures)`:

1. **`pub(crate) use` cannot re-export a `pub(in crate::test_fixtures)` item.** A re-export's visibility cannot exceed the source visibility -- an empirical `rustc` probe of this exact pattern returns `E0364: ... is private, and cannot be re-exported`.
2. **`replace.rs` tests pervasively access `PoolFixture` non-RAII fields directly** (`f.paths` and `f.inhibitor` at lines 2597, 2605, 2661, 2666, 2697, 2702, 2782, 2785, 2802, 2925, 2930, 3253, 3257, 3300, 3304, 3400, 3404, 3717, 3720, 3735, 3819, 3904, 4330). Narrowing those fields would force every one of those access sites to change.

Privacy tightening (narrowing facade-re-exported items, narrowing `PoolFixture` fields, narrowing `ADD_POOL_FSID` etc.) is deferred to a separate follow-up commit. This commit's scope is "no behavior changes, no privacy changes".

| Item | Declaration | Re-exported `pub(crate)` from facade? |
| --- | --- | --- |
| `mock_ok` (fn) | `pub(crate)` (unchanged) | yes |
| `MockFs` (struct) and methods | `pub(crate)` (unchanged); `unmounted` keeps `#[allow(dead_code)]` | yes |
| `PoolFixture` (struct) | `pub(crate)` (unchanged) | yes |
| `PoolFixture` non-RAII fields (`paths`, `config_path`, `pass_path`, `inhibitor`) | `pub(crate)` (unchanged) -- accessed directly by tests | n/a |
| `PoolFixture` RAII fields (`_state_tmp`, `_config_tmp`) | `pub(in crate::test_fixtures)` (was default-private; widened so sibling-submodule `impl PoolFixture` blocks can populate them when constructing `Self { ... }`) | n/a |
| `PoolFixture::empty_inner` | `pub(in crate::test_fixtures)` (was default-private; widened so sibling-submodule impls can call it) | n/a |
| `PoolFixture::two_disk_healthy / one_live_one_missing / empty / live_one_disk / one_live_only / replace_params / add_params` | `pub(crate)` (unchanged); `live_one_disk` and `add_params` keep `#[allow(dead_code)]` | n/a |
| `ReplacementPool` (struct + methods) | `pub(crate)` (unchanged) | yes (the type) |
| `ReplacementPool::canonical_mapper_to_dev / canonical_dev_to_uuid` and the six replace consts | default-private (unchanged) | n/a |
| `ReplaceParamsBuilder<'a>` + methods | `pub(crate)` (unchanged); `passphrase_stdin` and `progress` keep `#[allow(dead_code)]` | no -- accessed only via method-chain return type, not by name |
| `AddParamsBuilder<'a>` + methods | `pub(crate)` (unchanged); struct + impl keep `#[allow(dead_code)]` | no |
| `ADD_POOL_FSID` | `pub(crate)` (unchanged) | no |
| `AddTopology / AddPoolMode / AddStatefulPool / AddPoolHandle / AddDynFs / AddPlanKeyfileProbe / AddPlanTopology` | `pub(crate)` (unchanged); each keeps existing per-item `#[allow(dead_code)]` | no in this commit; future facade re-exports added when add-test migration consumes them |
| `mapper_devid / mapper_underlying / luks_uuid_for_underlying / pool_underlying_for_index` | default-private (unchanged) | n/a |

No `#![allow(dead_code)]` on any submodule. All preservation is per-item.

## Imports per submodule

### `shared.rs`

```rust
use crate::cmd::RawCommandOutput;
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::path::PathBuf;
use tempfile::TempDir;
```

### `replace.rs`

```rust
use super::shared::{PoolFixture, mock_ok};
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::progress::ProgressOutput;
use crate::replace::ReplaceParams;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;
```

### `add.rs`

```rust
use super::shared::{PoolFixture, mock_ok};
use crate::add::AddParams;
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::inhibit::RecordingInhibitor;
use crate::luks::{PassphraseReader, RealTty};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::probe::Filesystem;
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tempfile::TempDir;
```

Implementer should let `cargo check` confirm no unused imports; trim if any submodule's actual use is narrower than listed.

## Critical files

- `/Users/dan/Code/braid/cli/src/test_fixtures.rs` -- rewritten in place to the facade
- `/Users/dan/Code/braid/cli/src/test_fixtures/shared.rs` -- new
- `/Users/dan/Code/braid/cli/src/test_fixtures/replace.rs` -- new
- `/Users/dan/Code/braid/cli/src/test_fixtures/add.rs` -- new
- `/Users/dan/Code/braid/cli/src/replace.rs` -- unchanged; line 2549 import resolves through the facade re-exports
- `/Users/dan/Code/braid/cli/src/lib.rs` -- unchanged

## Implementation order

1. Create `cli/src/test_fixtures/` directory.
2. Create `cli/src/test_fixtures/shared.rs` with the content above.
3. Create `cli/src/test_fixtures/replace.rs` with the content above.
4. Create `cli/src/test_fixtures/add.rs` with the content above.
5. Rewrite `cli/src/test_fixtures.rs` in place to the facade content.
6. Run `cargo check --manifest-path cli/Cargo.toml --tests`. The most likely diagnostic if anything goes wrong is "function `empty_inner` is private" from the sibling-submodule `impl PoolFixture` blocks -- fix by ensuring the widening to `pub(in crate::test_fixtures)` is applied.
7. Run `cargo test --manifest-path cli/Cargo.toml --lib replace::tests`. Pass count must match pre-refactor.
8. Run `just test-rust`.

## Verification

- `cargo check --manifest-path cli/Cargo.toml --tests` -- compiles all test code; no new warnings (existing `#[allow(dead_code)]` set keeps unused-warnings silent for the unmigrated add items)
- `cargo test --manifest-path cli/Cargo.toml --lib replace::tests` -- replace unit tests pass identically pre-/post-refactor
- `just test-rust` -- full rust unit test target

Acceptance: all three exit 0 with no new warnings, and `git diff cli/src/replace.rs cli/src/lib.rs` shows zero changes.

## Risks / gotchas

- **Multiple inherent `impl PoolFixture` blocks across three files.** Supported by Rust; no clippy lint forbids it; rust-analyzer follows them all.
- **`#[cfg(test)]` inheritance.** Parent module is test-gated at `lib.rs:50-51`; submodules inherit transitively. No per-file `#[cfg(test)]` needed.
- **`pub use` visibility ceiling.** A `pub(crate) use` cannot re-export an item that is only `pub(in crate::test_fixtures)` -- the empirical `rustc` error is `E0364`. Source declarations of facade-re-exported items must be at least `pub(crate)`. This is why the plan keeps source visibilities as-is rather than narrowing.
- **`PoolFixture::empty_inner` + RAII-field widenings.** Three visibility *changes* in the refactor, all `pub(in crate::test_fixtures)`: `empty_inner` (so sibling submodule impls can call it) plus `_state_tmp` and `_config_tmp` (so sibling submodule constructors can populate `Self { _state_tmp: ..., _config_tmp: ..., ... }`). All three were default-private. No re-exports needed -- none are public surface items.
- **`test_fixtures.rs` + `test_fixtures/` coexistence.** Standard modern layout; Rust resolves cleanly with no `mod.rs`.
- **Doc-comment preservation.** All `///` per-item docs travel verbatim with their items. The module-root `//!` header stays at the facade and gets a one-line layout note appended. Each submodule gets a one-line `//!` for navigability only.
- **No cycles.** `replace` and `add` import only from `super::shared`; `shared` imports from neither. Strict tree.
- **Privacy tightening deferred.** Narrowing facade-re-exported items to `pub(in crate::test_fixtures)`, tightening `PoolFixture` field visibility (which would require touching ~25 test sites that read `f.paths` / `f.inhibitor`), and narrowing `ADD_POOL_FSID` are all out of scope for this commit. Track separately if desired.
