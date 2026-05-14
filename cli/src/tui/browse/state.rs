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
    NutStatus,
    NutVariables,
    NutCommands,
}

impl BrowseCommand {
    const BTRFS: [Self; 5] = [
        Self::BtrfsFilesystem,
        Self::BtrfsDevices,
        Self::BtrfsSubvolumes,
        Self::BtrfsScrub,
        Self::BtrfsBalance,
    ];
    const NUT: [Self; 3] = [Self::NutStatus, Self::NutVariables, Self::NutCommands];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::BtrfsFilesystem => "Filesystem",
            Self::BtrfsDevices => "Devices",
            Self::BtrfsSubvolumes => "Subvolumes",
            Self::BtrfsScrub => "Scrub",
            Self::BtrfsBalance => "Balance",
            Self::NutStatus => "Status",
            Self::NutVariables => "Variables",
            Self::NutCommands => "Commands",
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
}

impl FilesystemSubview {
    const ALL: [Self; 3] = [Self::Usage, Self::Show, Self::Df];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Usage => "Usage",
            Self::Show => "Show",
            Self::Df => "Df",
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
    BtrfsSubvolumes,
    BtrfsScrub,
    BtrfsBalance,
    NutStatus,
    NutVariables,
    NutCommands,
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
        if self.is_nut_selected() && ups_config.is_none() {
            self.install_empty(BrowseEmptyState::UpsNotConfigured);
            return None;
        }

        if matches!(
            selection,
            BrowseSelection::NutStatus | BrowseSelection::NutVariables
        ) {
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
            || self.current_selection() != BrowseSelection::BtrfsSubvolumes
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

        if self.current_selection() == BrowseSelection::BtrfsSubvolumes
            && self.mode == BrowseMode::Normal
        {
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
            _ => Vec::new(),
        }
    }

    pub(crate) fn has_subviews(&self) -> bool {
        matches!(
            self.current_command(),
            BrowseCommand::BtrfsFilesystem | BrowseCommand::BtrfsDevices
        )
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn empty_state(&self) -> Option<BrowseEmptyState> {
        self.empty_state
    }

    pub(crate) fn is_subvolume_list(&self) -> bool {
        self.current_selection() == BrowseSelection::BtrfsSubvolumes
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
            BrowseSelection::BtrfsDevices(DeviceSubview::Usage) => CmdRequest::BtrfsDeviceUsage {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsDevices(DeviceSubview::Stats) => CmdRequest::BtrfsDeviceStats {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsSubvolumes => CmdRequest::BtrfsSubvolumeList {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsScrub => CmdRequest::BtrfsScrubStatusHuman {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::BtrfsBalance => CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point.clone(),
            },
            BrowseSelection::NutStatus | BrowseSelection::NutVariables => CmdRequest::UpscQuery {
                name: ups_config?.name.clone(),
            },
            BrowseSelection::NutCommands => CmdRequest::UpscmdList {
                name: ups_config?.name.clone(),
            },
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
            BrowseSelection::BtrfsSubvolumes => Some(CmdRequest::BtrfsSubvolumeList {
                mount_point: mount_point?.clone(),
            }),
            BrowseSelection::BtrfsScrub => Some(CmdRequest::BtrfsScrubStatusHuman {
                mount_point: mount_point?.clone(),
            }),
            BrowseSelection::BtrfsBalance => Some(CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point?.clone(),
            }),
            BrowseSelection::NutStatus | BrowseSelection::NutVariables => None,
            BrowseSelection::NutCommands => Some(CmdRequest::UpscmdList {
                name: ups_config?.name.clone(),
            }),
        }
    }

    fn current_selection(&self) -> BrowseSelection {
        match self.current_command() {
            BrowseCommand::BtrfsFilesystem => {
                BrowseSelection::BtrfsFilesystem(self.filesystem_subview)
            }
            BrowseCommand::BtrfsDevices => BrowseSelection::BtrfsDevices(self.device_subview),
            BrowseCommand::BtrfsSubvolumes => BrowseSelection::BtrfsSubvolumes,
            BrowseCommand::BtrfsScrub => BrowseSelection::BtrfsScrub,
            BrowseCommand::BtrfsBalance => BrowseSelection::BtrfsBalance,
            BrowseCommand::NutStatus => BrowseSelection::NutStatus,
            BrowseCommand::NutVariables => BrowseSelection::NutVariables,
            BrowseCommand::NutCommands => BrowseSelection::NutCommands,
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

    fn is_nut_selected(&self) -> bool {
        self.program == BrowseProgram::Nut
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
    // Scenario: user selects Btrfs Subvolumes and presses l from Command.
    #[test]
    fn l_from_command_skips_subview_when_no_subviews() {
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Command;
        state.select_next();
        state.select_next();
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

    // Intent: filesystem subview selection cycles Usage -> Show -> Df.
    // Why it exists: all three raw filesystem commands are exposed under
    // one command group.
    // Scenario: user focuses the subview column and presses j repeatedly.
    #[test]
    fn j_in_subview_cycles_filesystem_usage_show_df() {
        let mut state = BrowseState::default();
        state.focus = BrowseFocus::Subview;
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![("Usage", false), ("Show", true), ("Df", false)]
        );
        state.select_next();
        assert_eq!(
            state.subview_rows(),
            vec![("Usage", false), ("Show", false), ("Df", true)]
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
