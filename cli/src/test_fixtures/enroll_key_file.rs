//! Enroll-key-file scope fixtures: cross-test scaffolding for
//! `cli/src/enroll_key_file.rs`'s `mod tests`.
//!
//! Enroll is mutating-command oriented but the test surface is dominated
//! by per-test request-set composition and load-bearing missing-mock
//! contracts (eleven tests pin exact `runner.requests()` orderings or
//! deliberately omit a probe to drive a `MissingMock`). So this module
//! ships as a flat collection of leaf helpers like `test_fixtures::mount`,
//! not the `*Pool` + `*ParamsBuilder` triad that `add` / `remove` /
//! `replace` ship.
//!
//! Three intentional omissions, all load-bearing:
//!
//!   * No `EnrollPool` / `EnrollTopology` handler installer. A broad
//!     `with_handler` would resolve the deliberate `MissingMock` probes
//!     ten enroll tests rely on and silently invert their assertions.
//!   * No `EnrollKeyFileParamsBuilder`. Many enroll tests build an
//!     `EnrollKeyFileParams` inline. The `plan_enroll` planner cohort
//!     sets `passphrase_stdin: false` / `passphrase_file: None`
//!     uniformly and varies only generate / dry_run / membership /
//!     keyfile / paths; inline literals keep each test's planning
//!     inputs explicit at the callsite, which a positional builder
//!     would obscure.
//!   * No base preflight runner analogous to mount's
//!     `base_two_disk_runner`. Enroll tests vary per-disk outcomes
//!     combinatorially (verify_pass / verify_fail x keyfile_ok /
//!     keyfile_fail x slot1_empty / slot1_occupied), and the
//!     `CryptsetupTestKeyFile` request key includes the keyfile path,
//!     which differs per test -- override-on-base would have to match
//!     a compound key that shifts per scenario. Cleaner to keep the
//!     leaf factories.
//!
//! Naming: every newly-exported helper carries an `enroll_` prefix. Two
//! distinct pressures enforce the convention:
//!
//!   1. The facade already exports `mountpoint_ok` / `mountpoint_fail`
//!      from `doctor` and `err_raw` / `luks_uuid_ok` / `test_passphrase_fail`
//!      / `ok_raw` from `mount`. Unprefixed enroll helpers with the same
//!      names would collide at the facade.
//!   2. During the staged migration (sub-commits 2-4), each migrated
//!      test's `use crate::test_fixtures::...;` line lands while the
//!      same-named local helpers still exist in `enroll_key_file.rs::tests`
//!      for tests that have not yet been migrated. Rust treats `use foo::bar;`
//!      plus a same-named local `fn bar` in the same module as a
//!      duplicate-definition error. Prefixing avoids the collision.
//!
//! Reuse via the existing facade exports (no new declarations):
//!
//!   * `mock_ok(cmd, stdout)` from `shared` -- byte-identical to
//!     enroll's local `ok_raw`. Imported bare; no alias needed because
//!     the local is named `ok_raw`.
//!   * `isolated_paths()` from `doctor` -- byte-identical to enroll's
//!     local `test_paths`. Imported bare; no alias needed because the
//!     local is named `test_paths`.
//!   * `err_raw` from `mount` -- byte-identical to enroll's local
//!     `err_raw`. Imported under the alias `err_raw as enroll_err_raw`
//!     because the local is also named `err_raw` and a bare `use`
//!     would collide during sub-commits 2-4. The alias stays after
//!     sub-commit 5; do not "simplify" it back to a bare `err_raw`.

use super::mount::err_raw;
use super::shared;
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::luks::KEYFILE_SIZE;
use crate::membership::{DiskMember, PoolMembership};
use crate::secret::Passphrase;
use crate::types::{ByIdPath, DiskName, MountPoint};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Thin wrapper around `shared::MockFs::unmounted` that takes a `&[&str]`
/// for ergonomic per-test seeding. Centralises the `&str -> String`
/// conversion so every test-mod call site stays one line.
///
/// Safe to use the shared "unmounted" mountinfo body because the enroll
/// call graph (`enroll_key_file.rs`, `probe.rs`, `luks.rs`,
/// `credential_verify.rs`) never calls `fs.read_to_string`,
/// `fs.is_block_device`, or `fs.list_dir` -- only `fs.exists`. Verified
/// by grep over those four files at module-introduction time; if a
/// future change adds one of those calls, the shared body's "/" rootfs
/// answer is still correct for the "pool not yet mounted" scenario every
/// enroll test models.
pub(crate) fn enroll_fs(paths: &[&str]) -> shared::MockFs {
    shared::MockFs::unmounted(paths.iter().map(|p| (*p).to_string()).collect())
}

// ---------------------------------------------------------------------------
// Identifier / credential primitives
// ---------------------------------------------------------------------------

pub(crate) fn enroll_by_id(path: &str) -> ByIdPath {
    ByIdPath::parse(path).unwrap()
}

pub(crate) fn enroll_passphrase(s: &str) -> Passphrase {
    Passphrase::from_zeroizing(zeroize::Zeroizing::new(s.to_owned()))
}

