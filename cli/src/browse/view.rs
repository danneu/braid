use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use ratatui::Frame;

use super::model::{Model, Tab, ViewMode};

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn tab_bar(model: &Model) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if *tab == model.tab {
            spans.push(Span::styled(
                tab.label(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            ));
        } else {
            spans.push(Span::styled(
                tab.label(),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    Line::from(spans)
}

fn subtab_bar(model: &Model) -> Option<Line<'static>> {
    let subtabs = model.tab.subtabs();
    if subtabs.len() <= 1 {
        return None;
    }
    let mut spans = Vec::new();
    for (i, subtab) in subtabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if i == model.subtab_index {
            spans.push(Span::styled(
                format!("[{}]", subtab.label()),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                subtab.label().to_owned(),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }
    Some(Line::from(spans))
}

pub fn view(model: &mut Model, frame: &mut Frame) {
    let area = frame.area();
    let has_subtab_bar = model.tab.subtabs().len() > 1;

    let mut constraints = vec![
        Constraint::Length(1), // tab bar
    ];
    if has_subtab_bar {
        constraints.push(Constraint::Length(1)); // subtab bar
    }
    constraints.push(Constraint::Length(1)); // spacer
    constraints.push(Constraint::Min(0)); // body
    constraints.push(Constraint::Length(1)); // command line
    constraints.push(Constraint::Length(1)); // footer

    let outer = Layout::vertical(constraints).split(area);
    let mut off: usize = 0;

    // Tab bar
    frame.render_widget(tab_bar(model), outer[off]);
    off += 1;

    // Subtab bar
    if has_subtab_bar {
        if let Some(bar) = subtab_bar(model) {
            frame.render_widget(Paragraph::new(bar), outer[off]);
        }
        off += 1;
    }

    // Spacer
    off += 1;

    // Body
    let body_area = outer[off];
    model.viewport_height = body_area.height;

    if model.loading && model.output.is_empty() {
        let spinner_ch = SPINNER[(model.frame as usize / 8) % SPINNER.len()];
        frame.render_widget(
            Paragraph::new(format!("  {spinner_ch} loading...")),
            body_area,
        );
    } else if model.tab == Tab::Subvolumes && model.mode == ViewMode::Normal {
        // Selectable subvolume list
        if model.subvolumes.is_empty() && !model.output.is_empty() {
            // Show raw output (might be an error or empty list message)
            let lines: Vec<Line> = model
                .output
                .iter()
                .map(|l| Line::from(l.as_str().to_owned()))
                .collect();
            frame.render_widget(Paragraph::new(lines), body_area);
        } else {
            let visible_height = body_area.height as usize;
            // Keep selected item visible
            let scroll = if model.subvol_selected >= visible_height {
                model.subvol_selected - visible_height + 1
            } else {
                0
            };
            let lines: Vec<Line> = model
                .subvolumes
                .iter()
                .enumerate()
                .skip(scroll)
                .take(visible_height)
                .map(|(i, sv)| {
                    let marker = if i == model.subvol_selected {
                        "> "
                    } else {
                        "  "
                    };
                    let style = if i == model.subvol_selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::from(Span::styled(format!("{marker}{}", sv.path), style))
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), body_area);
        }
    } else {
        // Raw output with scroll
        let visible_height = body_area.height as usize;
        let lines: Vec<Line> = model
            .output
            .iter()
            .skip(model.scroll_offset)
            .take(visible_height)
            .map(|l| Line::from(l.as_str().to_owned()))
            .collect();
        frame.render_widget(Paragraph::new(lines), body_area);
    }
    off += 1;

    // Command line
    let cmd_display = if model.mode == ViewMode::SubvolDetail {
        if !model.subvolumes.is_empty() {
            let sv = &model.subvolumes[model.subvol_selected];
            format!(
                "btrfs subvolume show {}/{}",
                model.mount_point.as_str(),
                sv.path
            )
        } else {
            model.current_command_display()
        }
    } else {
        model.current_command_display()
    };
    let cmd_line = if model.loading {
        let spinner_ch = SPINNER[(model.frame as usize / 8) % SPINNER.len()];
        format!("$ {cmd_display} {spinner_ch}")
    } else {
        format!("$ {cmd_display}")
    };
    frame.render_widget(
        Paragraph::new(cmd_line).style(Style::default().fg(Color::DarkGray)),
        outer[off],
    );
    off += 1;

    // Footer
    let mut hints = vec!["q:quit", "?:help", "r:reload", "j/k:scroll"];
    if has_subtab_bar {
        hints.push("h/l:subtab");
    }
    if model.tab == Tab::Subvolumes && model.mode == ViewMode::Normal {
        hints.push("enter:detail");
    }
    if model.mode == ViewMode::SubvolDetail {
        hints.push("esc:back");
    }
    let footer = hints.join(" | ");
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        outer[off],
    );

    // Help overlay
    if model.mode == ViewMode::Help {
        view_help(frame, area);
    }
}

fn view_help(frame: &mut Frame, area: Rect) {
    let width = 46.min(area.width.saturating_sub(4));
    let height = 16.min(area.height.saturating_sub(4));
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Help ")
        .padding(Padding::horizontal(1));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("Tab/Shift-Tab", Style::default().fg(Color::Cyan)),
            Span::raw("  switch tab"),
        ]),
        Line::from(vec![
            Span::styled("h / l", Style::default().fg(Color::Cyan)),
            Span::raw("          switch subtab"),
        ]),
        Line::from(vec![
            Span::styled("j / k", Style::default().fg(Color::Cyan)),
            Span::raw("          scroll / select"),
        ]),
        Line::from(vec![
            Span::styled("Ctrl-D/U", Style::default().fg(Color::Cyan)),
            Span::raw("       page down/up"),
        ]),
        Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::raw("          subvol detail"),
        ]),
        Line::from(vec![
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::raw("            back"),
        ]),
        Line::from(vec![
            Span::styled("r", Style::default().fg(Color::Cyan)),
            Span::raw("              reload"),
        ]),
        Line::from(vec![
            Span::styled("q", Style::default().fg(Color::Cyan)),
            Span::raw("              quit"),
        ]),
        Line::from(""),
        Line::from(Span::styled(
            "  press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browse::model::{Model, Tab};
    use crate::parse::types::BtrfsSubvolume;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(model: &mut Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| view(model, frame)).unwrap();
        terminal
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                output.push_str(buf[(x, y)].symbol());
            }
            // Trim trailing spaces for cleaner snapshots
            let trimmed = output.trim_end();
            output.truncate(trimmed.len());
            output.push('\n');
        }
        output
    }

    macro_rules! snap {
        ($value:expr) => {
            insta::with_settings!({ prepend_module_to_snapshot => false }, {
                insta::assert_snapshot!($value);
            });
        };
    }

    /*
     * Intent: Filesystem tab with Usage subtab shows output and subtab bar.
     *
     * Why it exists: baseline snapshot for the default view on startup.
     *
     * Scenario: user runs `braid browse` and sees the initial Filesystem > Usage view.
     */
    #[test]
    fn snapshot_filesystem_usage() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Filesystem,
            vec![
                "Overall:".into(),
                "    Device size:          10.91TiB".into(),
                "    Device allocated:       5.46TiB".into(),
                "    Used:                   4.20TiB".into(),
            ],
        );
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Filesystem tab with Show subtab selected.
     *
     * Why it exists: verifies that subtab_index changes which subtab label
     * is highlighted.
     *
     * Scenario: user presses 'l' to move from Usage to Show.
     */
    #[test]
    fn snapshot_filesystem_show() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Filesystem,
            vec![
                "Label: none  uuid: abc-123".into(),
                "  Total devices 2  FS bytes used 4.20TiB".into(),
            ],
        );
        model.subtab_index = 1; // Show
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Devices tab renders with its own subtab bar.
     *
     * Why it exists: verifies Devices tab has Usage/Stats subtabs.
     *
     * Scenario: user navigates to Devices tab.
     */
    #[test]
    fn snapshot_devices_tab() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Devices,
            vec![
                "/dev/mapper/braid-toshiba, ID: 1".into(),
                "  Device size:     5.46TiB".into(),
            ],
        );
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Subvolumes tab shows selectable list with > marker.
     *
     * Why it exists: the subvolume selection UI is unique to this tab;
     * verifies the marker and highlighting work.
     *
     * Scenario: user is on Subvolumes tab with 3 subvolumes, first selected.
     */
    #[test]
    fn snapshot_subvolumes_with_selection() {
        let mut model = Model::new_demo("/mnt/storage", Tab::Subvolumes, vec![]);
        model.subvolumes = vec![
            BtrfsSubvolume {
                id: 256,
                generation: 10,
                top_level: 5,
                path: "data".into(),
            },
            BtrfsSubvolume {
                id: 257,
                generation: 20,
                top_level: 5,
                path: "snapshots".into(),
            },
            BtrfsSubvolume {
                id: 258,
                generation: 30,
                top_level: 257,
                path: "snapshots/daily".into(),
            },
        ];
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: SubvolDetail mode shows detail output and esc:back hint.
     *
     * Why it exists: verifies the detail view renders correctly with its
     * unique footer hints.
     *
     * Scenario: user pressed Enter on a subvolume and sees its detail.
     */
    #[test]
    fn snapshot_subvol_detail() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Subvolumes,
            vec![
                "data".into(),
                "  Name:     data".into(),
                "  UUID:     abc-123".into(),
            ],
        );
        model.mode = ViewMode::SubvolDetail;
        model.subvolumes = vec![BtrfsSubvolume {
            id: 256,
            generation: 10,
            top_level: 5,
            path: "data".into(),
        }];
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: loading state shows spinner in body and command line.
     *
     * Why it exists: verifies the user sees feedback while a command runs.
     *
     * Scenario: user just switched tabs and the command hasn't returned yet.
     */
    #[test]
    fn snapshot_loading() {
        let mut model = Model::new_demo("/mnt/storage", Tab::Filesystem, vec![]);
        model.loading = true;
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: help overlay is centered and lists all key bindings.
     *
     * Why it exists: verifies the help popup renders correctly.
     *
     * Scenario: user presses '?' to see available key bindings.
     */
    #[test]
    fn snapshot_help() {
        let mut model =
            Model::new_demo("/mnt/storage", Tab::Filesystem, vec!["some output".into()]);
        model.mode = ViewMode::Help;
        let terminal = render(&mut model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: tabs with a single subtab hide the subtab bar.
     *
     * Why it exists: the Scrub tab has only one subtab (Status); showing
     * a bar with a single item wastes space.
     *
     * Scenario: user navigates to the Scrub tab.
     */
    #[test]
    fn snapshot_single_subtab_no_bar() {
        let mut model = Model::new_demo(
            "/mnt/storage",
            Tab::Scrub,
            vec![
                "UUID:  abc-123".into(),
                "Scrub started:    Mon Mar 22 02:00:00 2026".into(),
                "Status:           finished".into(),
            ],
        );
        let terminal = render(&mut model, 60, 12);
        snap!(buffer_to_string(&terminal));
    }
}
