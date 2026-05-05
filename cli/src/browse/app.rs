use crate::cmd::{CmdRequest, RawCommandOutput};
use crate::parse::parse_btrfs_subvolume_list;

use super::Effect;
use super::model::{Model, SubTab, Tab, ViewMode};

pub enum Message {
    Quit,
    ToggleHelp,
    NextTab,
    PrevTab,
    NextSubTab,
    PrevSubTab,
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    Select,
    Back,
    Reload,
    CommandFinished {
        raw: RawCommandOutput,
        generation: u64,
    },
}

pub fn update(model: &mut Model, msg: Message) -> Vec<Effect> {
    match msg {
        Message::Quit => {
            model.running = false;
            vec![]
        }
        Message::ToggleHelp => {
            model.mode = match model.mode {
                ViewMode::Help => model.help_return_mode,
                other => {
                    model.help_return_mode = other;
                    ViewMode::Help
                }
            };
            vec![]
        }
        Message::NextTab => {
            model.tab = model.tab.next();
            model.subtab_index = 0;
            switch_command(model)
        }
        Message::PrevTab => {
            model.tab = model.tab.prev();
            model.subtab_index = 0;
            switch_command(model)
        }
        Message::NextSubTab => {
            let subtabs = model.tab.subtabs();
            if subtabs.len() > 1 {
                model.subtab_index = (model.subtab_index + 1) % subtabs.len();
                switch_command(model)
            } else {
                vec![]
            }
        }
        Message::PrevSubTab => {
            let subtabs = model.tab.subtabs();
            if subtabs.len() > 1 {
                model.subtab_index = (model.subtab_index + subtabs.len() - 1) % subtabs.len();
                switch_command(model)
            } else {
                vec![]
            }
        }
        Message::ScrollDown => {
            if model.tab == Tab::Subvolumes
                && model.mode == ViewMode::Normal
                && !model.subvolumes.is_empty()
            {
                let max = model.subvolumes.len().saturating_sub(1);
                model.subvol_selected = (model.subvol_selected + 1).min(max);
            } else {
                let max_scroll = model
                    .output
                    .len()
                    .saturating_sub(model.viewport_height as usize);
                model.scroll_offset = (model.scroll_offset + 1).min(max_scroll);
            }
            vec![]
        }
        Message::ScrollUp => {
            if model.tab == Tab::Subvolumes
                && model.mode == ViewMode::Normal
                && !model.subvolumes.is_empty()
            {
                model.subvol_selected = model.subvol_selected.saturating_sub(1);
            } else {
                model.scroll_offset = model.scroll_offset.saturating_sub(1);
            }
            vec![]
        }
        Message::PageDown => {
            let page = model.viewport_height as usize;
            let max_scroll = model
                .output
                .len()
                .saturating_sub(model.viewport_height as usize);
            model.scroll_offset = (model.scroll_offset + page).min(max_scroll);
            vec![]
        }
        Message::PageUp => {
            let page = model.viewport_height as usize;
            model.scroll_offset = model.scroll_offset.saturating_sub(page);
            vec![]
        }
        Message::Select => {
            if model.tab == Tab::Subvolumes
                && model.mode == ViewMode::Normal
                && !model.subvolumes.is_empty()
            {
                let subvol = &model.subvolumes[model.subvol_selected];
                let path = format!("{}/{}", model.mount_point.as_str(), subvol.path);
                model.mode = ViewMode::SubvolDetail;
                model.subvol_list_output = model.output.clone();
                dispatch(model, CmdRequest::BtrfsSubvolumeShow { path })
            } else {
                vec![]
            }
        }
        Message::Back => {
            match model.mode {
                ViewMode::SubvolDetail => {
                    model.mode = ViewMode::Normal;
                    model.output = model.subvol_list_output.clone();
                    model.scroll_offset = 0;
                    model.command_gen += 1;
                    model.loading = false;
                }
                ViewMode::Help => {
                    model.mode = model.help_return_mode;
                }
                ViewMode::Normal => {}
            }
            vec![]
        }
        Message::Reload => {
            if model.loading {
                return vec![];
            }
            if model.mode == ViewMode::SubvolDetail {
                // Reload the detail for the currently selected subvolume
                if !model.subvolumes.is_empty() {
                    let subvol = &model.subvolumes[model.subvol_selected];
                    let path = format!("{}/{}", model.mount_point.as_str(), subvol.path);
                    return dispatch(model, CmdRequest::BtrfsSubvolumeShow { path });
                }
                vec![]
            } else {
                switch_command(model)
            }
        }
        Message::CommandFinished { raw, generation } => {
            if generation != model.command_gen {
                return vec![];
            }
            model.loading = false;
            model.output = raw.stdout.lines().map(|l| l.to_owned()).collect();
            if !raw.stderr.is_empty() {
                for line in raw.stderr.lines() {
                    model.output.push(line.to_owned());
                }
            }
            model.scroll_offset = 0;

            // Parse subvolume list when on SubvolList subtab in Normal mode
            if model.current_subtab() == SubTab::SubvolList && model.mode == ViewMode::Normal {
                match parse_btrfs_subvolume_list(&raw) {
                    Ok(parsed) => {
                        model.subvolumes = parsed.subvolumes;
                        model.subvol_selected = model
                            .subvol_selected
                            .min(model.subvolumes.len().saturating_sub(1));
                    }
                    Err(_) => {
                        model.subvolumes.clear();
                    }
                }
            }

            vec![]
        }
    }
}

