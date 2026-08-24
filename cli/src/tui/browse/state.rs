use std::cell::Cell;
use std::collections::HashMap;

use crate::cmd::{CmdRequest, RawCommandOutput};
use crate::config::Ups;
use crate::parse::types::BtrfsSubvolume;
use crate::parse::{SystemdUnitRow, parse_btrfs_subvolume_list, parse_systemctl_list_units_json};
use crate::tui::effect::Effect;
use crate::tui::model::{PoolStatus, smart_query_device};
use crate::types::MountPoint;

/// Focus owner for Browse's sidebar/content regions so the top-level
/// key router can send local movement to the right column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowseFocus {
    Program,
    Command,
    Subview,
    Content,
}

/// First Browse column: external tool family whose raw output is being
/// inspected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BrowseProgram {
    Btrfs,
    Nut,
    Systemd,
    Smartctl,
    Lsblk,
}

impl BrowseProgram {
    const ALL: [Self; 5] = [
        Self::Btrfs,
        Self::Nut,
        Self::Systemd,
        Self::Smartctl,
        Self::Lsblk,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Btrfs => "Btrfs",
            Self::Nut => "NUT",
            Self::Systemd => "Systemd",
            Self::Smartctl => "SMART",
            Self::Lsblk => "lsblk",
        }
    }
}

/// Second Browse column: command groups exposed for the active program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BrowseCommand {
    BtrfsFilesystem,
    BtrfsDevices,
    BtrfsSubvolumes,
    BtrfsScrub,
    BtrfsBalance,
    BtrfsQuota,
    BtrfsInspect,
    NutStatus,
    NutVariables,
    NutCommands,
    NutClients,
    NutRwVars,
    NutUpses,
    SystemdStatus,
    SystemdShow,
    SystemdBraid,
    SystemdFailed,
    SystemdTimers,
    SystemdMounts,
    SmartctlScan,
    SmartctlHealth,
    SmartctlInfo,
    SmartctlAttributes,
    SmartctlSelftestLog,
    SmartctlErrorLog,
    LsblkTree,
    LsblkFilesystems,
    LsblkDisks,
    LsblkAllColumns,
    LsblkScsi,
}

impl BrowseCommand {
    const BTRFS: [Self; 7] = [
        Self::BtrfsFilesystem,
        Self::BtrfsDevices,
        Self::BtrfsSubvolumes,
        Self::BtrfsScrub,
        Self::BtrfsBalance,
        Self::BtrfsQuota,
        Self::BtrfsInspect,
    ];
    const NUT: [Self; 6] = [
        Self::NutStatus,
        Self::NutVariables,
        Self::NutCommands,
        Self::NutClients,
        Self::NutRwVars,
        Self::NutUpses,
    ];
    const SYSTEMD: [Self; 6] = [
        Self::SystemdStatus,
        Self::SystemdShow,
        Self::SystemdBraid,
        Self::SystemdFailed,
        Self::SystemdTimers,
        Self::SystemdMounts,
    ];
    const SMARTCTL: [Self; 6] = [
        Self::SmartctlScan,
        Self::SmartctlHealth,
        Self::SmartctlInfo,
        Self::SmartctlAttributes,
        Self::SmartctlSelftestLog,
        Self::SmartctlErrorLog,
    ];
    const LSBLK: [Self; 5] = [
        Self::LsblkTree,
        Self::LsblkFilesystems,
        Self::LsblkDisks,
        Self::LsblkAllColumns,
        Self::LsblkScsi,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BtrfsFilesystem => "Filesystem",
            Self::BtrfsDevices => "Devices",
            Self::BtrfsSubvolumes => "Subvolumes",
            Self::BtrfsScrub => "Scrub",
            Self::BtrfsBalance => "Balance",
            Self::BtrfsQuota => "Quota",
            Self::BtrfsInspect => "Inspect",
            Self::NutStatus => "Status",
            Self::NutVariables => "Variables",
            Self::NutCommands => "Commands",
            Self::NutClients => "Clients",
            Self::NutRwVars => "RW Vars",
            Self::NutUpses => "UPSes",
            Self::SystemdStatus => "Status",
            Self::SystemdShow => "Show",
            Self::SystemdBraid => "Braid",
            Self::SystemdFailed => "Failed",
            Self::SystemdTimers => "Timers",
            Self::SystemdMounts => "Mounts",
            Self::SmartctlScan => "Scan",
            Self::SmartctlHealth => "Health",
            Self::SmartctlInfo => "Info",
            Self::SmartctlAttributes => "Attributes",
            Self::SmartctlSelftestLog => "Self-test Log",
            Self::SmartctlErrorLog => "Error Log",
            Self::LsblkTree => "Tree",
            Self::LsblkFilesystems => "Filesystems",
            Self::LsblkDisks => "Disks",
            Self::LsblkAllColumns => "All Columns",
            Self::LsblkScsi => "SCSI",
        }
    }
}

/// Btrfs filesystem subviews preserve the raw commands the old browse
/// TUI exposed under its Filesystem tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum FilesystemSubview {
    Usage,
    Show,
    Df,
    CommitStats,
}

impl FilesystemSubview {
    const ALL: [Self; 4] = [Self::Usage, Self::Show, Self::Df, Self::CommitStats];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Show => "Show",
            Self::Df => "Df",
            Self::CommitStats => "Commit Stats",
        }
    }
}

/// Btrfs device subviews keep device stats visible as raw error-counter
/// output while curated disk tables stay separate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DeviceSubview {
    Usage,
    Stats,
}

impl DeviceSubview {
    const ALL: [Self; 2] = [Self::Usage, Self::Stats];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Stats => "Stats",
        }
    }
}

/// Btrfs subvolume subviews split the parsed default list from raw
/// inventories that use different field and type filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum SubvolumeSubview {
    List,
    Full,
    Snapshots,
    Deleted,
    Default,
}

impl SubvolumeSubview {
    const ALL: [Self; 5] = [
        Self::List,
        Self::Full,
        Self::Snapshots,
        Self::Deleted,
        Self::Default,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::List => "List",
            Self::Full => "Full",
            Self::Snapshots => "Snapshots",
            Self::Deleted => "Deleted",
            Self::Default => "Default",
        }
    }
}

/// Scrub subviews separate live scrub state from read-only throttle limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ScrubSubview {
    Status,
    Limits,
}

impl ScrubSubview {
    const ALL: [Self; 2] = [Self::Status, Self::Limits];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Limits => "Limits",
        }
    }
}

/// Quota subviews expose global quota status and qgroup accounting without
/// enabling, disabling, or rescanning quotas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum QuotaSubview {
    Status,
    Qgroups,
}

impl QuotaSubview {
    const ALL: [Self; 2] = [Self::Status, Self::Qgroups];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Status => "Status",
            Self::Qgroups => "Qgroups",
        }
    }
}

/// Inspect subviews are raw btrfs inspect-internal reports that do not
/// mutate filesystem state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum InspectSubview {
    Chunks,
}

impl InspectSubview {
    const ALL: [Self; 1] = [Self::Chunks];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Chunks => "Chunks",
        }
    }
}

/// Content replacement installed by the central Browse loader when the
/// selected command is gated by unavailable runtime state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BrowseEmptyState {
    PoolOffline,
    UpsNotConfigured,
    NoDisksKnown,
}

impl BrowseEmptyState {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::PoolOffline => "pool not mounted -- run `braid unlock` to access btrfs data",
            Self::UpsNotConfigured => {
                "UPS not configured -- set `ups.name` in the braid NixOS module"
            }
            Self::NoDisksKnown => {
                "no disks known to braid -- run `braid discover` or add disks first"
            }
        }
    }
}

/// Borrowed disk inventory lets Browse derive device pickers from the live
/// model without owning or cloning the TUI's authoritative disk identity map.
pub(crate) struct DiskInventory<'a> {
    pub(crate) by_id: &'a HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseMode {
    Normal,
    SubvolDetail,
    SmartctlDeviceDetail,
    SystemdUnitDetail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum BrowseSelection {
    BtrfsFilesystem(FilesystemSubview),
    BtrfsDevices(DeviceSubview),
    BtrfsSubvolumes(SubvolumeSubview),
    BtrfsScrub(ScrubSubview),
    BtrfsBalance,
    BtrfsQuota(QuotaSubview),
    BtrfsInspect(InspectSubview),
    NutStatus,
    NutVariables,
    NutCommands,
    NutClients,
    NutRwVars,
    NutUpses,
    SystemdStatus,
    SystemdShow,
    SystemdBraid,
    SystemdFailed,
    SystemdTimers,
    SystemdMounts,
    SmartctlScan,
    SmartctlHealth,
    SmartctlInfo,
    SmartctlAttributes,
    SmartctlSelftestLog,
    SmartctlErrorLog,
    LsblkTree,
    LsblkFilesystems,
    LsblkDisks,
    LsblkAllColumns,
    LsblkScsi,
}

impl BrowseSelection {
    fn requires_ups_name(self) -> bool {
        matches!(
            self,
            Self::NutStatus
                | Self::NutVariables
                | Self::NutCommands
                | Self::NutClients
                | Self::NutRwVars
        )
    }

