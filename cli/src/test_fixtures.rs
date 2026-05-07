//! Test-only shared fixtures for `replace` and `add` (and, in follow-ups,
//! `remove`, `remove_missing`, `recover`, `doctor`).
//!
//! These fixtures consolidate the per-test scaffolding that previously
//! lived as one-off `*Runner` structs and inline `tempdir + config + pass +
//! membership` setups. The split is:
//!
//!   * `MockFs` -- generic `Filesystem` mock with the canonical
//!     `/proc/self/mountinfo` body and an optional sysfs override.
//!   * `ReplacementPool` -- canonical pool-topology mock-handler
//!     installer for `replace` (mapper -> dev, dev -> uuid, btrfs
//!     filesystem show / usage with state flipping on `replace_done`,
//!     plus the boring preflight surface).
//!   * `AddTopology` -- canonical static one-disk pool topology installer
//!     for `add` tests that exercise the live-pool returning-disk surface.
//!   * `AddStatefulPool` + `AddPoolHandle` + `AddDynFs` -- stateful
//!     bootstrap+live mutation lifecycle installer for `add` tests that
//!     observe mount/device-add commits and per-mapper opens.
//!   * `AddPlanTopology` -- `plan_add` boundary topology with
//!     parameterised keyfile-probe responses and missing-device count.
//!   * `PoolFixture` -- bundled tempdirs + `StatePaths` + config +
//!     passphrase + `RecordingInhibitor`.
//!   * `ReplaceParamsBuilder` / `AddParamsBuilder` -- per-test builders
//!     over the `ReplaceParams` / `AddParams` defaults.
//!
//! Layout: this file is a facade. `shared` holds cross-scope items;
//! `replace` and `add` hold their per-scope topologies, builders, and
//! `PoolFixture` constructors.

mod add;
mod replace;
mod shared;

pub(crate) use replace::ReplacementPool;
pub(crate) use shared::{MockFs, PoolFixture, mock_ok};