fn switch_command(model: &mut Model) -> Vec<Effect> {
    let request = model.current_subtab().request(&model.mount_point);
    dispatch(model, request)
}

fn dispatch(model: &mut Model, request: CmdRequest) -> Vec<Effect> {
    model.scroll_offset = 0;
    model.command_gen += 1;
    model.loading = true;
    model.output.clear();
    vec![Effect::RunCommand {
        request,
        generation: model.command_gen,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::model::Tab;

    fn demo_model() -> Model {
        Model::new_demo(
            "/mnt/storage",
            Tab::Filesystem,
            vec!["line1".into(), "line2".into()],
        )
    }

    /*
     * Intent: switching tabs resets scroll and fires a RunCommand effect.
     *
     * Why it exists: tab switches must load new data; stale scroll positions
     * from a previous tab would show wrong content.
     *
     * Scenario: user is scrolled halfway through Filesystem output, then
     * presses Tab to switch to Devices.
     */
    #[test]
    fn next_tab_resets_scroll_and_loads() {
        let mut model = demo_model();
        model.scroll_offset = 5;
        let effects = update(&mut model, Message::NextTab);
        assert_eq!(model.tab, Tab::Devices);
        assert_eq!(model.scroll_offset, 0);
        assert_eq!(model.subtab_index, 0);
        assert!(model.loading);
        assert_eq!(effects.len(), 1);
    }

    /*
     * Intent: subtab cycling wraps around and fires a RunCommand.
     *
     * Why it exists: subtab navigation with h/l must wrap and load new data.
     *
     * Scenario: user is on Filesystem > Df (last subtab), presses 'l' and
     * wraps to Usage.
     */
    #[test]
    fn next_subtab_wraps_and_loads() {
        let mut model = demo_model();
        model.subtab_index = 2; // Df (last in Filesystem)
        let effects = update(&mut model, Message::NextSubTab);
        assert_eq!(model.subtab_index, 0); // wrapped to Usage
        assert_eq!(effects.len(), 1);
        assert!(model.loading);
    }

    /*
     * Intent: CommandFinished with a stale generation is silently ignored.
     *
     * Why it exists: prevents a slow command's result from overwriting output
     * loaded by a newer tab/subtab switch.
     *
     * Scenario: user switches tabs rapidly; the first tab's command finishes
     * after the second tab's command was already dispatched.
     */
    #[test]
    fn stale_command_ignored() {
        let mut model = demo_model();
        model.command_gen = 5;
        model.output = vec!["current".into()];
        let effects = update(
            &mut model,
            Message::CommandFinished {
                raw: RawCommandOutput {
                    cmd: "old".into(),
                    stdout: "stale output".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
                generation: 3,
            },
        );
        assert!(effects.is_empty());
        assert_eq!(model.output, vec!["current"]);
    }

    /*
     * Intent: Select in Subvolumes tab enters SubvolDetail and fires
     * a RunCommand for `btrfs subvolume show`.
     *
     * Why it exists: the drill-in path is the key interactive feature of the
     * Subvolumes tab.
     *
     * Scenario: user navigates to a subvolume with j/k and presses Enter.
     */
    #[test]
    fn select_enters_subvol_detail() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Subvolumes,
            vec!["ID 256 gen 10 top level 5 path data".into()],
        );
        model.subvolumes = vec![crate::parse::types::BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];
        let effects = update(&mut model, Message::Select);
        assert_eq!(model.mode, ViewMode::SubvolDetail);
        assert_eq!(effects.len(), 1);
        assert!(model.loading);
    }

    /*
     * Intent: Back from SubvolDetail restores Normal mode and the list output.
     *
     * Why it exists: the user needs to return to the selectable list after
     * viewing a subvolume's detail.
     *
     * Scenario: user presses Esc in the subvolume detail view.
     */
    #[test]
    fn back_returns_to_normal() {
        let mut model =
            Model::new_demo("/mnt/storage", Tab::Subvolumes, vec!["detail line".into()]);
        model.mode = ViewMode::SubvolDetail;
        model.subvol_list_output = vec!["list line".into()];
        let effects = update(&mut model, Message::Back);
        assert_eq!(model.mode, ViewMode::Normal);
        assert_eq!(model.output, vec!["list line"]);
        assert!(effects.is_empty());
    }

    // Intent: a `btrfs subvolume show` response that arrives after Back is
    // dropped instead of clobbering the restored list view.
    //
    // Why it exists: Back used to leave `command_gen` and `loading` untouched,
    // so the in-flight detail command's response replaced the restored list
    // output and ran the list parser against `subvolume show` text, clearing
    // `model.subvolumes` and making the table unusable until reload.
    //
    // Scenario: user drills into a subvolume, presses Esc before
    // `btrfs subvolume show` returns, then the response lands.
    #[test]
    fn back_discards_in_flight_subvol_detail_command() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Subvolumes,
            vec!["ID 256 gen 10 top level 5 path data".into()],
        );
        model.subvolumes = vec![crate::parse::types::BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];

        let _ = update(&mut model, Message::Select);
        let detail_gen = model.command_gen;
        let _ = update(&mut model, Message::Back);

        let stale_show_stdout = "\tName:\t\t\tdata\n\tUUID:\t\t\t...\n\tParent UUID:\t\t-\n";
        let _ = update(
            &mut model,
            Message::CommandFinished {
                raw: RawCommandOutput {
                    cmd: "btrfs subvolume show /mnt/storage/data".into(),
                    stdout: stale_show_stdout.into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
                generation: detail_gen,
            },
        );

        assert_eq!(model.mode, ViewMode::Normal);
        assert_eq!(
            model.output,
            vec!["ID 256 gen 10 top level 5 path data".to_string()]
        );
        assert_eq!(model.subvolumes.len(), 1);
        assert!(!model.loading);
    }

    // Intent: opening and closing Help while in SubvolDetail returns to
    // SubvolDetail, not Normal.
    //
    // Why it exists: ToggleHelp used to flip Help to Normal unconditionally, so
    // `Select -> ? -> ?` leaked into Normal mode while a detail command was
    // still in flight. The late response then fed `subvolume show` text into
    // the list parser and cleared the table.
    //
    // Scenario: user drills into a subvolume, opens and closes Help before
    // `btrfs subvolume show` returns, then the response lands.
    #[test]
    fn help_round_trip_preserves_subvol_detail() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Subvolumes,
            vec!["ID 256 gen 10 top level 5 path data".into()],
        );
        model.subvolumes = vec![crate::parse::types::BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];

        let _ = update(&mut model, Message::Select);
        let detail_gen = model.command_gen;
        let _ = update(&mut model, Message::ToggleHelp);
        let _ = update(&mut model, Message::ToggleHelp);

        assert_eq!(model.mode, ViewMode::SubvolDetail);

        let stale_show_stdout = "\tName:\t\t\tdata\n\tUUID:\t\t\t...\n\tParent UUID:\t\t-\n";
        let _ = update(
            &mut model,
            Message::CommandFinished {
                raw: RawCommandOutput {
                    cmd: "btrfs subvolume show /mnt/storage/data".into(),
                    stdout: stale_show_stdout.into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
                generation: detail_gen,
            },
        );

        assert_eq!(model.subvolumes.len(), 1);
        assert_eq!(model.mode, ViewMode::SubvolDetail);
    }

    // Intent: Reload while in SubvolDetail re-dispatches `btrfs subvolume
    // show <selected>` with a bumped generation and clears output + scroll,
    // matching every other dispatch path.
    // Why it exists: this path previously skipped output.clear() and
    // scroll_offset = 0 silently; the unified `dispatch` helper aligns all
    // three call sites. This test pins the dispatched request, the
    // generation bump, and the cleared state so a future regression cannot
    // silently swap the request kind or skip the resets.
    // Scenario: user drills into a subvolume, scrolls partway down the
    // detail output, and presses 'r' to refresh.
    #[test]
    fn reload_in_subvol_detail_clears_and_dispatches() {
        let mut model = Model::new_demo("/mnt/storage", Tab::Subvolumes, vec!["old detail".into()]);
        model.subvolumes = vec![crate::parse::types::BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];
        model.mode = ViewMode::SubvolDetail;
        model.scroll_offset = 7;
        let generation_before = model.command_gen;

        let effects = update(&mut model, Message::Reload);

        assert_eq!(effects.len(), 1);
        match &effects[0] {
            Effect::RunCommand {
                request,
                generation,
            } => {
                assert_eq!(
                    *request,
                    CmdRequest::BtrfsSubvolumeShow {
                        path: "/mnt/storage/data".into(),
                    },
                );
                assert_eq!(*generation, generation_before + 1);
            }
        }
        assert!(model.loading);
        assert!(model.output.is_empty());
        assert_eq!(model.scroll_offset, 0);
        assert_eq!(model.command_gen, generation_before + 1);
    }

    /*
     * Intent: Reload while loading is a no-op.
     *
     * Why it exists: prevents duplicate in-flight requests from stacking up.
     *
     * Scenario: impatient user mashes 'r' while a command is already running.
     */
    #[test]
    fn reload_when_loading_is_noop() {
        let mut model = demo_model();
        model.loading = true;
        let effects = update(&mut model, Message::Reload);
        assert!(effects.is_empty());
    }
}