    fn uses_model_snapshot(self) -> bool {
        matches!(self, Self::NutStatus | Self::NutVariables)
    }

    fn is_smartctl_picker(self) -> bool {
        matches!(
            self,
            Self::SmartctlHealth
                | Self::SmartctlInfo
                | Self::SmartctlAttributes
                | Self::SmartctlSelftestLog
                | Self::SmartctlErrorLog
        )
    }

    fn is_systemd_picker(self) -> bool {
        matches!(self, Self::SystemdStatus | Self::SystemdShow)
    }
}

#[derive(Clone, Default)]
struct CachedOutput {
    output: Vec<String>,
    subvolumes: Vec<BtrfsSubvolume>,
    systemd_units: Vec<SystemdUnitRow>,
}

/// State owned by the `tui` model for the Browse tab. It centralizes
/// sidebar selection, command generations, raw output cache, and
/// subvolume drill-in so update and view code share one Browse contract.
pub(crate) struct BrowseState {
    pub(crate) focus: BrowseFocus,
    program: BrowseProgram,
    btrfs_command: BrowseCommand,
    nut_command: BrowseCommand,
    systemd_command: BrowseCommand,
    smartctl_command: BrowseCommand,
    lsblk_command: BrowseCommand,
    filesystem_subview: FilesystemSubview,
    device_subview: DeviceSubview,
    subvolume_subview: SubvolumeSubview,
    scrub_subview: ScrubSubview,
    quota_subview: QuotaSubview,
    inspect_subview: InspectSubview,
    output: Vec<String>,
    cache: HashMap<BrowseSelection, CachedOutput>,
    empty_state: Option<BrowseEmptyState>,
    loading: bool,
    command_gen: u64,
    scroll_offset: usize,
    viewport_height: Cell<u16>,
    mode: BrowseMode,
    subvolumes: Vec<BtrfsSubvolume>,
    subvol_selected: usize,
    subvol_list_output: Vec<String>,
    smartctl_devices: Vec<(String, String)>,
    smartctl_selected: usize,
    smartctl_picker_output: Vec<String>,
    systemd_units: Vec<SystemdUnitRow>,
    systemd_unit_selected: usize,
    systemd_picker_output: Vec<String>,
    force_reload_once: bool,
    /// Footer source for detail modes: the request last dispatched into a
    /// drill-in. Normal mode derives its footer from the selection map.
    last_detail_request: Option<CmdRequest>,
}

impl Default for BrowseState {
    fn default() -> Self {
        Self {
            focus: BrowseFocus::Program,
            program: BrowseProgram::Btrfs,
            btrfs_command: BrowseCommand::BtrfsFilesystem,
            nut_command: BrowseCommand::NutStatus,
            systemd_command: BrowseCommand::SystemdStatus,
            smartctl_command: BrowseCommand::SmartctlScan,
            lsblk_command: BrowseCommand::LsblkTree,
            filesystem_subview: FilesystemSubview::Usage,
            device_subview: DeviceSubview::Usage,
            subvolume_subview: SubvolumeSubview::List,
            scrub_subview: ScrubSubview::Status,
            quota_subview: QuotaSubview::Status,
            inspect_subview: InspectSubview::Chunks,
            output: Vec::new(),
            cache: HashMap::new(),
            empty_state: None,
            loading: false,
            command_gen: 0,
            scroll_offset: 0,
            viewport_height: Cell::new(20),
            mode: BrowseMode::Normal,
            subvolumes: Vec::new(),
            subvol_selected: 0,
            subvol_list_output: Vec::new(),
            smartctl_devices: Vec::new(),
            smartctl_selected: 0,
            smartctl_picker_output: Vec::new(),
            systemd_units: Vec::new(),
            systemd_unit_selected: 0,
            systemd_picker_output: Vec::new(),
            force_reload_once: false,
            last_detail_request: None,
        }
    }
}

impl BrowseState {
    /// Central scheduler for Browse content. Every tab-entry, sidebar
    /// selection, and reload funnels through this function so offline
    /// pool and missing-UPS empty states cannot drift between call sites.
    pub(crate) fn load_current(
        &mut self,
        pool: &PoolStatus,
        ups_config: Option<&Ups>,
        disks: &DiskInventory<'_>,
    ) -> Option<Effect> {
        self.mode = BrowseMode::Normal;
        self.scroll_offset = 0;
        self.empty_state = None;
        self.loading = false;

        let selection = self.current_selection();
        let force_reload = std::mem::take(&mut self.force_reload_once);

        if self.is_btrfs_selected() && pool.current().is_none() {
            self.install_empty(BrowseEmptyState::PoolOffline);
            return None;
        }
        if selection.requires_ups_name() && ups_config.is_none() {
            self.install_empty(BrowseEmptyState::UpsNotConfigured);
            return None;
        }

        if selection.uses_model_snapshot() {
            self.output.clear();
            self.clear_picker_rows();
            return None;
        }

        if selection.is_smartctl_picker() {
            self.populate_smartctl_devices(disks, pool);
            if self.smartctl_devices.is_empty() {
                self.install_empty(BrowseEmptyState::NoDisksKnown);
            } else {
                self.empty_state = None;
                self.loading = false;
                self.output = vec!["press Enter for SMART data".to_owned()];
                self.subvolumes.clear();
                self.systemd_units.clear();
            }
            return None;
        }

        if matches!(selection, BrowseSelection::NutCommands)
            && !force_reload
            && let Some(cached) = self.cache.get(&selection).cloned()
        {
            self.install_cached(cached);
            return None;
        }

        if let Some(cached) = self.cache.get(&selection).cloned() {
            self.install_cached(cached);
        } else {
            self.output.clear();
            self.clear_picker_rows();
        }

        let request = self.current_request(pool, ups_config)?;
        self.command_gen = self.command_gen.saturating_add(1);
        self.loading = true;
        Some(Effect::BrowseRunCommand {
            request,
            generation: self.command_gen,
        })
    }

    /// Mark the next `load_current` call as an explicit refresh so
    /// session-cached commands such as `upscmd -l` can be fetched again.
    pub(crate) fn force_next_load(&mut self) {
        self.force_reload_once = true;
    }

    /// Move focus one Browse region to the left, respecting the dynamic
    /// absence of a subview column for commands that have no subviews.
    pub(crate) fn focus_left(&mut self) {
        self.focus = match self.focus {
            BrowseFocus::Program => BrowseFocus::Program,
            BrowseFocus::Command => BrowseFocus::Program,
            BrowseFocus::Subview => BrowseFocus::Command,
            BrowseFocus::Content if self.has_subviews() => BrowseFocus::Subview,
            BrowseFocus::Content => BrowseFocus::Command,
        };
    }

    /// Move focus one Browse region to the right, skipping the subview
    /// region unless the active command actually has subviews.
    pub(crate) fn focus_right(&mut self) {
        self.focus = match self.focus {
            BrowseFocus::Program => BrowseFocus::Command,
            BrowseFocus::Command if self.has_subviews() => BrowseFocus::Subview,
            BrowseFocus::Command => BrowseFocus::Content,
            BrowseFocus::Subview => BrowseFocus::Content,
            BrowseFocus::Content => BrowseFocus::Content,
        };
    }

    /// Advance selection or content scroll in the currently focused
    /// Browse region.
    pub(crate) fn select_next(&mut self) {
        match self.focus {
            BrowseFocus::Program => self.cycle_program(1),
            BrowseFocus::Command => self.cycle_command(1),
            BrowseFocus::Subview => self.cycle_subview(1),
            BrowseFocus::Content => self.content_down(),
        }
        self.normalize_focus();
    }

    /// Move selection or content scroll backward in the currently
    /// focused Browse region.
    pub(crate) fn select_prev(&mut self) {
        match self.focus {
            BrowseFocus::Program => self.cycle_program(-1),
            BrowseFocus::Command => self.cycle_command(-1),
            BrowseFocus::Subview => self.cycle_subview(-1),
            BrowseFocus::Content => self.content_up(),
        }
        self.normalize_focus();
    }

    /// Drill into the selected content row when Browse owns a parsed picker
    /// surface. Btrfs, SMART, and Systemd keep separate detail modes because
    /// their target identifiers come from different model boundaries.
    pub(crate) fn enter(&mut self, pool: &PoolStatus, disks: &DiskInventory<'_>) -> Option<Effect> {
        if self.focus != BrowseFocus::Content || self.mode != BrowseMode::Normal {
            return None;
        }
        if self.is_subvolume_list() && !self.subvolumes.is_empty() {
            let pool = pool.current()?;
            let subvol = &self.subvolumes[self.subvol_selected];
            let path = format!("{}/{}", pool.mount_point.as_str(), subvol.path);
            self.mode = BrowseMode::SubvolDetail;
            self.subvol_list_output = self.output.clone();
            return self.dispatch(CmdRequest::BtrfsSubvolumeShow { path });
        }
        if self.is_smartctl_picker() {
            if self.smartctl_devices.is_empty() {
                self.populate_smartctl_devices(disks, pool);
            }
            let request = self.selected_smartctl_request()?;
            self.mode = BrowseMode::SmartctlDeviceDetail;
            self.smartctl_picker_output = self.output.clone();
            return self.dispatch(request);
        }
        if self.is_systemd_picker() && !self.systemd_units.is_empty() {
            let request = self.selected_systemd_request()?;
            self.mode = BrowseMode::SystemdUnitDetail;
            self.systemd_picker_output = self.output.clone();
            return self.dispatch(request);
        }
        None
    }

