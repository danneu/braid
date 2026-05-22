use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::alert::AlertState;
use crate::parse::types::{BtrfsDfEntry, DeviceAllocation, ScrubState, SmartHealth, UpsStatusFlag};
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, DiskErrors};
use crate::tui::browse::BrowseState;
use crate::tui::effect::Effect;
use crate::types::{ByIdPath, LuksUuid, MountPoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Data,
    Scrub,
    Browse,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Data, Tab::Scrub, Tab::Browse];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Data => "Data",
            Tab::Scrub => "Scrub",
            Tab::Browse => "Browse",
        }
    }

    pub fn next(self) -> Tab {
        match self {
            Tab::Data => Tab::Scrub,
            Tab::Scrub => Tab::Browse,
            Tab::Browse => Tab::Data,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Data => Tab::Browse,
            Tab::Scrub => Tab::Data,
            Tab::Browse => Tab::Scrub,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskLuksInfo {
    pub cipher: String,
    pub key_size_bits: u32,
    pub keyslot_count: u32,
}

/// Membership-derived disk identity bundled as one value so the TUI model and
/// probe effect share a single source of truth instead of four parallel maps.
/// All fields are name-keyed; `names` carries display order from
/// `membership.iter_by_name()`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiskIdentity {
    pub names: Vec<String>,
    pub by_id: HashMap<String, String>,
    pub luks_uuid: HashMap<String, LuksUuid>,
    /// Persistent btrfs devid bindings, used when a live probe cannot observe
    /// the underlying LUKS UUID for a mounted device.
    pub devid: HashMap<String, u64>,
}

impl DiskIdentity {
    /// Build the TUI's name-keyed view of pool membership at session start.
    pub fn from_membership(m: &crate::membership::PoolMembership) -> Self {
        let members = m.iter_by_name();
        let names: Vec<String> = members
            .iter()
            .map(|(_, member)| member.name.as_str().to_owned())
            .collect();
        let by_id: HashMap<String, String> = members
            .iter()
            .map(|(_, member)| (member.name.as_str().to_owned(), member.by_id.to_string()))
            .collect();
        let luks_uuid: HashMap<String, LuksUuid> = members
            .iter()
            .map(|(uuid, member)| (member.name.as_str().to_owned(), (*uuid).clone()))
            .collect();
        let devid: HashMap<String, u64> = members
            .iter()
            .filter_map(|(_, member)| {
                member
                    .devid
                    .map(|devid| (member.name.as_str().to_owned(), devid))
            })
            .collect();
        Self {
            names,
            by_id,
            luks_uuid,
            devid,
        }
    }
}

/// Per-declared-disk lock state surfaced independently of pool mount
/// status so disk detail can stay truthful while the pool is offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskLockState {
    Unlocked,
    Locked,
    /// Probe failure or failed mapper ownership confirmation.
    Unknown,
}

/// Mount-independent LUKS snapshot for one declared disk. Kept on
/// `Model`, not `PoolState`, because cryptsetup state exists even when
/// btrfs cannot mount.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskLuksState {
    pub lock: DiskLockState,
    /// Backing block device from `cryptsetup status`. `None` means either
    /// the mapper is closed or an open mapper reports no backing device.
    pub underlying_present: Option<String>,
    pub metadata: Option<DiskLuksInfo>,
}

/// Raw chassis fan telemetry from sysfs. `pwm_raw` is the PWM register value
/// (0-255); `rpm` is the latest `fanN_input` tachometer reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FanReading {
    pub pwm_raw: u8,
    pub rpm: u32,
}

/// Hottest drivetemp-reporting SATA drive on the system, as the TUI's
/// best-effort approximation of hddfancontrol's `-d ata` selector. The
/// daemon's actual selected set is authoritative; `DaemonStatus` is the
/// source of truth for whether the control loop is live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivingDrive {
    pub label: String,
    pub celsius: i16,
}

/// Live state of `hddfancontrol-braid.service` as reported by
/// `systemctl show -P ActiveState`. Sensor readings are still meaningful when
/// the daemon is not `Active`, but the control loop isn't acting on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Active,
    /// `activating`, `reloading`, or `deactivating`.
    Transitioning,
    Inactive,
    Failed,
    /// Output from `systemctl show -P ActiveState` didn't match any known state,
    /// or the command itself failed to spawn.
    Unknown,
}