// ---------------------------------------------------------------------------
// (CmdRequest, RawCommandOutput) factories
// ---------------------------------------------------------------------------
//
// Each pair-returning helper returns the JSON / canonical-UUID shapes
// enroll tests rely on. Distinct from `mount`'s same-keyword helpers
// (different signatures, or different request variants), so the facade
// keeps both sets reachable via the `enroll_` prefix.

/// Returns the supplied canonical UUID for a device. Distinct from
/// `mount::luks_uuid_ok` by prefix so both helpers stay reachable
/// through the same facade.
pub(crate) fn enroll_luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksUuid {
            device: device.to_owned(),
        },
        shared::mock_ok(
            &format!("cryptsetup luksUUID {device}"),
            &format!("{uuid}\n"),
        ),
    )
}

pub(crate) fn enroll_luks_uuid_not_luks(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksUuid {
            device: device.to_owned(),
        },
        err_raw(
            &format!("cryptsetup luksUUID {device}"),
            4,
            "Device is not a valid LUKS device.",
        ),
    )
}

/// `cryptsetup luksDump` (JSON variant) for a header with only slot 0
/// occupied. Drives `check_key_slot`'s empty/occupied branch -- slot 1
/// empty means ready to enroll a keyfile. Distinct from `mount`'s
/// `luks_dump_text_*` (text variant) which drives the
/// `LuksHeaderState` gateway, not slot inventory.
pub(crate) fn enroll_luks_dump_slot1_empty(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksDump {
            device: device.to_owned(),
        },
        shared::mock_ok(
            &format!("cryptsetup luksDump {device}"),
            r#"{"keyslots":{"0":{"type":"luks2"}}}"#,
        ),
    )
}

pub(crate) fn enroll_luks_dump_slot1_occupied(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksDump {
            device: device.to_owned(),
        },
        shared::mock_ok(
            &format!("cryptsetup luksDump {device}"),
            r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
        ),
    )
}

pub(crate) fn enroll_test_keyfile_ok(
    device: &str,
    key_file: &str,
) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupTestKeyFile {
            device: device.to_owned(),
            key_file_path: key_file.to_owned(),
        },
        shared::mock_ok("cryptsetup open --test-passphrase --key-file", ""),
    )
}

pub(crate) fn enroll_test_keyfile_fail(
    device: &str,
    key_file: &str,
) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupTestKeyFile {
            device: device.to_owned(),
            key_file_path: key_file.to_owned(),
        },
        err_raw("cryptsetup open --test-passphrase --key-file", 2, "No key"),
    )
}

/// Takes the per-test target dir (e.g. `/mnt/usb`), not the canonical
/// `/mnt/storage` that `doctor::mountpoint_ok` hardcodes -- so doctor's
/// helper is not a drop-in. The direct collision with the
/// already-exported `test_fixtures::mountpoint_ok` from `doctor` is the
/// load-bearing reason for the `enroll_` prefix on this name.
pub(crate) fn enroll_mountpoint_ok(dir: &Path) -> (CmdRequest, RawCommandOutput) {
    let dir = dir.display().to_string();
    (
        CmdRequest::MountpointCheck {
            path: MountPoint(dir.clone()),
        },
        shared::mock_ok(&format!("mountpoint -q {dir}"), ""),
    )
}

pub(crate) fn enroll_mountpoint_fail(dir: &Path) -> (CmdRequest, RawCommandOutput) {
    let dir = dir.display().to_string();
    (
        CmdRequest::MountpointCheck {
            path: MountPoint(dir.clone()),
        },
        err_raw(&format!("mountpoint -q {dir}"), 1, ""),
    )
}

// ---------------------------------------------------------------------------
// (CmdRequest, Vec<u8>, RawCommandOutput) triple factories
// ---------------------------------------------------------------------------
//
// Triple shape -- not pair -- because enroll tests vary the passphrase
// bytes per scenario (`testpass`, `wrongpass`, `pass-disk2`, ...) and
// ~20 call sites destructure into `(req, stdin, out)`. Distinct from
// mount's pair-shaped `test_passphrase_fail`, which pairs with the
// `MOUNT_TEST_PASSPHRASE_BYTES` constant at the call site.
// `with_output_stdin(req, stdin, out)` consumes the triple verbatim.

pub(crate) fn enroll_test_passphrase_ok(
    device: &str,
    passphrase: &str,
) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
    (
        CmdRequest::CryptsetupTestPassphrase {
            device: device.to_owned(),
        },
        passphrase.as_bytes().to_vec(),
        shared::mock_ok(&format!("cryptsetup open --test-passphrase {device}"), ""),
    )
}

pub(crate) fn enroll_test_passphrase_fail(
    device: &str,
    passphrase: &str,
) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
    (
        CmdRequest::CryptsetupTestPassphrase {
            device: device.to_owned(),
        },
        passphrase.as_bytes().to_vec(),
        err_raw(
            &format!("cryptsetup open --test-passphrase {device}"),
            2,
            "No key available with this passphrase.",
        ),
    )
}