    /// Return from drill-in content to the cached list, invalidating the
    /// in-flight detail command so stale detail output cannot overwrite it.
    pub(crate) fn back(&mut self) {
        let restored = match self.mode {
            BrowseMode::Normal => return,
            BrowseMode::SubvolDetail => self.subvol_list_output.clone(),
            BrowseMode::SmartctlDeviceDetail => self.smartctl_picker_output.clone(),
            BrowseMode::SystemdUnitDetail => self.systemd_picker_output.clone(),
        };
        self.mode = BrowseMode::Normal;
        self.output = restored;
        self.scroll_offset = 0;
        self.command_gen = self.command_gen.saturating_add(1);
        self.loading = false;
    }

    /// Reload the currently open detail while preserving the picker selection
    /// that owns the list beneath it.
    pub(crate) fn reload_detail(
        &mut self,
        pool: &PoolStatus,
        disks: &DiskInventory<'_>,
    ) -> Option<Effect> {
        match self.mode {
            BrowseMode::Normal => None,
            BrowseMode::SubvolDetail => {
                if self.subvolumes.is_empty() {
                    return None;
                }
                let pool = pool.current()?;
                let subvol = &self.subvolumes[self.subvol_selected];
                let path = format!("{}/{}", pool.mount_point.as_str(), subvol.path);
                self.dispatch(CmdRequest::BtrfsSubvolumeShow { path })
            }
            BrowseMode::SmartctlDeviceDetail => {
                if self.smartctl_devices.is_empty() {
                    self.populate_smartctl_devices(disks, pool);
                }
                let request = self.selected_smartctl_request()?;
                self.dispatch(request)
            }
            BrowseMode::SystemdUnitDetail => {
                let request = self.selected_systemd_request()?;
                self.dispatch(request)
            }
        }
    }

    /// Apply raw command output if its generation still matches the
    /// active Browse request.
    pub(crate) fn command_finished(&mut self, raw: RawCommandOutput, generation: u64) {
        if generation != self.command_gen {
            return;
        }
        self.loading = false;
        self.empty_state = None;
        self.output = raw.stdout.lines().map(str::to_owned).collect();
        if !raw.stderr.is_empty() {
            self.output.extend(raw.stderr.lines().map(str::to_owned));
        }
        self.scroll_offset = 0;

        if self.mode == BrowseMode::Normal {
            self.subvolumes.clear();
            self.systemd_units.clear();
            if self.is_subvolume_list() {
                match parse_btrfs_subvolume_list(&raw) {
                    Ok(parsed) => {
                        self.subvolumes = parsed.subvolumes;
                        self.subvol_selected = self
                            .subvol_selected
                            .min(self.subvolumes.len().saturating_sub(1));
                    }
                    Err(_) => self.subvolumes.clear(),
                }
            } else if self.is_systemd_picker() {
                match parse_systemctl_list_units_json(&raw) {
                    Ok(parsed) => {
                        self.systemd_units = parsed;
                        self.systemd_unit_selected = self
                            .systemd_unit_selected
                            .min(self.systemd_units.len().saturating_sub(1));
                    }
                    Err(_) => self.systemd_units.clear(),
                }
            }
        }

        if self.mode == BrowseMode::Normal {
            self.cache.insert(
                self.current_selection(),
                CachedOutput {
                    output: self.output.clone(),
                    subvolumes: self.subvolumes.clone(),
                    systemd_units: self.systemd_units.clone(),
                },
            );
        }
    }

    /// Page the content area down by one viewport, clamping at the last
    /// full viewport start.
    pub(crate) fn page_down(&mut self) {
        let page = self.viewport_height.get() as usize;
        self.scroll_offset = (self.scroll_offset + page).min(self.max_scroll());
    }

    /// Page the content area up by one viewport.
    pub(crate) fn page_up(&mut self) {
        self.scroll_offset = self
            .scroll_offset
            .saturating_sub(self.viewport_height.get() as usize);
    }

    /// Store the current renderable content height so content movement
    /// and page movement clamp to the actual visible Browse body.
    pub(crate) fn set_viewport_height(&self, height: u16) {
        self.viewport_height.set(height);
    }

    /// Largest `scroll_offset` that still fills the content viewport, so
    /// paging and line scrolling clamp here instead of revealing a blank
    /// tail past the last line.
    fn max_scroll(&self) -> usize {
        self.output
            .len()
            .saturating_sub(self.viewport_height.get() as usize)
    }

    pub(crate) fn focus(&self) -> BrowseFocus {
        self.focus
    }

    pub(crate) fn program_rows(&self) -> Vec<(&'static str, bool)> {
        BrowseProgram::ALL
            .iter()
            .map(|p| (p.label(), *p == self.program))
            .collect()
    }

    pub(crate) fn command_rows(&self) -> Vec<(&'static str, bool)> {
        self.commands()
            .iter()
            .map(|c| (c.label(), *c == self.current_command()))
            .collect()
    }