/// Snapshot of the fan control subsystem — produced on every fan probe.
#[derive(Debug, Clone)]
pub struct FanSnapshot {
    pub fan: Option<FanReading>,
    pub driving: Option<DrivingDrive>,
    pub daemon: DaemonStatus,
    pub probed_at: Instant,
}

/// Snapshot of UPS state for the TUI -- produced on every UPS probe.
///
/// Distinct from `UpscOutput`: the TUI only needs the fields the
/// section actually renders (status flags, charge, runtime, load,
/// watts estimate, daemon state), so we keep the Model light. The
/// conversion from `UpscOutput` -> `UpsSnapshot` lives in
/// `tui::probe::probe_ups_for_tui`, the single authoritative bridge.
#[derive(Debug, Clone)]
pub struct UpsSnapshot {
    pub flags: Vec<UpsStatusFlag>,
    pub battery_charge_pct: Option<u8>,
    pub runtime_secs: Option<u32>,
    pub load_pct: Option<u8>,
    /// Only set when both `ups.load` and `ups.realpower.nominal` are
    /// available. When `None`, the view omits the watts annotation
    /// entirely rather than guessing.
    pub watts_estimated: Option<u32>,
    /// Raw `upsc <name>` stdout captured for the Browse tab's Variables
    /// view without widening the parsed `braid ups status --json` model.
    pub raw_text: String,
    pub daemon: DaemonStatus,
    pub probed_at: Instant,
}

/// Physical identity of a disk for session-scoped temperature tracking.
/// LUKS UUID is preferred so watermarks survive device-path changes on
/// unplug/replug; by-id path is a fallback for disks whose UUID isn't
/// available in the probe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TemperatureDiskId {
    LuksUuid(LuksUuid),
    ByIdPath(ByIdPath),
}

/// Current temperature reading for one disk, produced per probe tick.
/// `celsius` is signed because SMART can legitimately report sub-zero values.
#[derive(Debug, Clone)]
pub struct TemperatureReading {
    pub id: TemperatureDiskId,
    pub celsius: i16,
}

/// Session-scoped hi/lo watermarks for one disk. Reset via Shift+R.
/// No `last` field: the current value is always read from the latest
/// `PoolState` so a failed probe can't produce a stale current temp.
#[derive(Debug, Clone, Copy)]
pub struct TemperatureWatermark {
    pub min_celsius: i16,
    pub max_celsius: i16,
    pub sample_count: u32,
}

#[derive(Clone)]
pub struct DiskUsage {
    pub size: u64,
    pub allocations: Vec<DeviceAllocation>,
    pub unallocated: u64,
}

impl DiskUsage {
    pub fn allocated(&self) -> u64 {
        self.allocations.iter().map(|a| a.bytes).sum()
    }
}

/// Render classification for a declared disk that is NOT currently
/// represented in the live pool's `disk_usage`. Populated by `tui::probe`
/// from the read-only `probe_config_disk` result so the disk table can
/// distinguish unplugged, valid-but-unrelated, and broken-header states
/// instead of collapsing them all into a generic "missing".
///
/// Variants are deliberately prefixed with `LuksHeader` (not just
/// `Header`) to avoid ambiguity in the view layer — `Header` alone could
/// read as a btrfs header, an lsblk column header, etc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnpooledDiskRender {
    /// `ConfigDiskState::Absent` — device file does not exist.
    Missing,
    /// `ConfigDiskState::PresentLuks` whose UUID is not in the live pool.
    /// LUKS header is valid but the disk does not belong to this pool.
    UnknownLuks,
    /// `ConfigDiskState::PresentNotLuks` refined to
    /// `LuksHeaderState::Unreadable` (or fallback). Severe — needs
    /// off-system header backup restore.
    LuksHeaderUnreadable,
    /// `ConfigDiskState::PresentNotLuks` refined to
    /// `LuksHeaderState::Damaged`. Potentially repairable via
    /// `cryptsetup repair`.
    LuksHeaderDamaged,
    /// `probe_config_disk` returned `ProbeError::UnsupportedLuksVersion`.
    /// The disk is on-disk LUKS but the wrong version (LUKS1 — braid
    /// requires LUKS2). Recovery: back up data, re-add via `braid add`.
    WrongLuksVersion(u32),
    /// `probe_config_disk` found `braid-<DiskName>` open for the wrong
    /// backing device or LUKS UUID. Recovery for all ownership-conflict
    /// shapes is to close the mapper and unlock again; detailed
    /// expected/found data lives on the underlying `ProbeError`.
    MapperHijacked,
}

