//! Test-only shared fixtures for `replace`, `add`, `remove`,
//! `remove_missing`, and `recover` (and, in follow-ups, `doctor`).
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
//!   * `RemovalPool` -- canonical pool-topology mock-handler installer
//!     for `remove` tests that exercise 2->1 and 3->2 success paths.
//!   * `RemountHarness` -- promoted stateful-FS + mapper-closing-runner
//!     pair for `recover` tests that exercise the relock / remount cycle.
//!   * `PoolFixture` -- bundled tempdirs + `StatePaths` + config +
//!     passphrase + `RecordingInhibitor`.
//!   * `ReplaceParamsBuilder` / `AddParamsBuilder` / `RemoveParamsBuilder`
//!     / `RemoveMissingParamsBuilder` / `RecoverParamsBuilder` -- per-test
//!     builders over command defaults.
//!
//! Layout: this file is a facade. `shared` holds cross-scope items;
//! `replace`, `add`, `remove`, `remove_missing`, and `recover` hold their
//! per-scope topologies, builders, and `PoolFixture` constructors.

mod add;
mod recover;
mod remove;
mod remove_missing;
mod replace;
mod shared;

#[allow(unused_imports)]
pub(crate) use recover::{RecoverParamsBuilder, RemountHarness};
#[allow(unused_imports)]
pub(crate) use remove::{
    RemovalPool, RemoveParamsBuilder, target_device, valid_three_disk_df_json,
    valid_three_disk_usage_stdout, valid_two_disk_df_json, valid_two_disk_usage_stdout,
};
#[allow(unused_imports)]
pub(crate) use remove_missing::{RemoveMissingParamsBuilder, RemoveMissingPool};
pub(crate) use replace::ReplacementPool;
#[allow(unused_imports)]
pub(crate) use shared::{MockFs, PoolFixture, TEST_PASSPHRASE_BYTES, mock_ok};