    pub(crate) fn subview_rows(&self) -> Vec<(&'static str, bool)> {
        match self.current_command() {
            BrowseCommand::BtrfsFilesystem => FilesystemSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.filesystem_subview))
                .collect(),
            BrowseCommand::BtrfsDevices => DeviceSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.device_subview))
                .collect(),
            BrowseCommand::BtrfsSubvolumes => SubvolumeSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.subvolume_subview))
                .collect(),
            BrowseCommand::BtrfsScrub => ScrubSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.scrub_subview))
                .collect(),
            BrowseCommand::BtrfsQuota => QuotaSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.quota_subview))
                .collect(),
            BrowseCommand::BtrfsInspect => InspectSubview::ALL
                .iter()
                .map(|s| (s.label(), *s == self.inspect_subview))
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn has_subviews(&self) -> bool {
        matches!(
            self.current_command(),
            BrowseCommand::BtrfsFilesystem
                | BrowseCommand::BtrfsDevices
                | BrowseCommand::BtrfsSubvolumes
                | BrowseCommand::BtrfsScrub
                | BrowseCommand::BtrfsQuota
                | BrowseCommand::BtrfsInspect
        )
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn empty_state(&self) -> Option<BrowseEmptyState> {
        self.empty_state
    }

    pub(crate) fn is_subvolume_list(&self) -> bool {
        self.current_selection() == BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::List)
            && self.mode == BrowseMode::Normal
    }

    pub(crate) fn is_detail(&self) -> bool {
        self.mode != BrowseMode::Normal
    }

    pub(crate) fn is_smartctl_picker(&self) -> bool {
        self.current_selection().is_smartctl_picker() && self.mode == BrowseMode::Normal
    }

    pub(crate) fn is_systemd_picker(&self) -> bool {
        self.current_selection().is_systemd_picker() && self.mode == BrowseMode::Normal
    }

    pub(crate) fn is_nut_status(&self) -> bool {
        self.current_selection() == BrowseSelection::NutStatus
    }

    pub(crate) fn is_nut_variables(&self) -> bool {
        self.current_selection() == BrowseSelection::NutVariables
    }

    pub(crate) fn output(&self) -> &[String] {
        &self.output
    }

    pub(crate) fn scroll_offset(&self) -> usize {
        self.scroll_offset
    }

    pub(crate) fn subvolumes(&self) -> &[BtrfsSubvolume] {
        &self.subvolumes
    }

    pub(crate) fn selected_subvolume(&self) -> usize {
        self.subvol_selected
    }

    pub(crate) fn smartctl_devices(&self) -> &[(String, String)] {
        &self.smartctl_devices
    }

    pub(crate) fn selected_smartctl_device(&self) -> usize {
        self.smartctl_selected
    }

    pub(crate) fn systemd_units(&self) -> &[SystemdUnitRow] {
        &self.systemd_units
    }

    pub(crate) fn selected_systemd_unit(&self) -> usize {
        self.systemd_unit_selected
    }

    pub(crate) fn command_display(
        &self,
        mount_point: &MountPoint,
        ups_config: Option<&Ups>,
    ) -> Option<String> {
        let request = match self.mode {
            BrowseMode::Normal => self.selection_request(Some(mount_point), ups_config)?,
            _ => self.last_detail_request.clone()?,
        };
        Some(request.to_argv().to_shell_string())
    }

    /// Single map from a Normal-mode Browse selection to its command so
    /// dispatch and footer rendering cannot drift for raw command views.
    fn selection_request(
        &self,
        mount_point: Option<&MountPoint>,
        ups_config: Option<&Ups>,
    ) -> Option<CmdRequest> {
        let request = match self.current_selection() {
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Usage) => {
                CmdRequest::BtrfsFilesystemUsage {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Show) => {
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Df) => {
                CmdRequest::BtrfsFilesystemDf {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::CommitStats) => {
                CmdRequest::BtrfsFilesystemCommitStats {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsDevices(DeviceSubview::Usage) => CmdRequest::BtrfsDeviceUsage {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsDevices(DeviceSubview::Stats) => CmdRequest::BtrfsDeviceStats {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::List) => {
                CmdRequest::BtrfsSubvolumeList {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Full) => {
                CmdRequest::BtrfsSubvolumeListFull {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Snapshots) => {
                CmdRequest::BtrfsSubvolumeListSnapshots {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Deleted) => {
                CmdRequest::BtrfsSubvolumeListDeleted {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Default) => {
                CmdRequest::BtrfsSubvolumeGetDefault {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Status) => {
                CmdRequest::BtrfsScrubStatusHuman {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Limits) => CmdRequest::BtrfsScrubLimit {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsBalance => CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsQuota(QuotaSubview::Status) => CmdRequest::BtrfsQuotaStatus {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsQuota(QuotaSubview::Qgroups) => CmdRequest::BtrfsQgroupShow {
                mount_point: mount_point?.clone(),
            },
            BrowseSelection::BtrfsInspect(InspectSubview::Chunks) => {
                CmdRequest::BtrfsInspectListChunks {
                    mount_point: mount_point?.clone(),
                }
            }
            BrowseSelection::NutStatus | BrowseSelection::NutVariables => CmdRequest::UpscQuery {
                name: ups_config?.name.as_str().to_owned(),
            },
            BrowseSelection::NutCommands => CmdRequest::UpscmdList {
                name: ups_config?.name.as_str().to_owned(),
            },
            BrowseSelection::NutClients => CmdRequest::UpscClients {
                name: ups_config?.name.as_str().to_owned(),
            },
            BrowseSelection::NutRwVars => CmdRequest::UpsrwList {
                name: ups_config?.name.as_str().to_owned(),
            },
            BrowseSelection::NutUpses => CmdRequest::UpscListUpses,
            BrowseSelection::SystemdStatus | BrowseSelection::SystemdShow => {
                CmdRequest::SystemctlListUnitsBraidJson
            }
            BrowseSelection::SystemdBraid => CmdRequest::SystemctlListUnitsBraid,
            BrowseSelection::SystemdFailed => CmdRequest::SystemctlListUnitsFailed,
            BrowseSelection::SystemdTimers => CmdRequest::SystemctlListTimers,
            BrowseSelection::SystemdMounts => CmdRequest::SystemctlListMounts,
            BrowseSelection::SmartctlScan => CmdRequest::SmartctlScan,
            BrowseSelection::SmartctlHealth
            | BrowseSelection::SmartctlInfo
            | BrowseSelection::SmartctlAttributes
            | BrowseSelection::SmartctlSelftestLog
            | BrowseSelection::SmartctlErrorLog => return self.selected_smartctl_request(),
            BrowseSelection::LsblkTree => CmdRequest::LsblkTree,
            BrowseSelection::LsblkFilesystems => CmdRequest::LsblkFilesystems,
            BrowseSelection::LsblkDisks => CmdRequest::LsblkDisks,
            BrowseSelection::LsblkAllColumns => CmdRequest::LsblkAllColumns,
            BrowseSelection::LsblkScsi => CmdRequest::LsblkScsi,
        };
        Some(request)
    }

    fn install_empty(&mut self, state: BrowseEmptyState) {
        self.output.clear();
        self.clear_picker_rows();
        self.empty_state = Some(state);
    }

    fn install_cached(&mut self, cached: CachedOutput) {
        self.output = cached.output;
        self.subvolumes = cached.subvolumes;
        self.systemd_units = cached.systemd_units;
        if !self.current_selection().is_smartctl_picker() {
            self.smartctl_devices.clear();
        }
    }

    fn clear_picker_rows(&mut self) {
        self.subvolumes.clear();
        self.smartctl_devices.clear();
        self.systemd_units.clear();
    }

    /// Build the picker rows, resolving each disk's probe target through the
    /// shared SMART rule: a present member shows (and dispatches against) its
    /// live backing path, an offline disk its persisted by-id handle. Storing
    /// the *resolved* device means the table row and the footer command cannot
    /// diverge from what `smartctl` actually probes (decision 024).
    fn populate_smartctl_devices(&mut self, disks: &DiskInventory<'_>, pool: &PoolStatus) {
        let present_underlying = pool.current().map(|p| &p.disk_underlying);
        self.smartctl_devices = disks
            .by_id
            .iter()
            .map(|(name, by_id)| {
                let device = match present_underlying {
                    Some(underlying) => smart_query_device(name, by_id, underlying).to_owned(),
                    None => by_id.clone(),
                };
                (name.clone(), device)
            })
            .collect();
        self.smartctl_devices.sort_by(|a, b| a.0.cmp(&b.0));
        self.smartctl_selected = self
            .smartctl_selected
            .min(self.smartctl_devices.len().saturating_sub(1));
    }

    fn dispatch(&mut self, request: CmdRequest) -> Option<Effect> {
        self.scroll_offset = 0;
        self.command_gen = self.command_gen.saturating_add(1);
        self.loading = true;
        self.empty_state = None;
        self.output.clear();
        self.last_detail_request = Some(request.clone());
        Some(Effect::BrowseRunCommand {
            request,
            generation: self.command_gen,
        })
    }

    fn current_request(&self, pool: &PoolStatus, ups_config: Option<&Ups>) -> Option<CmdRequest> {
        let selection = self.current_selection();
        if selection.uses_model_snapshot() || selection.is_smartctl_picker() {
            return None;
        }
        self.selection_request(pool.current().map(|p| &p.mount_point), ups_config)
    }

    fn selected_smartctl_request(&self) -> Option<CmdRequest> {
        let (_, device) = self.smartctl_devices.get(self.smartctl_selected)?;
        match self.current_selection() {
            BrowseSelection::SmartctlHealth => Some(CmdRequest::SmartctlHealth {
                device: device.clone(),
            }),
            BrowseSelection::SmartctlInfo => Some(CmdRequest::SmartctlInfo {
                device: device.clone(),
            }),
            BrowseSelection::SmartctlAttributes => Some(CmdRequest::SmartctlAttributes {
                device: device.clone(),
            }),
            BrowseSelection::SmartctlSelftestLog => Some(CmdRequest::SmartctlSelftestLog {
                device: device.clone(),
            }),
            BrowseSelection::SmartctlErrorLog => Some(CmdRequest::SmartctlErrorLog {
                device: device.clone(),
            }),
            _ => None,
        }
    }

    fn selected_systemd_request(&self) -> Option<CmdRequest> {
        let unit = self
            .systemd_units
            .get(self.systemd_unit_selected)?
            .unit
            .clone();
        match self.current_selection() {
            BrowseSelection::SystemdStatus => Some(CmdRequest::SystemctlStatusUnit { unit }),
            BrowseSelection::SystemdShow => Some(CmdRequest::SystemctlShowUnit { unit }),
            _ => None,
        }
    }

    fn current_selection(&self) -> BrowseSelection {
        match self.current_command() {
            BrowseCommand::BtrfsFilesystem => {
                BrowseSelection::BtrfsFilesystem(self.filesystem_subview)
            }
            BrowseCommand::BtrfsDevices => BrowseSelection::BtrfsDevices(self.device_subview),
            BrowseCommand::BtrfsSubvolumes => {
                BrowseSelection::BtrfsSubvolumes(self.subvolume_subview)
            }
            BrowseCommand::BtrfsScrub => BrowseSelection::BtrfsScrub(self.scrub_subview),
            BrowseCommand::BtrfsBalance => BrowseSelection::BtrfsBalance,
            BrowseCommand::BtrfsQuota => BrowseSelection::BtrfsQuota(self.quota_subview),
            BrowseCommand::BtrfsInspect => BrowseSelection::BtrfsInspect(self.inspect_subview),
            BrowseCommand::NutStatus => BrowseSelection::NutStatus,
            BrowseCommand::NutVariables => BrowseSelection::NutVariables,
            BrowseCommand::NutCommands => BrowseSelection::NutCommands,
            BrowseCommand::NutClients => BrowseSelection::NutClients,
            BrowseCommand::NutRwVars => BrowseSelection::NutRwVars,
            BrowseCommand::NutUpses => BrowseSelection::NutUpses,
            BrowseCommand::SystemdStatus => BrowseSelection::SystemdStatus,
            BrowseCommand::SystemdShow => BrowseSelection::SystemdShow,
            BrowseCommand::SystemdBraid => BrowseSelection::SystemdBraid,
            BrowseCommand::SystemdFailed => BrowseSelection::SystemdFailed,
            BrowseCommand::SystemdTimers => BrowseSelection::SystemdTimers,
            BrowseCommand::SystemdMounts => BrowseSelection::SystemdMounts,
            BrowseCommand::SmartctlScan => BrowseSelection::SmartctlScan,
            BrowseCommand::SmartctlHealth => BrowseSelection::SmartctlHealth,
            BrowseCommand::SmartctlInfo => BrowseSelection::SmartctlInfo,
            BrowseCommand::SmartctlAttributes => BrowseSelection::SmartctlAttributes,
            BrowseCommand::SmartctlSelftestLog => BrowseSelection::SmartctlSelftestLog,
            BrowseCommand::SmartctlErrorLog => BrowseSelection::SmartctlErrorLog,
            BrowseCommand::LsblkTree => BrowseSelection::LsblkTree,
            BrowseCommand::LsblkFilesystems => BrowseSelection::LsblkFilesystems,
            BrowseCommand::LsblkDisks => BrowseSelection::LsblkDisks,
            BrowseCommand::LsblkAllColumns => BrowseSelection::LsblkAllColumns,
            BrowseCommand::LsblkScsi => BrowseSelection::LsblkScsi,
        }
    }

    fn current_command(&self) -> BrowseCommand {
        match self.program {
            BrowseProgram::Btrfs => self.btrfs_command,
            BrowseProgram::Nut => self.nut_command,
            BrowseProgram::Systemd => self.systemd_command,
            BrowseProgram::Smartctl => self.smartctl_command,
            BrowseProgram::Lsblk => self.lsblk_command,
        }
    }

    fn commands(&self) -> &'static [BrowseCommand] {
        match self.program {
            BrowseProgram::Btrfs => &BrowseCommand::BTRFS,
            BrowseProgram::Nut => &BrowseCommand::NUT,
            BrowseProgram::Systemd => &BrowseCommand::SYSTEMD,
            BrowseProgram::Smartctl => &BrowseCommand::SMARTCTL,
            BrowseProgram::Lsblk => &BrowseCommand::LSBLK,
        }
    }

    fn is_btrfs_selected(&self) -> bool {
        self.program == BrowseProgram::Btrfs
    }

    fn cycle_program(&mut self, delta: isize) {
        let idx = BrowseProgram::ALL
            .iter()
            .position(|p| *p == self.program)
            .expect("active Browse program is in BrowseProgram::ALL");
        let next = wrap_index(idx, BrowseProgram::ALL.len(), delta);
        self.program = BrowseProgram::ALL[next];
    }

    fn cycle_command(&mut self, delta: isize) {
        let commands = self.commands();
        let current = self.current_command();
        let idx = commands
            .iter()
            .position(|c| *c == current)
            .expect("active Browse command is in program command list");
        let next = commands[wrap_index(idx, commands.len(), delta)];
        match self.program {
            BrowseProgram::Btrfs => self.btrfs_command = next,
            BrowseProgram::Nut => self.nut_command = next,
            BrowseProgram::Systemd => self.systemd_command = next,
            BrowseProgram::Smartctl => self.smartctl_command = next,
            BrowseProgram::Lsblk => self.lsblk_command = next,
        }
    }

    fn cycle_subview(&mut self, delta: isize) {
        match self.current_command() {
            BrowseCommand::BtrfsFilesystem => {
                let idx = FilesystemSubview::ALL
                    .iter()
                    .position(|s| *s == self.filesystem_subview)
                    .expect("active filesystem subview is in ALL");
                self.filesystem_subview =
                    FilesystemSubview::ALL[wrap_index(idx, FilesystemSubview::ALL.len(), delta)];
            }
            BrowseCommand::BtrfsDevices => {
                let idx = DeviceSubview::ALL
                    .iter()
                    .position(|s| *s == self.device_subview)
                    .expect("active device subview is in ALL");
                self.device_subview =
                    DeviceSubview::ALL[wrap_index(idx, DeviceSubview::ALL.len(), delta)];
            }
            BrowseCommand::BtrfsSubvolumes => {
                let idx = SubvolumeSubview::ALL
                    .iter()
                    .position(|s| *s == self.subvolume_subview)
                    .expect("active subvolume subview is in ALL");
                self.subvolume_subview =
                    SubvolumeSubview::ALL[wrap_index(idx, SubvolumeSubview::ALL.len(), delta)];
            }
            BrowseCommand::BtrfsScrub => {
                let idx = ScrubSubview::ALL
                    .iter()
                    .position(|s| *s == self.scrub_subview)
                    .expect("active scrub subview is in ALL");
                self.scrub_subview =
                    ScrubSubview::ALL[wrap_index(idx, ScrubSubview::ALL.len(), delta)];
            }
            BrowseCommand::BtrfsQuota => {
                let idx = QuotaSubview::ALL
                    .iter()
                    .position(|s| *s == self.quota_subview)
                    .expect("active quota subview is in ALL");
                self.quota_subview =
                    QuotaSubview::ALL[wrap_index(idx, QuotaSubview::ALL.len(), delta)];
            }
            BrowseCommand::BtrfsInspect => {
                let idx = InspectSubview::ALL
                    .iter()
                    .position(|s| *s == self.inspect_subview)
                    .expect("active inspect subview is in ALL");
                self.inspect_subview =
                    InspectSubview::ALL[wrap_index(idx, InspectSubview::ALL.len(), delta)];
            }
            _ => {}
        }
    }

    fn content_down(&mut self) {
        if self.is_subvolume_list() && !self.subvolumes.is_empty() {
            let max = self.subvolumes.len().saturating_sub(1);
            self.subvol_selected = (self.subvol_selected + 1).min(max);
        } else if self.is_smartctl_picker() && !self.smartctl_devices.is_empty() {
            let max = self.smartctl_devices.len().saturating_sub(1);
            self.smartctl_selected = (self.smartctl_selected + 1).min(max);
        } else if self.is_systemd_picker() && !self.systemd_units.is_empty() {
            let max = self.systemd_units.len().saturating_sub(1);
            self.systemd_unit_selected = (self.systemd_unit_selected + 1).min(max);
        } else {
            self.scroll_offset = (self.scroll_offset + 1).min(self.max_scroll());
        }
    }

    fn content_up(&mut self) {
        if self.is_subvolume_list() && !self.subvolumes.is_empty() {
            self.subvol_selected = self.subvol_selected.saturating_sub(1);
        } else if self.is_smartctl_picker() && !self.smartctl_devices.is_empty() {
            self.smartctl_selected = self.smartctl_selected.saturating_sub(1);
        } else if self.is_systemd_picker() && !self.systemd_units.is_empty() {
            self.systemd_unit_selected = self.systemd_unit_selected.saturating_sub(1);
        } else {
            self.scroll_offset = self.scroll_offset.saturating_sub(1);
        }
    }

    fn normalize_focus(&mut self) {
        if self.focus == BrowseFocus::Subview && !self.has_subviews() {
            self.focus = BrowseFocus::Content;
        }
    }
}

fn wrap_index(idx: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let len = len as isize;
    (idx as isize + delta).rem_euclid(len) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::BalanceReport;
    use crate::tui::model::PoolState;
    use std::collections::HashMap;

    fn pool() -> PoolStatus {
        PoolStatus::Mounted(PoolState {
            mount_point: MountPoint::new("/mnt/storage".to_owned()),
            df_entries: vec![],
            disk_usage: HashMap::new(),
            disk_transport: HashMap::new(),
            smart: HashMap::new(),
            disk_temperature_readings: HashMap::new(),
            disk_underlying: HashMap::new(),
            devid_names: HashMap::new(),
            device_errors: HashMap::new(),
            unpooled_disks: HashMap::new(),
            alert_causes: Vec::new(),
            scrub: crate::parse::types::ScrubState::Unknown,
            balance: BalanceReport::Idle,
            capacity_total_bytes: None,
            capacity_used_bytes: 0,
        })
    }

    /// `pool()` with a populated `disk_underlying` so picker tests can exercise
    /// the present-member (live-path) branch alongside the by-id fallback.
    fn pool_with_underlying(disk_underlying: HashMap<String, String>) -> PoolStatus {
        match pool() {
            PoolStatus::Mounted(mut state) => {
                state.disk_underlying = disk_underlying;
                PoolStatus::Mounted(state)
            }
            _ => unreachable!("pool() returns Mounted"),
        }
    }

    fn ups() -> Ups {
        Ups {
            name: crate::types::UpsName::parse("ups").unwrap(),
        }
    }

    fn browse_request(effect: Option<Effect>) -> CmdRequest {
        match effect {
            Some(Effect::BrowseRunCommand { request, .. }) => request,
            _ => panic!("expected BrowseRunCommand effect"),
        }
    }

    fn load_current_for_test(
        state: &mut BrowseState,
        pool: &PoolStatus,
        ups_config: Option<&Ups>,
    ) -> Option<Effect> {
        let disks = HashMap::new();
        state.load_current(pool, ups_config, &DiskInventory { by_id: &disks })
    }

    fn disk_inventory() -> HashMap<String, String> {
        [
            (
                "disk1".to_owned(),
                "/dev/disk/by-id/virtio-disk1".to_owned(),
            ),
            (
                "disk2".to_owned(),
                "/dev/disk/by-id/virtio-disk2".to_owned(),
            ),
        ]
        .into_iter()
        .collect()
    }

    // Intent: page scrolling clamps to the last full content viewport and
    // the start of content.
    // Why it exists: without this guard, Ctrl-D can reveal a blank tail and
    // Ctrl-U can underflow past the start of Browse output.
    // Scenario: user pages through ten lines of Browse output in a three-line
    // viewport, then pages back to the top.
    #[test]
    fn page_scroll_clamps_to_content_bounds() {
        let mut state = BrowseState {
            output: (0..10).map(|line| format!("line {line}")).collect(),
            ..Default::default()
        };
        state.set_viewport_height(3);

        state.page_down();
        assert_eq!(state.scroll_offset(), 3);
        state.page_down();
        assert_eq!(state.scroll_offset(), 6);
        state.page_down();
        assert_eq!(state.scroll_offset(), 7);
        state.page_down();
        assert_eq!(state.scroll_offset(), 7);

        state.page_up();
        assert_eq!(state.scroll_offset(), 4);
        state.page_up();
        assert_eq!(state.scroll_offset(), 1);
        state.page_up();
        assert_eq!(state.scroll_offset(), 0);
        state.page_up();
        assert_eq!(state.scroll_offset(), 0);
    }

    // Intent: content line scrolling clamps to the same content bounds as
    // page scrolling.
    // Why it exists: j/k use a sibling path to Ctrl-D/Ctrl-U, so their clamp
    // can drift unless the routed Content movement is pinned directly.
    // Scenario: user line-scrolls through ten lines in a three-line Browse
    // viewport and keeps pressing j/k past both endpoints.
    #[test]
    fn content_scroll_clamps_to_content_bounds() {
        let mut state = BrowseState {
            focus: BrowseFocus::Content,
            output: (0..10).map(|line| format!("line {line}")).collect(),
            ..Default::default()
        };
        state.set_viewport_height(3);

        for expected in 1..=7 {
            state.select_next();
            assert_eq!(state.scroll_offset(), expected);
        }
        state.select_next();
        assert_eq!(state.scroll_offset(), 7);

        for expected in (0..7).rev() {
            state.select_prev();
            assert_eq!(state.scroll_offset(), expected);
        }
        state.select_prev();
        assert_eq!(state.scroll_offset(), 0);
    }

    // Intent: the default Btrfs Browse selection renders the command it
    // would dispatch as the footer.
    // Why it exists: the footer and navigation path share one selection map,
    // so this pins the common Normal-mode path.
    // Scenario: user opens Browse on a mounted pool and sees filesystem usage.
    #[test]
    fn command_display_normal_btrfs_matches_selection_request() {
        let state = BrowseState::default();
        let mount_point = MountPoint::new("/mnt/storage".to_owned());

        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("btrfs filesystem usage /mnt/storage".to_owned())
        );
    }

    // Intent: SMART picker selections render a per-device preview without
    // dispatching a command during normal navigation.
    // Why it exists: picker rows come from braid's disk inventory, but the
    // footer still needs to track the selected device command.
    // Scenario: user opens Browse > SMART > Health and inspects disk1. The
    // pool's disk_underlying is empty (no live-path branch), so this pins the
    // by-id fallback.
    #[test]
    fn command_display_smartctl_picker_preview_uses_selected_device() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlHealth,
            ..Default::default()
        };
        let disks = disk_inventory();
        let effect = state.load_current(&pool(), Some(&ups()), &DiskInventory { by_id: &disks });
        let mount_point = MountPoint::new("/mnt/storage".to_owned());

        assert!(effect.is_none());
        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("smartctl -H /dev/disk/by-id/virtio-disk1".to_owned())
        );
    }

    // Intent: the SMART picker resolves each disk's probe target through the
    // shared rule -- a present member to its live backing path, an offline
    // disk to its by-id handle -- and stores the resolved device so the footer
    // command matches what smartctl runs.
    // Why it exists: the Browse picker was the last by-id present-device SMART
    // surface (decision 024); under by-id drift it probed a stale node while
    // the Data tab probed the live one. The fix routes both through
    // disk_underlying. Resolving from the superset disk_luks_states would
    // reintroduce that divergence, so this pins the present + offline branches
    // in one test.
    // Scenario: disk1 is a btrfs-assembled present member at live /dev/vdb;
    // disk2 is declared but offline (absent from disk_underlying).
    #[test]
    fn smartctl_picker_resolves_present_member_to_live_path() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlHealth,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        let pool =
            pool_with_underlying(HashMap::from([("disk1".to_owned(), "/dev/vdb".to_owned())]));
        let disks = disk_inventory();
        let mount_point = MountPoint::new("/mnt/storage".to_owned());

        let effect = state.load_current(&pool, Some(&ups()), &DiskInventory { by_id: &disks });
        assert!(effect.is_none());

        // disk1 (selected first) is present: row/footer resolve to the live node.
        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("smartctl -H /dev/vdb".to_owned())
        );

        // disk2 is absent from disk_underlying: falls back to its by-id handle.
        state.select_next();
        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("smartctl -H /dev/disk/by-id/virtio-disk2".to_owned())
        );
    }

    // Intent: NUT snapshot-backed Browse selections still render the raw
    // `upsc` query that produced the model snapshot.
    // Why it exists: NUT status and variables intentionally do not dispatch
    // during Browse navigation, but the footer remains useful provenance.
    // Scenario: user opens Browse > NUT > Status with braid.ups.name set.
    #[test]
    fn command_display_nut_snapshot_source_shows_upsc_query() {
        let state = BrowseState {
            program: BrowseProgram::Nut,
            ..Default::default()
        };
        let mount_point = MountPoint::new("/mnt/storage".to_owned());

        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("upsc ups".to_owned())
        );
    }

    // Intent: subvolume detail renders the `btrfs subvolume show` command
    // dispatched when the user drilled into the row.
    // Why it exists: subvolume detail used to show the list footer even
    // though the detail view was produced by a show command.
    // Scenario: user drills into `data` and the footer must not keep showing
    // the stale subvolume-list command from the parent view.
    #[test]
    fn command_display_subvolume_detail_shows_dispatched_request() {
        let mut state = BrowseState {
            btrfs_command: BrowseCommand::BtrfsSubvolumes,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        state.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list /mnt/storage".into(),
                stdout: "ID 256 gen 10 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        let disks = HashMap::new();
        let _ = state.enter(&pool(), &DiskInventory { by_id: &disks });
        let mount_point = MountPoint::new("/mnt/storage".to_owned());

        assert_eq!(
            state.command_display(&mount_point, Some(&ups())),
            Some("btrfs subvolume show /mnt/storage/data".to_owned())
        );
    }

    // Intent: h at the leftmost Browse column stays on Program.
    // Why it exists: boundary movement should be stable, not wrap
    // sideways into content.
    // Scenario: user is focused on Program and presses h.
    #[test]
    fn h_at_leftmost_is_noop() {
        let mut state = BrowseState::default();
        state.focus_left();
        assert_eq!(state.focus(), BrowseFocus::Program);
    }

    // Intent: l at the rightmost Browse column stays on Content.
    // Why it exists: boundary movement should be stable, not wrap back
    // to Program.
    // Scenario: user is focused on Content and presses l.
    #[test]
    fn l_at_rightmost_is_noop() {
        let mut state = BrowseState {
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        state.focus_right();
        assert_eq!(state.focus(), BrowseFocus::Content);
    }

    // Intent: l from Command skips the subview column when the selected
    // command has no subviews.
    // Why it exists: the fourth column is conditional; focus movement
    // must match what is rendered.
    // Scenario: user selects Btrfs Balance and presses l from Command.
    #[test]
    fn l_from_command_skips_subview_when_no_subviews() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        for _ in 0..4 {
            state.select_next();
        }
        state.focus_right();
        assert_eq!(state.focus(), BrowseFocus::Content);
    }

    // Intent: l from Command enters the filesystem subview column when
    // Filesystem is selected.
    // Why it exists: Filesystem has Usage/Show/Df views that need a
    // reachable focus region.
    // Scenario: user starts on Btrfs Filesystem and presses l from Command.
    #[test]
    fn l_from_command_enters_subview_when_filesystem() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        state.focus_right();
        assert_eq!(state.focus(), BrowseFocus::Subview);
    }

    // Intent: l from Command enters the devices subview column when
    // Devices is selected.
    // Why it exists: Device Usage/Stats is the second subview-bearing
    // command and needs independent coverage.
    // Scenario: user selects Btrfs Devices and presses l from Command.
    #[test]
    fn l_from_command_enters_subview_when_devices() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        state.select_next();
        state.focus_right();
        assert_eq!(state.focus(), BrowseFocus::Subview);
    }

    // Intent: j in Program cycles through every Browse program.
    // Why it exists: Program is the top-level Browse inventory and must
    // stay keyboard-reachable.
    // Scenario: user presses j repeatedly in the Program column.
    #[test]
    fn j_in_program_cycles_all_programs() {
        let mut state = BrowseState::default();
        state.select_next();
        assert_eq!(
            state.program_rows(),
            vec![
                ("Btrfs", false),
                ("NUT", true),
                ("Systemd", false),
                ("SMART", false),
                ("lsblk", false),
            ]
        );
        state.select_next();
        assert_eq!(
            state.program_rows(),
            vec![
                ("Btrfs", false),
                ("NUT", false),
                ("Systemd", true),
                ("SMART", false),
                ("lsblk", false),
            ]
        );
        state.select_next();
        assert_eq!(
            state.program_rows(),
            vec![
                ("Btrfs", false),
                ("NUT", false),
                ("Systemd", false),
                ("SMART", true),
                ("lsblk", false),
            ]
        );
        state.select_next();
        assert_eq!(
            state.program_rows(),
            vec![
                ("Btrfs", false),
                ("NUT", false),
                ("Systemd", false),
                ("SMART", false),
                ("lsblk", true),
            ]
        );
        state.select_next();
        assert_eq!(
            state.program_rows(),
            vec![
                ("Btrfs", true),
                ("NUT", false),
                ("Systemd", false),
                ("SMART", false),
                ("lsblk", false),
            ]
        );
    }

    // Intent: filesystem subview selection cycles through all filesystem views.
    // Why it exists: all raw filesystem commands are exposed under
    // one command group.
    // Scenario: user focuses the subview column and presses j repeatedly.
    #[test]
    fn j_in_subview_cycles_filesystem_views() {
        let mut state = BrowseState {
            focus: BrowseFocus::Subview,
            ..Default::default()
        };
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![
                ("Usage", false),
                ("Show", true),
                ("Df", false),
                ("Commit Stats", false)
            ]
        );
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![
                ("Usage", false),
                ("Show", false),
                ("Df", true),
                ("Commit Stats", false)
            ]
        );
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![
                ("Usage", false),
                ("Show", false),
                ("Df", false),
                ("Commit Stats", true)
            ]
        );
    }

    // Intent: device subview selection cycles Usage -> Stats.
    // Why it exists: device stats preserve raw btrfs error-counter
    // inspection from the old browse surface.
    // Scenario: user focuses the Devices subview column and presses j.
    #[test]
    fn j_in_subview_cycles_devices_usage_stats() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        state.select_next();
        state.focus = BrowseFocus::Subview;
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![("Usage", false), ("Stats", true)]
        );
    }

    // Intent: Browse command rows append new top-level groups without
    // moving existing entries.
    // Why it exists: Browse muscle memory and ADR 025 ordering depend on
    // existing rows keeping their positions as the inventory grows.
    // Scenario: user opens Browse and scans the Btrfs and NUT command columns.
    #[test]
    fn command_rows_append_new_read_only_groups() {
        let mut state = BrowseState::default();
        assert_eq!(
            state.command_rows(),
            vec![
                ("Filesystem", true),
                ("Devices", false),
                ("Subvolumes", false),
                ("Scrub", false),
                ("Balance", false),
                ("Quota", false),
                ("Inspect", false),
            ]
        );

        state.select_next();
        assert_eq!(
            state.command_rows(),
            vec![
                ("Status", true),
                ("Variables", false),
                ("Commands", false),
                ("Clients", false),
                ("RW Vars", false),
                ("UPSes", false),
            ]
        );

        state.select_next();
        assert_eq!(
            state.command_rows(),
            vec![
                ("Status", true),
                ("Show", false),
                ("Braid", false),
                ("Failed", false),
                ("Timers", false),
                ("Mounts", false),
            ]
        );

        state.select_next();
        assert_eq!(
            state.command_rows(),
            vec![
                ("Scan", true),
                ("Health", false),
                ("Info", false),
                ("Attributes", false),
                ("Self-test Log", false),
                ("Error Log", false),
            ]
        );

        state.select_next();
        assert_eq!(
            state.command_rows(),
            vec![
                ("Tree", true),
                ("Filesystems", false),
                ("Disks", false),
                ("All Columns", false),
                ("SCSI", false),
            ]
        );
    }

    // Intent: new Btrfs command groups expose the expected subview rows.
    // Why it exists: these command groups are raw Browse surfaces, but the
    // selected subview determines which typed command is executed.
    // Scenario: user moves through Subvolumes, Scrub, Quota, and Inspect.
    #[test]
    fn new_btrfs_command_groups_have_expected_subviews() {
        let mut state = BrowseState {
            btrfs_command: BrowseCommand::BtrfsSubvolumes,
            ..Default::default()
        };
        assert_eq!(
            state.subview_rows(),
            vec![
                ("List", true),
                ("Full", false),
                ("Snapshots", false),
                ("Deleted", false),
                ("Default", false),
            ]
        );

        state.btrfs_command = BrowseCommand::BtrfsScrub;
        assert_eq!(
            state.subview_rows(),
            vec![("Status", true), ("Limits", false)]
        );

        state.btrfs_command = BrowseCommand::BtrfsQuota;
        assert_eq!(
            state.subview_rows(),
            vec![("Status", true), ("Qgroups", false)]
        );

        state.btrfs_command = BrowseCommand::BtrfsInspect;
        assert_eq!(state.subview_rows(), vec![("Chunks", true)]);
    }

    // Intent: new Browse selections map to their exact typed command
    // request variants.
    // Why it exists: all raw Browse commands run through CmdRequest, so
    // selection-to-request drift changes the command users see and run.
    // Scenario: user opens every new Btrfs/NUT view with a mounted pool
    // and configured UPS.
    #[test]
    fn new_browse_selections_map_to_expected_requests() {
        let mut state = BrowseState {
            filesystem_subview: FilesystemSubview::CommitStats,
            ..Default::default()
        };
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsFilesystemCommitStats { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsSubvolumes;
        state.subvolume_subview = SubvolumeSubview::Full;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListFull { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Snapshots;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListSnapshots { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Deleted;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListDeleted { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Default;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeGetDefault { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsScrub;
        state.scrub_subview = ScrubSubview::Limits;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsScrubLimit { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsQuota;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsQuotaStatus { .. }
        ));
        state.quota_subview = QuotaSubview::Qgroups;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsQgroupShow { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsInspect;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::BtrfsInspectListChunks { .. }
        ));

        state = BrowseState::default();
        state.program = BrowseProgram::Nut;
        state.nut_command = BrowseCommand::NutClients;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::UpscClients { .. }
        ));
        state.nut_command = BrowseCommand::NutRwVars;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::UpsrwList { .. }
        ));
        state.nut_command = BrowseCommand::NutUpses;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::UpscListUpses
        ));

        state = BrowseState::default();
        state.program = BrowseProgram::Systemd;
        state.systemd_command = BrowseCommand::SystemdStatus;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListUnitsBraidJson
        ));
        state.systemd_command = BrowseCommand::SystemdShow;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListUnitsBraidJson
        ));
        state.systemd_command = BrowseCommand::SystemdBraid;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListUnitsBraid
        ));
        state.systemd_command = BrowseCommand::SystemdFailed;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListUnitsFailed
        ));
        state.systemd_command = BrowseCommand::SystemdTimers;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListTimers
        ));
        state.systemd_command = BrowseCommand::SystemdMounts;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SystemctlListMounts
        ));

        state = BrowseState::default();
        state.program = BrowseProgram::Smartctl;
        state.smartctl_command = BrowseCommand::SmartctlScan;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::SmartctlScan
        ));
        for command in [
            BrowseCommand::SmartctlHealth,
            BrowseCommand::SmartctlInfo,
            BrowseCommand::SmartctlAttributes,
            BrowseCommand::SmartctlSelftestLog,
            BrowseCommand::SmartctlErrorLog,
        ] {
            state = BrowseState::default();
            state.program = BrowseProgram::Smartctl;
            state.smartctl_command = command;
            let disks = disk_inventory();
            let effect =
                state.load_current(&pool(), Some(&ups()), &DiskInventory { by_id: &disks });
            assert!(
                effect.is_none(),
                "smartctl picker should not run {command:?}"
            );
            assert_eq!(
                state.smartctl_devices(),
                &[
                    (
                        "disk1".to_owned(),
                        "/dev/disk/by-id/virtio-disk1".to_owned()
                    ),
                    (
                        "disk2".to_owned(),
                        "/dev/disk/by-id/virtio-disk2".to_owned()
                    ),
                ]
            );
        }

        state = BrowseState::default();
        state.program = BrowseProgram::Lsblk;
        state.lsblk_command = BrowseCommand::LsblkTree;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::LsblkTree
        ));
        state.lsblk_command = BrowseCommand::LsblkFilesystems;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::LsblkFilesystems
        ));
        state.lsblk_command = BrowseCommand::LsblkDisks;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::LsblkDisks
        ));
        state.lsblk_command = BrowseCommand::LsblkAllColumns;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::LsblkAllColumns
        ));
        state.lsblk_command = BrowseCommand::LsblkScsi;
        assert!(matches!(
            browse_request(load_current_for_test(&mut state, &pool(), Some(&ups()))),
            CmdRequest::LsblkScsi
        ));
    }

    // Intent: Btrfs Browse commands show a local empty state while the
    // pool is offline and do not spawn btrfs commands.
    // Why it exists: Browse must remain discoverable without turning an
    // offline pool into background command failures.
    // Scenario: a user opens Browse before running `braid unlock`.
    #[test]
    fn load_current_on_btrfs_offline_pool_returns_none_and_sets_empty_state() {
        let mut state = BrowseState::default();
        let effect = load_current_for_test(&mut state, &PoolStatus::NotMounted, Some(&ups()));
        assert!(effect.is_none());
        assert_eq!(state.empty_state(), Some(BrowseEmptyState::PoolOffline));
    }

    // Intent: NUT Browse commands show a local empty state when the
    // module has no UPS configuration and do not spawn NUT commands.
    // Why it exists: demo mode intentionally has no ups_config, but the
    // NUT menu should still be visible and stable.
    // Scenario: a user navigates to Browse > NUT on a host without UPS.
    #[test]
    fn load_current_on_nut_without_config_returns_none_and_sets_empty_state() {
        let mut state = BrowseState::default();
        state.select_next();
        let effect = load_current_for_test(&mut state, &pool(), None);
        assert!(effect.is_none());
        assert_eq!(
            state.empty_state(),
            Some(BrowseEmptyState::UpsNotConfigured)
        );
    }

    // Intent: NUT > UPSes runs without a configured UPS name.
    // Why it exists: this view is the bootstrap path for discovering the
    // name to put in braid.ups.name.
    // Scenario: a host has braid installed but has not enabled the UPS module.
    #[test]
    fn nut_upses_without_config_runs_discovery_command() {
        let mut state = BrowseState {
            program: BrowseProgram::Nut,
            nut_command: BrowseCommand::NutUpses,
            ..Default::default()
        };

        let effect = load_current_for_test(&mut state, &pool(), None);

        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::UpscListUpses,
                ..
            })
        ));
        assert_eq!(state.empty_state(), None);
    }

    // Intent: SMART per-device commands show a local empty state when braid
    // has no stable disk paths to offer.
    // Why it exists: per-device SMART commands need by-id targets; running an
    // empty picker would make Enter ambiguous.
    // Scenario: user opens Browse > SMART > Health before discovery has found
    // any disks.
    #[test]
    fn smartctl_per_device_without_disks_sets_empty_state() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlHealth,
            ..Default::default()
        };
        let disks = HashMap::new();

        let effect = state.load_current(&pool(), Some(&ups()), &DiskInventory { by_id: &disks });

        assert!(effect.is_none());
        assert_eq!(state.empty_state(), Some(BrowseEmptyState::NoDisksKnown));
    }

    // Intent: SMART scan remains runnable without braid's disk inventory.
    // Why it exists: scan is the bootstrap diagnostic for what smartctl sees
    // independently of braid config.
    // Scenario: user opens Browse > SMART > Scan on a fresh host.
    #[test]
    fn smartctl_scan_runs_without_disks() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlScan,
            ..Default::default()
        };

        let effect = load_current_for_test(&mut state, &pool(), Some(&ups()));

        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::SmartctlScan,
                ..
            })
        ));
    }

    // Intent: name-required NUT views still install the missing-config
    // empty state instead of running partial commands.
    // Why it exists: only UPS discovery can work without braid.ups.name;
    // other NUT views need a concrete target UPS.
    // Scenario: user has not enabled the UPS module and selects every
    // NUT view except UPSes.
    #[test]
    fn nut_views_that_need_a_name_still_require_config() {
        for command in [
            BrowseCommand::NutStatus,
            BrowseCommand::NutVariables,
            BrowseCommand::NutCommands,
            BrowseCommand::NutClients,
            BrowseCommand::NutRwVars,
        ] {
            let mut state = BrowseState {
                program: BrowseProgram::Nut,
                nut_command: command,
                ..Default::default()
            };

            let effect = load_current_for_test(&mut state, &pool(), None);

            assert!(effect.is_none(), "unexpected effect for {command:?}");
            assert_eq!(
                state.empty_state(),
                Some(BrowseEmptyState::UpsNotConfigured),
                "missing empty state for {command:?}",
            );
        }
    }

    // Intent: command scheduling bumps the generation and returns a
    // BrowseRunCommand effect for a mounted Btrfs selection.
    // Why it exists: generation equality is the stale-response guard for
    // raw Browse commands.
    // Scenario: user enters Browse on a mounted pool.
    #[test]
    fn load_current_bumps_generation_and_returns_effect() {
        let mut state = BrowseState::default();
        let effect = load_current_for_test(&mut state, &pool(), Some(&ups()));
        match effect {
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::BtrfsFilesystemUsage { mount_point },
                generation,
            }) => {
                assert_eq!(mount_point.as_str(), "/mnt/storage");
                assert_eq!(generation, 1);
            }
            _ => panic!("unexpected effect"),
        }
    }

    // Intent: Enter on a selected subvolume dispatches a detail request
    // without losing the list output needed for Back.
    // Why it exists: the Browse tab keeps the old standalone browse
    // subvolume drill-in behavior.
    // Scenario: user selects a subvolume row and presses Enter.
    #[test]
    fn enter_in_subvolume_row_drills_in() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        state.select_next();
        state.select_next();
        state.focus = BrowseFocus::Content;
        state.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list /mnt/storage".into(),
                stdout: "ID 256 gen 10 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );

        let disks = HashMap::new();
        let effect = state.enter(&pool(), &DiskInventory { by_id: &disks });
        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::BtrfsSubvolumeShow { .. },
                ..
            })
        ));
        assert!(state.is_detail());
    }

    // Intent: Enter on a selected SMART device dispatches the active
    // per-device SMART request with the device's stable by-id path.
    // Why it exists: SMART health/info/log views are pickers, not raw commands
    // against an implicit current disk.
    // Scenario: user selects disk2 in Browse > SMART > Health and presses Enter.
    // The pool's disk_underlying is empty (no live-path branch), so the
    // dispatched device is the by-id handle.
    #[test]
    fn enter_in_smartctl_device_row_drills_in() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlHealth,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        let disks = disk_inventory();
        let _ = state.load_current(&pool(), Some(&ups()), &DiskInventory { by_id: &disks });
        state.select_next();

        let effect = state.enter(&pool(), &DiskInventory { by_id: &disks });

        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::SmartctlHealth { device },
                ..
            }) if device == "/dev/disk/by-id/virtio-disk2"
        ));
        assert!(state.is_detail());
    }

    // Intent: Enter on a selected Systemd unit dispatches the active
    // per-unit detail request.
    // Why it exists: Status/Show share the same JSON picker but must drill
    // into different systemctl subcommands.
    // Scenario: user selects braid-online.service in Browse > Systemd > Status
    // and presses Enter.
    #[test]
    fn enter_in_systemd_unit_row_drills_in() {
        let mut state = BrowseState {
            program: BrowseProgram::Systemd,
            systemd_command: BrowseCommand::SystemdStatus,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        state.command_finished(
            RawCommandOutput {
                cmd: "systemctl list-units --output=json --all braid-* hddfancontrol-braid.service"
                    .into(),
                stdout: r#"[{"unit":"braid-online.service","load":"loaded","active":"active","sub":"exited","description":"online"}]"#
                    .into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        let disks = HashMap::new();

        let effect = state.enter(&pool(), &DiskInventory { by_id: &disks });

        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::SystemctlStatusUnit { unit },
                ..
            }) if unit == "braid-online.service"
        ));
        assert!(state.is_detail());
    }

    // Intent: only Subvolumes > List supports parsed-table drill-in.
    // Why it exists: the other subvolume views use richer/different raw
    // output shapes that the existing parser and detail path do not own.
    // Scenario: user selects Subvolumes > Full and presses Enter in content.
    #[test]
    fn non_list_subvolume_views_do_not_drill_in() {
        let mut state = BrowseState {
            btrfs_command: BrowseCommand::BtrfsSubvolumes,
            subvolume_subview: SubvolumeSubview::Full,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        state.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list -a /mnt/storage".into(),
                stdout: "ID 256 gen 10 parent 5 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );

        let disks = HashMap::new();
        let effect = state.enter(&pool(), &DiskInventory { by_id: &disks });

        assert!(effect.is_none());
        assert!(!state.is_detail());
        assert!(state.subvolumes().is_empty());
    }

    // Intent: Esc/Backspace from subvolume detail restores the cached
    // list and invalidates the in-flight detail command.
    // Why it exists: late detail output must not overwrite the list view.
    // Scenario: user drills into a subvolume and immediately backs out.
    #[test]
    fn esc_pops_back() {
        let mut state = BrowseState {
            focus: BrowseFocus::Command,
            ..Default::default()
        };
        state.select_next();
        state.select_next();
        state.focus = BrowseFocus::Content;
        state.output = vec!["ID 256 gen 10 top level 5 path data".into()];
        state.subvolumes = vec![BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];
        let disks = HashMap::new();
        let _ = state.enter(&pool(), &DiskInventory { by_id: &disks });
        state.back();
        assert!(!state.is_detail());
        assert_eq!(
            state.output(),
            &["ID 256 gen 10 top level 5 path data".to_owned()]
        );
    }

    // Intent: Esc/Backspace from SMART detail restores the device picker.
    // Why it exists: late detail output should not strand the user away from
    // the selected device list.
    // Scenario: user drills into SMART health for a disk and backs out.
    #[test]
    fn esc_pops_back_from_smartctl_detail() {
        let mut state = BrowseState {
            program: BrowseProgram::Smartctl,
            smartctl_command: BrowseCommand::SmartctlHealth,
            focus: BrowseFocus::Content,
            ..Default::default()
        };
        let disks = disk_inventory();
        let _ = state.load_current(&pool(), Some(&ups()), &DiskInventory { by_id: &disks });
        let _ = state.enter(&pool(), &DiskInventory { by_id: &disks });

        state.back();

        assert!(!state.is_detail());
        assert_eq!(state.smartctl_devices().len(), 2);
        assert_eq!(state.output(), &["press Enter for SMART data".to_owned()]);
    }
}