#[derive(Clone)]
pub struct PoolState {
    pub mount_point: MountPoint,
    pub df_entries: Vec<BtrfsDfEntry>,
    pub disk_usage: HashMap<String, DiskUsage>,
    pub disk_transport: HashMap<String, String>,
    pub smart_health: HashMap<String, SmartHealth>,
    pub disk_temperature_readings: HashMap<String, TemperatureReading>,
    pub device_errors: HashMap<String, DiskErrors>,
    /// Per-declared-disk render classification for disks NOT in
    /// `disk_usage`. Populated by `tui::probe` via `probe_config_disk`
    /// so the disk table can render Unreadable / Damaged / UnknownLuks /
    /// Missing distinctly. Disks present in `disk_usage` are omitted.
    pub unpooled_disks: HashMap<String, UnpooledDiskRender>,
    pub alert_state: AlertState,
    pub scrub: ScrubState,
    pub balance: BalanceReport,
    pub capacity_total_bytes: Option<u64>,
    pub capacity_used_bytes: u64,
    pub probed_at: Instant,
}

pub enum PoolStatus {
    Loading,
    NotMounted,
    Mounted(PoolState),
    Refreshing(PoolState),
    Error(String),
    ErrorStale(String, PoolState),
}

impl PoolStatus {
    pub fn current(&self) -> Option<&PoolState> {
        match self {
            PoolStatus::Mounted(p) | PoolStatus::Refreshing(p) | PoolStatus::ErrorStale(_, p) => {
                Some(p)
            }
            _ => None,
        }
    }

    /// Error message from a failed refresh when stale pool data remains visible.
    pub fn stale_error(&self) -> Option<&str> {
        match self {
            PoolStatus::ErrorStale(msg, _) => Some(msg.as_str()),
            _ => None,
        }
    }

    pub fn is_inflight(&self) -> bool {
        matches!(self, PoolStatus::Loading | PoolStatus::Refreshing(_))
    }
}

pub struct Model {
    pub running: bool,
    pub show_help: bool,
    pub show_disk_detail: bool,
    pub tab: Tab,
    pub disks: DiskIdentity,
    pub selected_disk: usize,
    pub pool: PoolStatus,
    pub mount_point: MountPoint,
    pub probe_duration: Option<Duration>,
    pub frame: u64,
    pub spinner_deadline: Option<Instant>,
    pub advisories: Vec<String>,
    pub paths: Option<StatePaths>,
    pub disk_luks_states: HashMap<String, DiskLuksState>,
    pub session_temperature_stats: HashMap<TemperatureDiskId, TemperatureWatermark>,
    pub fan_control: Option<crate::config::FanControl>,
    pub fan: Option<FanSnapshot>,
    pub fan_probe_inflight: bool,
    pub fan_scheduler_pending: bool,
    pub ups_config: Option<crate::config::Ups>,
    pub ups: Option<UpsSnapshot>,
    pub ups_probe_inflight: bool,
    pub ups_scheduler_pending: bool,
    /// Session state for the raw-output Browse tab, kept inside the TUI
    /// model so tab changes and probe results can drive its loader.
    pub browse: BrowseState,
}

