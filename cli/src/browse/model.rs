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
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap();
        Self::ALL[(idx + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Tab {
        let idx = Self::ALL.iter().position(|t| *t == self).unwrap();
        Self::ALL[(idx + Self::ALL.len() - 1) % Self::ALL.len()]
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
            SubTab::ScrubStatus => CmdRequest::BtrfsScrubStatus {
                mount_point: mount_point.clone(),
            },
            SubTab::BalanceStatus => CmdRequest::BtrfsBalanceStatus {
                mount_point: mount_point.clone(),
            },
        }
    }

    pub fn command_display(self, mount_point: &str) -> String {
        match self {
            SubTab::FsUsage => format!("btrfs filesystem usage {mount_point}"),
            SubTab::FsShow => format!("btrfs filesystem show {mount_point}"),
            SubTab::FsDf => format!("btrfs filesystem df {mount_point}"),
            SubTab::DevUsage => format!("btrfs device usage {mount_point}"),
            SubTab::DevStats => format!("btrfs device stats {mount_point}"),
            SubTab::SubvolList => format!("btrfs subvolume list {mount_point}"),
            SubTab::ScrubStatus => format!("btrfs scrub status {mount_point}"),
            SubTab::BalanceStatus => format!("btrfs balance status {mount_point}"),
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
}

impl Model {
    pub fn new(mount_point: MountPoint) -> (Self, Vec<super::Effect>) {
        let request = SubTab::FsUsage.request(&mount_point);
        let model = Self {
            running: true,
            mode: ViewMode::Normal,
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
            .command_display(self.mount_point.as_str())
    }

    #[cfg(test)]
    pub fn new_demo(mount_point: &str, tab: Tab, output: Vec<String>) -> Self {
        Self {
            running: true,
            mode: ViewMode::Normal,
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
        }
    }
}
