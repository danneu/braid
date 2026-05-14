use std::cell::Cell;
use std::collections::HashMap;

use crate::cmd::{CmdRequest, RawCommandOutput};
use crate::config::Ups;
use crate::parse::parse_btrfs_subvolume_list;
use crate::parse::types::BtrfsSubvolume;
use crate::tui::effect::Effect;
use crate::tui::model::PoolStatus;
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
}

impl BrowseProgram {
    const ALL: [Self; 2] = [Self::Btrfs, Self::Nut];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Btrfs => "Btrfs",
            Self::Nut => "NUT",
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
}

impl BrowseEmptyState {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::PoolOffline => "pool not mounted -- run `braid unlock` to access btrfs data",
            Self::UpsNotConfigured => {
                "UPS not configured -- set `ups.name` in the braid NixOS module"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseMode {
    Normal,
    SubvolDetail,
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
}

#[derive(Clone, Default)]
struct CachedOutput {
    output: Vec<String>,
    subvolumes: Vec<BtrfsSubvolume>,
}

/// State owned by the `tui` model for the Browse tab. It centralizes
/// sidebar selection, command generations, raw output cache, and
/// subvolume drill-in so update and view code share one Browse contract.
pub(crate) struct BrowseState {
    pub(crate) focus: BrowseFocus,
    program: BrowseProgram,
    btrfs_command: BrowseCommand,
    nut_command: BrowseCommand,
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
    force_reload_once: bool,
}

impl Default for BrowseState {
    fn default() -> Self {
        Self {
            focus: BrowseFocus::Program,
            program: BrowseProgram::Btrfs,
            btrfs_command: BrowseCommand::BtrfsFilesystem,
            nut_command: BrowseCommand::NutStatus,
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
            force_reload_once: false,
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
            self.subvolumes.clear();
            return None;
        }

        if matches!(selection, BrowseSelection::NutCommands)
            && !force_reload
            && let Some(cached) = self.cache.get(&selection).cloned()
        {
            self.output = cached.output;
            self.subvolumes = cached.subvolumes;
            return None;
        }

        if let Some(cached) = self.cache.get(&selection).cloned() {
            self.output = cached.output;
            self.subvolumes = cached.subvolumes;
        } else {
            self.output.clear();
            self.subvolumes.clear();
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

    /// Drill into the selected content row when Browse owns a drill-in
    /// surface, currently the Btrfs subvolume list.
    pub(crate) fn enter(&mut self, pool: &PoolStatus) -> Option<Effect> {
        if self.focus != BrowseFocus::Content
            || self.mode != BrowseMode::Normal
            || !self.is_subvolume_list()
            || self.subvolumes.is_empty()
        {
            return None;
        }
        let pool = pool.current()?;
        let subvol = &self.subvolumes[self.subvol_selected];
        let path = format!("{}/{}", pool.mount_point.as_str(), subvol.path);
        self.mode = BrowseMode::SubvolDetail;
        self.subvol_list_output = self.output.clone();
        self.dispatch(CmdRequest::BtrfsSubvolumeShow { path })
    }

    /// Return from drill-in content to the cached list, invalidating the
    /// in-flight detail command so stale detail output cannot overwrite it.
    pub(crate) fn back(&mut self) {
        if self.mode == BrowseMode::SubvolDetail {
            self.mode = BrowseMode::Normal;
            self.output = self.subvol_list_output.clone();
            self.scroll_offset = 0;
            self.command_gen = self.command_gen.saturating_add(1);
            self.loading = false;
        }
    }

    /// Reload the currently open subvolume detail while preserving the
    /// sidebar selection that owns the list beneath it.
    pub(crate) fn reload_detail(&mut self, pool: &PoolStatus) -> Option<Effect> {
        if self.mode != BrowseMode::SubvolDetail || self.subvolumes.is_empty() {
            return None;
        }
        let pool = pool.current()?;
        let subvol = &self.subvolumes[self.subvol_selected];
        let path = format!("{}/{}", pool.mount_point.as_str(), subvol.path);
        self.dispatch(CmdRequest::BtrfsSubvolumeShow { path })
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
            }
        }

        if self.mode == BrowseMode::Normal {
            self.cache.insert(
                self.current_selection(),
                CachedOutput {
                    output: self.output.clone(),
                    subvolumes: self.subvolumes.clone(),
                },
            );
        }
    }

    /// Page the content area down by one viewport, clamping at the last
    /// full viewport start.
    pub(crate) fn page_down(&mut self) {
        let page = self.viewport_height.get() as usize;
        let max_scroll = self.output.len().saturating_sub(page);
        self.scroll_offset = (self.scroll_offset + page).min(max_scroll);
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

    pub(crate) fn is_subvolume_detail(&self) -> bool {
        self.mode == BrowseMode::SubvolDetail
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

    pub(crate) fn command_display(
        &self,
        mount_point: &MountPoint,
        ups_config: Option<&Ups>,
    ) -> Option<String> {
        let request = match self.current_selection() {
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Usage) => {
                CmdRequest::BtrfsFilesystemUsage {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Show) => {
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Df) => {
                CmdRequest::BtrfsFilesystemDf {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::CommitStats) => {
                CmdRequest::BtrfsFilesystemCommitStats {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsDevices(DeviceSubview::Usage) => CmdRequest::BtrfsDeviceUsage {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsDevices(DeviceSubview::Stats) => CmdRequest::BtrfsDeviceStats {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::List) => {
                CmdRequest::BtrfsSubvolumeList {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Full) => {
                CmdRequest::BtrfsSubvolumeListFull {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Snapshots) => {
                CmdRequest::BtrfsSubvolumeListSnapshots {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Deleted) => {
                CmdRequest::BtrfsSubvolumeListDeleted {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Default) => {
                CmdRequest::BtrfsSubvolumeGetDefault {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Status) => {
                CmdRequest::BtrfsScrubStatusHuman {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Limits) => CmdRequest::BtrfsScrubLimit {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsBalance => CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsQuota(QuotaSubview::Status) => CmdRequest::BtrfsQuotaStatus {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsQuota(QuotaSubview::Qgroups) => CmdRequest::BtrfsQgroupShow {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsInspect(InspectSubview::Chunks) => {
                CmdRequest::BtrfsInspectListChunks {
                    mount_point: mount_point.clone(),
                }
            }
            BrowseSelection::NutStatus | BrowseSelection::NutVariables => CmdRequest::UpscQuery {
                name: ups_config?.name.clone(),
            },
            BrowseSelection::NutCommands => CmdRequest::UpscmdList {
                name: ups_config?.name.clone(),
            },
            BrowseSelection::NutClients => CmdRequest::UpscClients {
                name: ups_config?.name.clone(),
            },
            BrowseSelection::NutRwVars => CmdRequest::UpsrwList {
                name: ups_config?.name.clone(),
            },
            BrowseSelection::NutUpses => CmdRequest::UpscListUpses,
        };
        Some(request.to_argv().to_shell_string())
    }

    fn install_empty(&mut self, state: BrowseEmptyState) {
        self.output.clear();
        self.subvolumes.clear();
        self.empty_state = Some(state);
    }

    fn dispatch(&mut self, request: CmdRequest) -> Option<Effect> {
        self.scroll_offset = 0;
        self.command_gen = self.command_gen.saturating_add(1);
        self.loading = true;
        self.empty_state = None;
        self.output.clear();
        Some(Effect::BrowseRunCommand {
            request,
            generation: self.command_gen,
        })
    }

    fn current_request(&self, pool: &PoolStatus, ups_config: Option<&Ups>) -> Option<CmdRequest> {
        let mount_point = pool.current().map(|p| &p.mount_point);
        match self.current_selection() {
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Usage) => {
                Some(CmdRequest::BtrfsFilesystemUsage {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Show) => {
                Some(CmdRequest::BtrfsFilesystemShow {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::Df) => {
                Some(CmdRequest::BtrfsFilesystemDf {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsFilesystem(FilesystemSubview::CommitStats) => {
                Some(CmdRequest::BtrfsFilesystemCommitStats {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsDevices(DeviceSubview::Usage) => {
                Some(CmdRequest::BtrfsDeviceUsage {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsDevices(DeviceSubview::Stats) => {
                Some(CmdRequest::BtrfsDeviceStats {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::List) => {
                Some(CmdRequest::BtrfsSubvolumeList {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Full) => {
                Some(CmdRequest::BtrfsSubvolumeListFull {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Snapshots) => {
                Some(CmdRequest::BtrfsSubvolumeListSnapshots {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Deleted) => {
                Some(CmdRequest::BtrfsSubvolumeListDeleted {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsSubvolumes(SubvolumeSubview::Default) => {
                Some(CmdRequest::BtrfsSubvolumeGetDefault {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Status) => {
                Some(CmdRequest::BtrfsScrubStatusHuman {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsScrub(ScrubSubview::Limits) => {
                Some(CmdRequest::BtrfsScrubLimit {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsBalance => Some(CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point?.clone(),
            }),
            BrowseSelection::BtrfsQuota(QuotaSubview::Status) => {
                Some(CmdRequest::BtrfsQuotaStatus {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsQuota(QuotaSubview::Qgroups) => {
                Some(CmdRequest::BtrfsQgroupShow {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::BtrfsInspect(InspectSubview::Chunks) => {
                Some(CmdRequest::BtrfsInspectListChunks {
                    mount_point: mount_point?.clone(),
                })
            }
            BrowseSelection::NutStatus | BrowseSelection::NutVariables => None,
            BrowseSelection::NutCommands => Some(CmdRequest::UpscmdList {
                name: ups_config?.name.clone(),
            }),
            BrowseSelection::NutClients => Some(CmdRequest::UpscClients {
                name: ups_config?.name.clone(),
            }),
            BrowseSelection::NutRwVars => Some(CmdRequest::UpsrwList {
                name: ups_config?.name.clone(),
            }),
            BrowseSelection::NutUpses => Some(CmdRequest::UpscListUpses),
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
        }
    }

    fn current_command(&self) -> BrowseCommand {
        match self.program {
            BrowseProgram::Btrfs => self.btrfs_command,
            BrowseProgram::Nut => self.nut_command,
        }
    }

    fn commands(&self) -> &'static [BrowseCommand] {
        match self.program {
            BrowseProgram::Btrfs => &BrowseCommand::BTRFS,
            BrowseProgram::Nut => &BrowseCommand::NUT,
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
        } else {
            let max_scroll = self
                .output
                .len()
                .saturating_sub(self.viewport_height.get() as usize);
            self.scroll_offset = (self.scroll_offset + 1).min(max_scroll);
        }
    }

    fn content_up(&mut self) {
        if self.is_subvolume_list() && !self.subvolumes.is_empty() {
            self.subvol_selected = self.subvol_selected.saturating_sub(1);
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
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;
    use crate::alert::AlertState;
    use crate::status::BalanceReport;
    use crate::tui::model::PoolState;

    fn pool() -> PoolStatus {
        PoolStatus::Mounted(PoolState {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            df_entries: vec![],
            disk_usage: HashMap::new(),
            disk_transport: HashMap::new(),
            smart_health: HashMap::new(),
            disk_temperature_readings: HashMap::new(),
            device_errors: HashMap::new(),
            unpooled_disks: HashMap::new(),
            alert_state: AlertState::default(),
            scrub: crate::parse::types::ScrubState::Unknown,
            balance: BalanceReport::Idle,
            capacity_total_bytes: None,
            capacity_used_bytes: 0,
            probed_at: Instant::now(),
        })
    }

    fn ups() -> Ups {
        Ups { name: "ups".into() }
    }

    fn browse_request(effect: Option<Effect>) -> CmdRequest {
        match effect {
            Some(Effect::BrowseRunCommand { request, .. }) => request,
            _ => panic!("expected BrowseRunCommand effect"),
        }
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Content;
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
        state.select_next();
        state.focus_right();
        assert_eq!(state.focus(), BrowseFocus::Subview);
    }

    // Intent: j in Program cycles between Btrfs and NUT.
    // Why it exists: Program is the top-level Browse inventory and must
    // stay keyboard-reachable.
    // Scenario: user presses j twice in the Program column.
    #[test]
    fn j_in_program_cycles_btrfs_nut() {
        let mut state = BrowseState::default();
        state.select_next();
        assert_eq!(state.program_rows(), vec![("Btrfs", false), ("NUT", true)]);
        state.select_next();
        assert_eq!(state.program_rows(), vec![("Btrfs", true), ("NUT", false)]);
    }

    // Intent: filesystem subview selection cycles through all filesystem views.
    // Why it exists: all raw filesystem commands are exposed under
    // one command group.
    // Scenario: user focuses the subview column and presses j repeatedly.
    #[test]
    fn j_in_subview_cycles_filesystem_views() {
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Subview;
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
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
    }

    // Intent: new Btrfs command groups expose the expected subview rows.
    // Why it exists: these command groups are raw Browse surfaces, but the
    // selected subview determines which typed command is executed.
    // Scenario: user moves through Subvolumes, Scrub, Quota, and Inspect.
    #[test]
    fn new_btrfs_command_groups_have_expected_subviews() {
        let mut state = BrowseState::default();

        state.btrfs_command = BrowseCommand::BtrfsSubvolumes;
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
        let mut state = BrowseState::default();
        state.filesystem_subview = FilesystemSubview::CommitStats;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsFilesystemCommitStats { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsSubvolumes;
        state.subvolume_subview = SubvolumeSubview::Full;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListFull { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Snapshots;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListSnapshots { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Deleted;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeListDeleted { .. }
        ));
        state.subvolume_subview = SubvolumeSubview::Default;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsSubvolumeGetDefault { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsScrub;
        state.scrub_subview = ScrubSubview::Limits;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsScrubLimit { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsQuota;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsQuotaStatus { .. }
        ));
        state.quota_subview = QuotaSubview::Qgroups;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsQgroupShow { .. }
        ));

        state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsInspect;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::BtrfsInspectListChunks { .. }
        ));

        state = BrowseState::default();
        state.program = BrowseProgram::Nut;
        state.nut_command = BrowseCommand::NutClients;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::UpscClients { .. }
        ));
        state.nut_command = BrowseCommand::NutRwVars;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::UpsrwList { .. }
        ));
        state.nut_command = BrowseCommand::NutUpses;
        assert!(matches!(
            browse_request(state.load_current(&pool(), Some(&ups()))),
            CmdRequest::UpscListUpses
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
        let effect = state.load_current(&PoolStatus::NotMounted, Some(&ups()));
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
        let effect = state.load_current(&pool(), None);
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
        let mut state = BrowseState::default();
        state.program = BrowseProgram::Nut;
        state.nut_command = BrowseCommand::NutUpses;

        let effect = state.load_current(&pool(), None);

        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::UpscListUpses,
                ..
            })
        ));
        assert_eq!(state.empty_state(), None);
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
            let mut state = BrowseState::default();
            state.program = BrowseProgram::Nut;
            state.nut_command = command;

            let effect = state.load_current(&pool(), None);

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
        let effect = state.load_current(&pool(), Some(&ups()));
        match effect {
            Some(Effect::BrowseRunCommand {
                request:
                    CmdRequest::BtrfsFilesystemUsage {
                        mount_point: MountPoint(mp),
                    },
                generation,
            }) => {
                assert_eq!(mp, "/mnt/storage");
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
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
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

        let effect = state.enter(&pool());
        assert!(matches!(
            effect,
            Some(Effect::BrowseRunCommand {
                request: CmdRequest::BtrfsSubvolumeShow { .. },
                ..
            })
        ));
        assert!(state.is_subvolume_detail());
    }

    // Intent: only Subvolumes > List supports parsed-table drill-in.
    // Why it exists: the other subvolume views use richer/different raw
    // output shapes that the existing parser and detail path do not own.
    // Scenario: user selects Subvolumes > Full and presses Enter in content.
    #[test]
    fn non_list_subvolume_views_do_not_drill_in() {
        let mut state = BrowseState::default();
        state.btrfs_command = BrowseCommand::BtrfsSubvolumes;
        state.subvolume_subview = SubvolumeSubview::Full;
        state.focus = BrowseFocus::Content;
        state.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list -a /mnt/storage".into(),
                stdout: "ID 256 gen 10 parent 5 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );

        let effect = state.enter(&pool());

        assert!(effect.is_none());
        assert!(!state.is_subvolume_detail());
        assert!(state.subvolumes().is_empty());
    }

    // Intent: Esc/Backspace from subvolume detail restores the cached
    // list and invalidates the in-flight detail command.
    // Why it exists: late detail output must not overwrite the list view.
    // Scenario: user drills into a subvolume and immediately backs out.
    #[test]
    fn esc_pops_back() {
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
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
        let _ = state.enter(&pool());
        state.back();
        assert!(!state.is_subvolume_detail());
        assert_eq!(
            state.output(),
            &["ID 256 gen 10 top level 5 path data".to_owned()]
        );
    }
}