/// `CryptsetupLuksAddKeyFile` triple for `apply_*` tests. Renamed from
/// the local `enroll_ok` to make the cryptsetup operation explicit --
/// the local name reads as "enroll succeeded" but the helper actually
/// mocks `cryptsetup luksAddKey`.
pub(crate) fn enroll_add_keyfile_ok(
    device: &str,
    key_file: &str,
    passphrase: &str,
) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksAddKeyFile {
            device: device.to_owned(),
            key_file_path: key_file.to_owned(),
        },
        passphrase.as_bytes().to_vec(),
        shared::mock_ok("cryptsetup luksAddKey", ""),
    )
}

/// Failing `CryptsetupLuksAddKeyFile` triple for apply-path tests that
/// need a post-preflight mutation failure after earlier disks changed.
pub(crate) fn enroll_add_keyfile_fail(
    device: &str,
    key_file: &str,
    passphrase: &str,
) -> (CmdRequest, Vec<u8>, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksAddKeyFile {
            device: device.to_owned(),
            key_file_path: key_file.to_owned(),
        },
        passphrase.as_bytes().to_vec(),
        err_raw("cryptsetup luksAddKey", 5, "Device or resource busy"),
    )
}

// ---------------------------------------------------------------------------
// Runner-chaining wrappers
// ---------------------------------------------------------------------------

pub(crate) fn enroll_with_mountpoint_ok(runner: MockRunner, dir: &Path) -> MockRunner {
    let (req, out) = enroll_mountpoint_ok(dir);
    runner.with_output(req, out)
}

pub(crate) fn enroll_with_mountpoint_fail(runner: MockRunner, dir: &Path) -> MockRunner {
    let (req, out) = enroll_mountpoint_fail(dir);
    runner.with_output(req, out)
}

// ---------------------------------------------------------------------------
// Membership / keyfile composers
// ---------------------------------------------------------------------------

/// `BTreeMap`-of-`DiskMember` from `(name, by-id-path)` pairs. Distinct
/// from `mount::two_disk_membership` / `three_disk_membership`, which
/// hardcode `virtio-diskN`; enroll tests use short by-id strings (`d1`,
/// `d2`) and arbitrary disk names so a parameterised builder is the
/// right shape.
pub(crate) fn enroll_make_membership(disks: &[(&str, &str)]) -> PoolMembership {
    let mut membership = PoolMembership::empty();
    for (idx, (key, path)) in disks.iter().enumerate() {
        let member = DiskMember {
            name: DiskName::parse(key).expect("valid enroll fixture disk name"),
            by_id: enroll_by_id(path),
            devid: None,
            added_at: None,
        };
        membership
            .insert(shared::test_uuid(500 + idx as u64), member)
            .expect("insert enroll fixture member");
    }
    membership
}

/// Writes `KEYFILE_SIZE` zero bytes to `<tmp>/braid.key`. Returns
/// `(path, str)` because most call sites need the display string for
/// `CryptsetupTestKeyFile { key_file_path, .. }` seeding.
pub(crate) fn enroll_make_existing_keyfile(tmp: &TempDir) -> (PathBuf, String) {
    let kf = tmp.path().join("braid.key");
    std::fs::write(&kf, vec![0u8; KEYFILE_SIZE]).unwrap();
    let kf_str = kf.display().to_string();
    (kf, kf_str)
}

// ---------------------------------------------------------------------------
// Composite preflight runner
// ---------------------------------------------------------------------------

/// Full `plan_enroll` discovery setup for a 2-disk pool of present LUKS
/// disks reachable through `probe_config_disk`: `luksUUID` ok for both,
/// the `luksDump` text gateway for both, and `braid-disk1` /
/// `braid-disk2` mappers reported closed. Used by the dry-run probe
/// family (5 tests) and the existing-keyfile-validation family
/// (1 test) -- 6 known consumers justify promoting unchanged.
///
/// What this runner does NOT seed -- by design:
///
///   * `CryptsetupIsLuks`. Not on `plan_enroll`'s discovery path; if
///     a future change adds the probe, consumer tests will surface
///     `MissingMock` and fail loudly rather than masking the drift.
///   * `CryptsetupLuksDump` (the JSON variant -- the slot-1 inventory
///     dump that drives keyfile-conflict detection). Per-test seeding
///     keeps that contract visible at the call site instead of hiding
///     it behind the composite.
///   * `CryptsetupTestPassphrase` / `CryptsetupTestKeyFile`. Each
///     test layers its own per-disk verify outcome with the correct
///     passphrase or keyfile path.
pub(crate) fn enroll_discovery_two_disks(d1: &str, d2: &str) -> MockRunner {
    let (req1, out1) = enroll_luks_uuid_ok(d1, shared::test_uuid(500).as_str());
    let (req2, out2) = enroll_luks_uuid_ok(d2, shared::test_uuid(501).as_str());
    MockRunner::default()
        .with_output(req1, out1)
        .with_output(req2, out2)
        .with_luks_dump_text_luks2_for(&[d1, d2])
        .with_mappers_closed(&["braid-disk1", "braid-disk2"])
}
