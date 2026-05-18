use std::collections::{HashMap, HashSet};
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
    pub flags: HashSet<UpsStatusFlag>,
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
    pub disk_names: Vec<String>,
    pub disk_by_id: HashMap<String, String>,
    /// Persistent disk identity map so TUI probes do not infer pool
    /// membership from mapper names.
    pub disk_luks_uuid: HashMap<String, LuksUuid>,
    /// Prior btrfs devid bindings used when a live probe cannot observe the
    /// underlying LUKS UUID for a mounted device.
    pub disk_devid: HashMap<String, u64>,
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
    // Constructor mirrors persisted config plus optional subsystem configs;
    // grouping them would hide the call-site mapping this boundary owns.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        disk_names: Vec<String>,
        disk_by_id: HashMap<String, String>,
        disk_luks_uuid: HashMap<String, LuksUuid>,
        disk_devid: HashMap<String, u64>,
        mount_point: String,
        fan_control: Option<crate::config::FanControl>,
        ups_config: Option<crate::config::Ups>,
        advisories: Vec<String>,
        paths: StatePaths,
    ) -> (Self, Vec<Effect>) {
        let mount_point = MountPoint(mount_point);
        let mut effects: Vec<Effect> = vec![Effect::ProbePool {
            mount_point: mount_point.clone(),
            disk_by_id: disk_by_id.clone(),
            disk_luks_uuid: disk_luks_uuid.clone(),
            disk_devid: disk_devid.clone(),
            paths: paths.clone(),
        }];
        let fan_probe_inflight = fan_control.is_some();
        if let Some(fc) = fan_control.as_ref() {
            effects.push(Effect::ProbeFan {
                sysfs_root: std::path::PathBuf::from("/sys"),
                dev_root: std::path::PathBuf::from("/dev"),
                disk_by_id: disk_by_id.clone(),
                fan_control: fc.clone(),
            });
        }
        // Kick off the UPS probe immediately so the first render shows
        // live state rather than a placeholder that disappears on the
        // next poll tick.
        let ups_probe_inflight = ups_config.is_some();
        if let Some(u) = ups_config.as_ref() {
            effects.push(Effect::ProbeUps {
                name: u.name.clone(),
            });
        }
        let model = Self {
            running: true,
            show_help: false,
            show_disk_detail: false,
            tab: Tab::Data,
            disk_names,
            disk_by_id,
            disk_luks_uuid,
            disk_devid,
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
            disk_names,
            disk_by_id: HashMap::new(),
            disk_luks_uuid: HashMap::new(),
            disk_devid: HashMap::new(),
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