impl Model {
    pub fn new(
        disks: DiskIdentity,
        mount_point: String,
        fan_control: Option<crate::config::FanControl>,
        ups_config: Option<crate::config::Ups>,
        advisories: Vec<String>,
        paths: StatePaths,
    ) -> (Self, Vec<Effect>) {
        let mount_point = MountPoint(mount_point);
        let mut effects: Vec<Effect> = vec![Effect::ProbePool {
            mount_point: mount_point.clone(),
            disks: disks.clone(),
            paths: paths.clone(),
        }];
        let mut fan_probe_inflight = false;
        if let Some(fc) = fan_control.as_ref() {
            effects.push(Effect::ProbeFan {
                sysfs_root: std::path::PathBuf::from("/sys"),
                dev_root: std::path::PathBuf::from("/dev"),
                disk_by_id: disks.by_id.clone(),
                fan_control: fc.clone(),
            });
            fan_probe_inflight = true;
        }
        // Kick off the UPS probe immediately so the first render shows
        // live state rather than a placeholder that disappears on the
        // next poll tick.
        let mut ups_probe_inflight = false;
        if let Some(u) = ups_config.as_ref() {
            effects.push(Effect::ProbeUps {
                name: u.name.clone(),
            });
            ups_probe_inflight = true;
        }
        let model = Self {
            running: true,
            show_help: false,
            show_disk_detail: false,
            tab: Tab::Data,
            disks,
            selected_disk: 0,
            pool: PoolStatus::Loading,
            mount_point,
            probe_duration: None,
            frame: 0,
            spinner_deadline: Some(Instant::now() + Duration::from_millis(500)),
            advisories,
            paths: Some(paths),
            disk_luks_states: HashMap::new(),
            session_temperature_stats: HashMap::new(),
            fan_control,
            fan: None,
            fan_probe_inflight,
            fan_scheduler_pending: false,
            ups_config,
            ups: None,
            ups_probe_inflight,
            ups_scheduler_pending: false,
            browse: BrowseState::default(),
        };
        (model, effects)
    }

