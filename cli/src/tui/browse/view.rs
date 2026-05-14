use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

use crate::tui::browse::state::BrowseFocus;
use crate::tui::model::{Model, UpsSnapshot};

const SPINNER: &[char] = &['|', '/', '-', '\\'];

/// Render Browse's sidebar columns and raw output content inside the
/// parent TUI tab body.
pub(crate) fn view_browse(model: &Model, frame: &mut Frame, area: Rect) {
    let body = if area.height > 1 {
        let vertical = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
        let command = model
            .browse
            .command_display(&model.mount_point, model.ups_config.as_ref())
            .map(|cmd| {
                if model.browse.loading() {
                    let ch = SPINNER[(model.frame as usize / 8) % SPINNER.len()];
                    format!("$ {cmd} {ch}")
                } else {
                    format!("$ {cmd}")
                }
            })
            .unwrap_or_else(|| "$".to_owned());
        frame.render_widget(
            Paragraph::new(command).style(Style::default().fg(Color::DarkGray)),
            vertical[1],
        );
        vertical[0]
    } else {
        area
    };

    let mut constraints = vec![Constraint::Length(12), Constraint::Length(16)];
    if model.browse.has_subviews() {
        constraints.push(Constraint::Length(12));
    }
    constraints.push(Constraint::Min(20));
    let cols = Layout::horizontal(constraints).split(body);

    render_rows(
        frame,
        cols[0],
        "Pgm",
        model.browse.focus() == BrowseFocus::Program,
        model.browse.program_rows(),
    );
    render_rows(
        frame,
        cols[1],
        "Cmd",
        model.browse.focus() == BrowseFocus::Command,
        model.browse.command_rows(),
    );

    let content_idx = if model.browse.has_subviews() {
        render_rows(
            frame,
            cols[2],
            "View",
            model.browse.focus() == BrowseFocus::Subview,
            model.browse.subview_rows(),
        );
        3
    } else {
        2
    };

    render_content(frame, cols[content_idx], model);
}

