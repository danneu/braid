//! Discover-scope fixtures for `cli/src/discover.rs` tests.
//!
//! Discovery tests need real tempdirs and symlinks so `read_dir` and
//! `canonicalize` keep exercising the host filesystem behavior under test.
//! The runner stays label-driven because unknown devices must return
//! `Ok(exit=1)`, matching cryptsetup's "not LUKS" signal.

use super::shared::mock_ok;
use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

/// Label-driven discover runner whose unknown-device path mirrors cryptsetup.
///
/// Unknown devices return `Ok(exit=1)`, not `Err`, so the discovery tests keep
/// proving that the LUKS gate is based on process status rather than runner
/// failure. Calls are recorded for the non-LUKS gate regression test.
pub(crate) struct DiscoverLabelMap {
    labels: HashMap<String, String>,
    versions: HashMap<String, u32>,
    uuids: HashMap<String, String>,
    dump_responses: HashMap<String, RawCommandOutput>,
    calls: Mutex<Vec<(String, String)>>,
}

impl DiscoverLabelMap {
    /// Build a label map from `(device_path, full_luks_label)` entries.
    pub(crate) fn new(entries: &[(&str, &str)]) -> Self {
        Self {
            labels: entries
                .iter()
                .map(|(path, label)| (path.to_string(), label.to_string()))
                .collect(),
            versions: HashMap::new(),
            uuids: HashMap::new(),
            dump_responses: HashMap::new(),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Override the LUKS version for one path while defaulting all others to LUKS2.
    pub(crate) fn with_version(mut self, path: &str, version: u32) -> Self {
        self.versions.insert(path.to_string(), version);
        self
    }

    /// Set the LUKS UUID emitted in the synthetic luksDump body for one path.
    /// Test paths without a configured UUID default to a deterministic
    /// per-path UUID so the dump parser always sees one and discover's
    /// missing/invalid UUID handling can be exercised separately by
    /// `with_dump_response`.
    #[allow(dead_code)]
    pub(crate) fn with_uuid(mut self, path: &str, uuid: &str) -> Self {
        self.uuids.insert(path.to_string(), uuid.to_string());
        self
    }

    /// Override one luksDump response for tests that pin warning classification.
    pub(crate) fn with_dump_response(mut self, path: &str, response: RawCommandOutput) -> Self {
        self.dump_responses.insert(path.to_string(), response);
        self
    }

    /// Snapshot recorded `(command_label, device_path)` pairs in call order.
    pub(crate) fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().unwrap().clone()
    }
}

/// Stable deterministic synthetic UUID per device path so fixture tests
/// satisfy `parse_cryptsetup_luks_uuid_from_dump` without forcing every
/// test to pin a UUID. The mapping is content-addressed (hash of the
/// path bytes) so distinct paths produce distinct UUIDs. The last
/// block is 12 hex digits (canonical hyphenated form); the leading
/// blocks are zero so the fixture UUID space cannot collide with
/// production-shape UUIDs.
fn synthesize_uuid_for(path: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut h);
    let bits = h.finish() & 0xffff_ffff_ffff; // mask to 48 bits
    format!("00000000-0000-0000-0000-{:012x}", bits)
}

impl CommandRunner for DiscoverLabelMap {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::CryptsetupIsLuks { device } => {
                self.calls
                    .lock()
                    .unwrap()
                    .push(("isLuks".into(), device.clone()));
                if self.labels.contains_key(device.as_str()) {
                    Ok(mock_ok("cryptsetup", ""))
                } else {
                    Ok(RawCommandOutput {
                        cmd: "cryptsetup".into(),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_status: 1,
                    })
                }
            }
            CmdRequest::CryptsetupLuksDumpText { device } => {
                self.calls
                    .lock()
                    .unwrap()
                    .push(("luksDump".into(), device.clone()));
                if let Some(response) = self.dump_responses.get(device.as_str()) {
                    Ok(response.clone())
                } else if let Some(label) = self.labels.get(device.as_str()) {
                    let version = self.versions.get(device.as_str()).copied().unwrap_or(2);
                    let uuid = self
                        .uuids
                        .get(device.as_str())
                        .cloned()
                        .unwrap_or_else(|| synthesize_uuid_for(device));
                    Ok(mock_ok(
                        "cryptsetup",
                        &format!(
                            "LUKS header information\nVersion:\t{version}\nUUID:\t{uuid}\nLabel:\t{label}\n"
                        ),
                    ))
                } else {
                    Ok(RawCommandOutput {
                        cmd: "cryptsetup".into(),
                        stdout: "Device /dev/foo is not a valid LUKS device.\n".into(),
                        stderr: String::new(),
                        exit_status: 1,
                    })
                }
            }
            _ => Err(CmdError::MissingMock),
        }
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        self.run(request)
    }
}

/// Create a real placeholder file so by-id symlinks canonicalize to a target.
pub(crate) fn discover_create_target(dir: &Path, name: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, b"").unwrap();
    path.to_string_lossy().into_owned()
}

/// Create a real by-id symlink so discovery keeps testing filesystem behavior.
pub(crate) fn discover_create_by_id_symlink(dir: &Path, name: &str, target: &str) -> String {
    let path = dir.join(name);
    std::os::unix::fs::symlink(target, &path).unwrap();
    path.to_string_lossy().into_owned()
}