    pub fn new_demo(disk_names: Vec<String>, pool: PoolStatus) -> Self {
        Self {
            running: true,
            show_help: false,
            show_disk_detail: false,
            tab: Tab::Data,
            disks: DiskIdentity {
                names: disk_names,
                ..Default::default()
            },
            selected_disk: 0,
            pool,
            mount_point: MountPoint(String::new()),
            probe_duration: None,
            frame: 0,
            spinner_deadline: None,
            advisories: vec![],
            paths: None,
            disk_luks_states: HashMap::new(),
            session_temperature_stats: HashMap::new(),
            fan_control: None,
            fan: None,
            fan_probe_inflight: false,
            fan_scheduler_pending: false,
            ups_config: None,
            ups: None,
            ups_probe_inflight: false,
            ups_scheduler_pending: false,
            browse: BrowseState::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::{DiskMember, PoolMembership};
    use crate::state_paths::StatePaths;
    use crate::tui::app::{Message, update};
    use crate::types::DiskName;

    fn uuid(raw: &str) -> LuksUuid {
        LuksUuid::parse(raw).expect("valid LUKS UUID in fixture")
    }

    fn by_id(raw: &str) -> ByIdPath {
        ByIdPath::parse(raw).expect("valid by-id path in fixture")
    }

    fn disk_name(raw: &str) -> DiskName {
        DiskName::parse(raw).expect("valid disk name in fixture")
    }

    // Intent: DiskIdentity::from_membership maps each membership axis into the
    //         right name-keyed slot (names sorted by DiskName, by_id and
    //         luks_uuid name-keyed without swap, devid filtered to Some(_)
    //         only).
    // Why it exists: the refactor moved an inline four-map build into a named
    //         constructor; a silent swap of luks_uuid <-> devid or a name/UUID
    //         pairing mistake would compile and pass a structure-only test.
    //         Inverting DiskName order against UUID order forces the test to
    //         distinguish them.
    // Scenario: two-member pool. UUID-A < UUID-B by sort, but UUID-A holds
    //         "zeta" and UUID-B holds "alpha". UUID-A has devid Some(7);
    //         UUID-B has devid None.
    #[test]
    fn from_membership_maps_all_four_fields() {
        let uuid_a = uuid("11111111-1111-1111-1111-111111111111");
        let uuid_b = uuid("22222222-2222-2222-2222-222222222222");
        let mut m = PoolMembership::empty();
        m.insert(
            uuid_a.clone(),
            DiskMember {
                name: disk_name("zeta"),
                by_id: by_id("/dev/disk/by-id/braid-zeta"),
                devid: Some(7),
                added_at: None,
            },
        )
        .unwrap();
        m.insert(
            uuid_b.clone(),
            DiskMember {
                name: disk_name("alpha"),
                by_id: by_id("/dev/disk/by-id/braid-alpha"),
                devid: None,
                added_at: None,
            },
        )
        .unwrap();

        let identity = DiskIdentity::from_membership(&m);

        assert_eq!(identity.names, vec!["alpha".to_owned(), "zeta".to_owned()]);
        assert_eq!(
            identity.by_id.get("alpha").map(String::as_str),
            Some("/dev/disk/by-id/braid-alpha"),
        );
        assert_eq!(
            identity.by_id.get("zeta").map(String::as_str),
            Some("/dev/disk/by-id/braid-zeta"),
        );
        assert_eq!(identity.luks_uuid.get("alpha"), Some(&uuid_b));
        assert_eq!(identity.luks_uuid.get("zeta"), Some(&uuid_a));
        assert_eq!(identity.devid.len(), 1);
        assert_eq!(identity.devid.get("zeta"), Some(&7));
        assert!(!identity.devid.contains_key("alpha"));
    }

    fn fixture_identity() -> DiskIdentity {
        DiskIdentity {
            names: vec!["alpha".to_owned()],
            by_id: HashMap::from([("alpha".to_owned(), "/dev/disk/by-id/braid-alpha".to_owned())]),
            luks_uuid: HashMap::from([(
                "alpha".to_owned(),
                uuid("11111111-1111-1111-1111-111111111111"),
            )]),
            devid: HashMap::from([("alpha".to_owned(), 9)]),
        }
    }

    // Intent: Model::new's initial Effect::ProbePool must carry the full
    //         DiskIdentity passed in, not a Default placeholder.
    // Why it exists: the initial probe is the only data the worker thread sees
    //         until the first refresh; emitting Default::default() would
    //         silently strip every name->UUID/devid binding and the probe
    //         would mis-classify mounted devices.
    // Scenario: TUI startup with a one-disk membership.
    #[test]
    fn new_carries_identity_into_initial_probe() {
        let identity = fixture_identity();
        let tmp = tempfile::tempdir().unwrap();
        let (_model, effects) = Model::new(
            identity.clone(),
            "/mnt/storage".to_owned(),
            None,
            None,
            vec![],
            StatePaths::custom(tmp.path().into()),
        );
        let probe_disks = effects
            .iter()
            .find_map(|e| match e {
                Effect::ProbePool { disks, .. } => Some(disks),
                _ => None,
            })
            .expect("startup must emit Effect::ProbePool");
        assert_eq!(probe_disks, &identity);
    }

    // Intent: Message::RefreshPool's re-emitted Effect::ProbePool must carry
    //         the Model's current DiskIdentity, not a stale or Default value.
    // Why it exists: the refresh path is where the user re-presses `r`; if
    //         the identity got dropped between startup and refresh, every
    //         post-refresh probe would lose name->UUID/devid resolution and
    //         disks would render as missing.
    // Scenario: demo Model seeded with a one-disk identity and StatePaths,
    //         user dispatches RefreshPool.
    #[test]
    fn refresh_pool_carries_identity_into_probe() {
        let identity = fixture_identity();
        let mut model = Model::new_demo(
            vec!["alpha".to_owned()],
            PoolStatus::Mounted(crate::tui::demo::sample_pool()),
        );
        model.disks = identity.clone();
        let tmp = tempfile::tempdir().unwrap();
        model.paths = Some(StatePaths::custom(tmp.path().into()));

        let effects = update(&mut model, Message::RefreshPool);

        let probe_disks = effects
            .iter()
            .find_map(|e| match e {
                Effect::ProbePool { disks, .. } => Some(disks),
                _ => None,
            })
            .expect("RefreshPool must emit Effect::ProbePool");
        assert_eq!(probe_disks, &identity);
    }
}