fn render_rows(
    frame: &mut Frame,
    area: Rect,
    title: &'static str,
    focused: bool,
    rows: Vec<(&'static str, bool)>,
) {
    let block = column_block(title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|(label, selected)| {
            if selected {
                Line::from(vec![
                    Span::styled(">", Style::default().fg(Color::Cyan)),
                    Span::raw(" "),
                    Span::styled(label, Style::default().add_modifier(Modifier::BOLD)),
                ])
            } else {
                Line::from(vec![Span::raw("  "), Span::raw(label)])
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_content(frame: &mut Frame, area: Rect, model: &Model) {
    let block = column_block("Content", model.browse.focus() == BrowseFocus::Content);
    let inner = block.inner(area);
    model.browse.set_viewport_height(inner.height);
    frame.render_widget(block, area);

    if let Some(empty) = model.browse.empty_state() {
        frame.render_widget(
            Paragraph::new(empty.message()).style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }

    if model.browse.loading() && model.browse.output().is_empty() {
        let ch = SPINNER[(model.frame as usize / 8) % SPINNER.len()];
        frame.render_widget(Paragraph::new(format!("{ch} loading...")), inner);
        return;
    }

    if model.browse.is_nut_status() {
        frame.render_widget(Paragraph::new(ups_status_lines(model.ups.as_ref())), inner);
        return;
    }

    if model.browse.is_nut_variables() {
        let lines = model
            .ups
            .as_ref()
            .map(|s| lines_from_text(&s.raw_text))
            .unwrap_or_else(|| vec![Line::from("waiting for UPS probe...")]);
        frame.render_widget(Paragraph::new(lines), inner);
        return;
    }

    if model.browse.is_subvolume_list() && !model.browse.subvolumes().is_empty() {
        render_subvolume_table(frame, inner, model);
        return;
    }

    let visible_height = inner.height as usize;
    let lines: Vec<Line> = model
        .browse
        .output()
        .iter()
        .skip(model.browse.scroll_offset())
        .take(visible_height)
        .map(|line| Line::from(line.clone()))
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
}

fn render_subvolume_table(frame: &mut Frame, area: Rect, model: &Model) {
    let header = Row::new(["", "ID", "Gen", "Top", "Path"]).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    );
    let rows: Vec<Row> = model
        .browse
        .subvolumes()
        .iter()
        .enumerate()
        .map(|(idx, sv)| {
            let selected = idx == model.browse.selected_subvolume();
            let marker = if selected { ">" } else { "" };
            let style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(marker),
                Cell::from(sv.id.to_string()),
                Cell::from(sv.generation.to_string()),
                Cell::from(sv.top_level.to_string()),
                Cell::from(sv.path.clone()),
            ])
            .style(style)
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(6),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Min(10),
    ];
    frame.render_widget(Table::new(rows, widths).header(header), area);
}

fn column_block(title: &'static str, focused: bool) -> Block<'static> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::new()
        .borders(Borders::ALL)
        .title(format!(" {title} "))
        .border_style(style)
}

fn ups_status_lines(snapshot: Option<&UpsSnapshot>) -> Vec<Line<'static>> {
    let Some(snapshot) = snapshot else {
        return vec![Line::from("waiting for UPS probe...")];
    };
    let flags = if snapshot.flags.is_empty() {
        "--".to_owned()
    } else {
        let mut flags: Vec<&str> = snapshot.flags.iter().map(|f| f.as_token()).collect();
        flags.sort();
        flags.join(" ")
    };
    let charge = snapshot
        .battery_charge_pct
        .map(|pct| format!("{pct}%"))
        .unwrap_or_else(|| "--".to_owned());
    let runtime = snapshot
        .runtime_secs
        .map(crate::ups::format_runtime)
        .unwrap_or_else(|| "--".to_owned());
    let load = match (snapshot.load_pct, snapshot.watts_estimated) {
        (Some(pct), Some(watts)) => format!("{pct}% ({watts} W est.)"),
        (Some(pct), None) => format!("{pct}%"),
        _ => "--".to_owned(),
    };
    vec![
        Line::from(vec![Span::styled("Status   ", dim()), Span::raw(flags)]),
        Line::from(vec![Span::styled("Battery  ", dim()), Span::raw(charge)]),
        Line::from(vec![Span::styled("Runtime  ", dim()), Span::raw(runtime)]),
        Line::from(vec![Span::styled("Load     ", dim()), Span::raw(load)]),
    ]
}

fn lines_from_text(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|line| Line::from(line.to_owned()))
        .collect()
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::RawCommandOutput;
    use crate::config::Ups;
    use crate::tui::demo::{sample_disk_names, sample_pool};
    use crate::tui::model::{DaemonStatus, PoolStatus, UpsSnapshot};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Instant;

    fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view_browse(model, frame, frame.area()))
            .unwrap();
        terminal
    }

    fn buffer_to_string(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf.cell((x, y)).map_or(" ", |c| c.symbol()));
            }
            let trimmed = out.trim_end();
            out.truncate(trimmed.len());
            out.push('\n');
        }
        out
    }

    macro_rules! snap {
        ($value:expr) => {
            insta::with_settings!({ prepend_module_to_snapshot => false }, {
                insta::assert_snapshot!($value);
            });
        };
    }

    fn model() -> Model {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs filesystem usage /mnt/storage".into(),
                stdout: "Overall:\n    Device size: 10.91TiB\n    Used: 4.20TiB\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        model
    }

    fn ups_snapshot() -> UpsSnapshot {
        UpsSnapshot {
            flags: [crate::parse::types::UpsStatusFlag::Ol]
                .into_iter()
                .collect(),
            battery_charge_pct: Some(100),
            runtime_secs: Some(1800),
            load_pct: Some(20),
            watts_estimated: Some(100),
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
            raw_text: "ups.status: OL\nbattery.charge: 100\n".into(),
        }
    }

    #[test]
    fn snapshot_browse_default() {
        let model = model();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_filesystem_usage() {
        let model = model();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_filesystem_show() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Subview;
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs filesystem show /mnt/storage".into(),
                stdout: "Label: none uuid: abc-123\nTotal devices 2\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_filesystem_df() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Subview;
        model.browse.select_next();
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs filesystem df /mnt/storage".into(),
                stdout: "Data, RAID1: total=2.00GiB, used=1.00GiB\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_devices_usage() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs device usage /mnt/storage".into(),
                stdout: "/dev/mapper/braid-disk1, ID: 1\n   Device size: 256.00MiB\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_devices_stats() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        model.browse.focus = BrowseFocus::Subview;
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs device stats /mnt/storage".into(),
                stdout: "[/dev/mapper/braid-disk1].write_io_errs 0\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_subvolumes() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list /mnt/storage".into(),
                stdout: "ID 256 gen 10 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_subvolume_detail() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        model.browse.select_next();
        model.browse.focus = BrowseFocus::Content;
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume list /mnt/storage".into(),
                stdout: "ID 256 gen 10 top level 5 path data\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        let _ = model.browse.enter(&model.pool);
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs subvolume show /mnt/storage/data".into(),
                stdout: "data\n\tName:\t\tdata\n\tUUID:\t\tabc-123\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            1,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_scrub() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        for _ in 0..3 {
            model.browse.select_next();
        }
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs scrub status /mnt/storage".into(),
                stdout: "UUID: abc-123\nScrub started: Tue Feb 24 02:00:00 2026\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_btrfs_balance() {
        let mut model = model();
        model.browse.focus = BrowseFocus::Command;
        for _ in 0..4 {
            model.browse.select_next();
        }
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: "No balance found on '/mnt/storage'\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_nut_status() {
        let mut model = model();
        model.ups_config = Some(Ups { name: "ups".into() });
        model.ups = Some(ups_snapshot());
        model.browse.select_next();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_nut_variables() {
        let mut model = model();
        model.ups_config = Some(Ups { name: "ups".into() });
        model.ups = Some(ups_snapshot());
        model.browse.select_next();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_nut_commands() {
        let mut model = model();
        model.ups_config = Some(Ups { name: "ups".into() });
        model.browse.select_next();
        model.browse.focus = BrowseFocus::Command;
        model.browse.select_next();
        model.browse.select_next();
        model.browse.command_finished(
            RawCommandOutput {
                cmd: "upscmd -l ups".into(),
                stdout: "Instant commands supported on UPS [ups]:\nbeeper.disable\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
            0,
        );
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_pool_offline() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::NotMounted);
        let _ = model
            .browse
            .load_current(&model.pool, model.ups_config.as_ref());
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_focus_command_col() {
        let mut model = model();
        model.browse.focus_right();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_focus_subview_col() {
        let mut model = model();
        model.browse.focus_right();
        model.browse.focus_right();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }

    #[test]
    fn snapshot_browse_focus_content() {
        let mut model = model();
        model.browse.focus_right();
        model.browse.focus_right();
        model.browse.focus_right();
        snap!(buffer_to_string(&render(&model, 80, 14)));
    }
}
