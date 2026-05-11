use ratatui::widgets::TableState;

use crate::cmd::CmdRequest;
use crate::parse::types::BtrfsSubvolume;
use crate::types::MountPoint;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Filesystem,
    Devices,
    Subvolumes,
    Scrub,
    Balance,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Filesystem,
        Tab::Devices,
        Tab::Subvolumes,
        Tab::Scrub,
        Tab::Balance,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Filesystem => "Filesystem",
            Tab::Devices => "Devices",
            Tab::Subvolumes => "Subvolumes",
            Tab::Scrub => "Scrub",
            Tab::Balance => "Balance",
        }
    }

    pub fn next(self) -> Tab {
        match self {
            Tab::Filesystem => Tab::Devices,
            Tab::Devices => Tab::Subvolumes,
            Tab::Subvolumes => Tab::Scrub,
            Tab::Scrub => Tab::Balance,
            Tab::Balance => Tab::Filesystem,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Filesystem => Tab::Balance,
            Tab::Devices => Tab::Filesystem,
            Tab::Subvolumes => Tab::Devices,
            Tab::Scrub => Tab::Subvolumes,
            Tab::Balance => Tab::Scrub,
        }
    }

    pub fn subtabs(self) -> &'static [SubTab] {
        match self {
            Tab::Filesystem => &[SubTab::FsUsage, SubTab::FsShow, SubTab::FsDf],
            Tab::Devices => &[SubTab::DevUsage, SubTab::DevStats],
            Tab::Subvolumes => &[SubTab::SubvolList],
            Tab::Scrub => &[SubTab::ScrubStatus],
            Tab::Balance => &[SubTab::BalanceStatus],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tab;

    // Intent: Tab::next returns the variant that follows self in the user-facing
    // cycle, wrapping Tab::Balance back to Tab::Filesystem.
    // Why it exists: this refactor replaces an ALL.position(...).unwrap() lookup
    // with an exhaustive match. Without an explicit cycle assertion, an
    // accidental swap of two arms would compile and silently misnavigate the TUI.
    // Scenario: a user in `braid browse` presses Tab from each starting tab and
    // sees the next tab become active, including the wrap from the last tab back
    // to the first.
    #[test]
    fn tab_next_cycles_forward_through_all_variants() {
        assert_eq!(Tab::Filesystem.next(), Tab::Devices);
        assert_eq!(Tab::Devices.next(), Tab::Subvolumes);
        assert_eq!(Tab::Subvolumes.next(), Tab::Scrub);
        assert_eq!(Tab::Scrub.next(), Tab::Balance);
        assert_eq!(Tab::Balance.next(), Tab::Filesystem);
    }

    // Intent: Tab::prev returns the variant that precedes self in the user-facing
    // cycle, wrapping Tab::Filesystem back to Tab::Balance.
    // Why it exists: symmetric guard to tab_next_cycles_forward_through_all_variants;
    // the new exhaustive-match prev() is a separate set of arms, so a swap there
    // is not caught by the next() test.
    // Scenario: a user in `braid browse` presses Shift+Tab from each starting tab
    // and sees the previous tab become active, including the wrap from the first
    // tab back to the last.
    #[test]
    fn tab_prev_cycles_backward_through_all_variants() {
        assert_eq!(Tab::Filesystem.prev(), Tab::Balance);
        assert_eq!(Tab::Devices.prev(), Tab::Filesystem);
        assert_eq!(Tab::Subvolumes.prev(), Tab::Devices);
        assert_eq!(Tab::Scrub.prev(), Tab::Subvolumes);
        assert_eq!(Tab::Balance.prev(), Tab::Scrub);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SubTab {
    FsUsage,
    FsShow,
    FsDf,
    DevUsage,
    DevStats,
    SubvolList,
    ScrubStatus,
    BalanceStatus,
}

impl SubTab {
    pub fn label(self) -> &'static str {
        match self {
            SubTab::FsUsage => "Usage",
            SubTab::FsShow => "Show",
            SubTab::FsDf => "Df",
            SubTab::DevUsage => "Usage",
            SubTab::DevStats => "Stats",
            SubTab::SubvolList => "List",
            SubTab::ScrubStatus => "Status",
            SubTab::BalanceStatus => "Status",
        }
    }

    pub fn request(self, mount_point: &MountPoint) -> CmdRequest {
        match self {
            SubTab::FsUsage => CmdRequest::BtrfsFilesystemUsage {
                mount_point: mount_point.clone(),
            },
            SubTab::FsShow => CmdRequest::BtrfsFilesystemShow {
                mount_point: mount_point.clone(),
            },
            SubTab::FsDf => CmdRequest::BtrfsFilesystemDf {
                mount_point: mount_point.clone(),
            },
            SubTab::DevUsage => CmdRequest::BtrfsDeviceUsage {
                mount_point: mount_point.clone(),
            },
            SubTab::DevStats => CmdRequest::BtrfsDeviceStats {
                mount_point: mount_point.clone(),
            },
            SubTab::SubvolList => CmdRequest::BtrfsSubvolumeList {
                mount_point: mount_point.clone(),
            },
            SubTab::ScrubStatus => CmdRequest::BtrfsScrubStatusHuman {
                mount_point: mount_point.clone(),
            },
            SubTab::BalanceStatus => CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Normal,
    SubvolDetail,
    Help,
}

pub struct Model {
    pub running: bool,
    pub mode: ViewMode,
    pub help_return_mode: ViewMode,
    pub mount_point: MountPoint,
    pub tab: Tab,
    pub subtab_index: usize,
    pub output: Vec<String>,
    pub scroll_offset: usize,
    pub loading: bool,
    pub frame: u64,
    pub command_gen: u64,
    pub subvolumes: Vec<BtrfsSubvolume>,
    pub subvol_selected: usize,
    pub viewport_height: u16,
    /// Stored output for the subvol list so we can restore it on Back.
    pub subvol_list_output: Vec<String>,
    pub subvol_table_state: TableState,
}

impl Model {
    pub fn new(mount_point: MountPoint) -> (Self, Vec<super::Effect>) {
        let request = SubTab::FsUsage.request(&mount_point);
        let model = Self {
            running: true,
            mode: ViewMode::Normal,
            help_return_mode: ViewMode::Normal,
            mount_point,
            tab: Tab::Filesystem,
            subtab_index: 0,
            output: Vec::new(),
            scroll_offset: 0,
            loading: true,
            frame: 0,
            command_gen: 1,
            subvolumes: Vec::new(),
            subvol_selected: 0,
            viewport_height: 20,
            subvol_list_output: Vec::new(),
            subvol_table_state: TableState::default(),
        };
        let effects = vec![super::Effect::RunCommand {
            request,
            generation: 1,
        }];
        (model, effects)
    }

    pub fn current_subtab(&self) -> SubTab {
        self.tab.subtabs()[self.subtab_index]
    }

    pub fn current_command_display(&self) -> String {
        self.current_subtab()
            .request(&self.mount_point)
            .to_argv()
            .to_shell_string()
    }

    #[cfg(test)]
    pub fn new_demo(mount_point: &str, tab: Tab, output: Vec<String>) -> Self {
        Self {
            running: true,
            mode: ViewMode::Normal,
            help_return_mode: ViewMode::Normal,
            mount_point: MountPoint(mount_point.to_owned()),
            tab,
            subtab_index: 0,
            output,
            scroll_offset: 0,
            loading: false,
            frame: 0,
            command_gen: 0,
            subvolumes: Vec::new(),
            subvol_selected: 0,
            viewport_height: 20,
            subvol_list_output: Vec::new(),
            subvol_table_state: TableState::default(),
        }
    }
}
