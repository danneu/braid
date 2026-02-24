use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};

use crate::status::{format_bytes, StatusCode};
use super::app::{DaemonStatus, Model};

pub fn view(model: &Model, frame: &mut Frame) {
    let debug_width = u16::from(model.show_debug);
    let chunks = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Fill(debug_width),
    ])
    .split(frame.area());

    let main_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" braid ");

    let inner = main_block.inner(chunks[0]);
    frame.render_widget(main_block, chunks[0]);

    // Layout: pool section, disk table, footer
    let sections = Layout::vertical([
        Constraint::Length(pool_section_height(model)),
        Constraint::Min(3), // disk table
        Constraint::Length(1), // footer
    ])
    .split(inner);

    render_pool_section(model, frame, sections[0]);
    render_disk_table(model, frame, sections[1]);

    let footer = Paragraph::new(Line::from("q quit | d debug | r refresh"));
    frame.render_widget(footer, sections[2]);

    if model.show_debug {
        let debug_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" debug ");

        let debug_text = format!("{model:#?}");
        let debug_content = Paragraph::new(debug_text).block(debug_block);
        frame.render_widget(debug_content, chunks[1]);
    }
}

fn pool_section_height(model: &Model) -> u16 {
    match &model.status_report {
        Some(_) => 7, // Pool, Status, Capacity (2 lines), Scrub, Errors, blank
        None => 2,    // loading/error/idle message + blank
    }
}

fn render_pool_section(model: &Model, frame: &mut Frame, area: ratatui::layout::Rect) {
    let report = match &model.status_report {
        Some(r) => r,
        None => {
            let msg = match &model.daemon_status {
                DaemonStatus::Requesting => "loading...".to_string(),
                DaemonStatus::Error(e) => format!("error: {e}"),
                _ => match &model.status_error {
                    Some(e) => format!("error: {e}"),
                    None => "press r to refresh".to_string(),
                },
            };
            let text = Paragraph::new(Line::from(msg));
            frame.render_widget(text, area);
            return;
        }
    };

    let status_style = match report.status_code {
        StatusCode::Healthy => Style::default().fg(Color::Green),
        StatusCode::Degraded => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        StatusCode::NotMounted => Style::default().fg(Color::Yellow),
    };

    let capacity_line = match &report.capacity {
        Some(cap) => {
            let pct = if cap.total_bytes > 0 {
                (cap.used_bytes as f64 / cap.total_bytes as f64 * 100.0) as u64
            } else {
                0
            };
            format!(
                "{} used / {} ({}%)",
                format_bytes(cap.used_bytes),
                format_bytes(cap.total_bytes),
                pct,
            )
        }
        None => "-".to_string(),
    };

    let free_line = match &report.capacity {
        Some(cap) => format!("{} free", format_bytes(cap.free_bytes)),
        None => String::new(),
    };

    let scrub_line = report
        .last_scrub
        .as_deref()
        .unwrap_or("-")
        .to_string();

    let total_errors: u64 = report
        .disks
        .iter()
        .filter_map(|d| d.errors.as_ref())
        .map(|e| e.read + e.write + e.flush + e.corruption + e.generation)
        .sum();

    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::raw(format!("{:<10}", "Pool")),
            Span::raw(&report.mount_point),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Status")),
            Span::styled(&report.status, status_style),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Capacity")),
            Span::raw(capacity_line),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "")),
            Span::raw(free_line),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Scrub")),
            Span::raw(scrub_line),
        ]),
        Line::from(vec![
            Span::raw(format!("{:<10}", "Errors")),
            Span::raw(format!("{total_errors} total")),
        ]),
    ];

    let paragraph = Paragraph::new(lines);
    frame.render_widget(paragraph, area);
}

fn render_disk_table(model: &Model, frame: &mut Frame, area: ratatui::layout::Rect) {
    let report = match &model.status_report {
        Some(r) => r,
        None => return,
    };

    if report.disks.is_empty() {
        return;
    }

    let header = Row::new(vec![
        Cell::from("NAME"),
        Cell::from("STATUS"),
        Cell::from("LUKS"),
        Cell::from("POOL"),
        Cell::from("R"),
        Cell::from("W"),
        Cell::from("F"),
        Cell::from("C"),
        Cell::from("G"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = report
        .disks
        .iter()
        .map(|disk| {
            let name = disk
                .by_id
                .rsplit('/')
                .next()
                .unwrap_or(&disk.by_id);
            let name = if name.len() > 20 {
                &name[..20]
            } else {
                name
            };

            let luks = if !disk.luks_uuid.is_empty() {
                "open"
            } else {
                "-"
            };

            let pool = if disk.devid.is_some() { "yes" } else { "-" };

            let (r, w, f, c, g) = match &disk.errors {
                Some(e) => (
                    e.read.to_string(),
                    e.write.to_string(),
                    e.flush.to_string(),
                    e.corruption.to_string(),
                    e.generation.to_string(),
                ),
                None => (
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                    "-".to_string(),
                ),
            };

            let style = if disk.status == "missing" {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };

            Row::new(vec![
                Cell::from(name.to_string()),
                Cell::from(disk.status.clone()),
                Cell::from(luks.to_string()),
                Cell::from(pool.to_string()),
                Cell::from(r),
                Cell::from(w),
                Cell::from(f),
                Cell::from(c),
                Cell::from(g),
            ])
            .style(style)
        })
        .collect();

    let widths = [
        Constraint::Length(20),
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Length(5),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
    ];

    let table = Table::new(rows, widths)
        .header(header)
        .column_spacing(1);

    frame.render_widget(table, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{CapacityReport, DiskErrors, DiskReport, StatusCode, StatusReport};
    use ratatui::{backend::TestBackend, Terminal};

    fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(model, frame))
            .unwrap();
        terminal
    }

    fn healthy_report() -> StatusReport {
        StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: StatusCode::Healthy,
            status: "healthy".to_owned(),
            total_devices: Some(2),
            present_count: Some(2),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![
                DiskReport {
                    mapper: "disk1".to_owned(),
                    by_id: "/dev/disk/by-id/ata-WDC_WD40EFRX_1".to_owned(),
                    luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                    devid: Some("1".to_owned()),
                    status: "present".to_owned(),
                    errors: Some(DiskErrors {
                        read: 0,
                        write: 0,
                        flush: 0,
                        corruption: 0,
                        generation: 0,
                    }),
                },
                DiskReport {
                    mapper: "disk2".to_owned(),
                    by_id: "/dev/disk/by-id/ata-WDC_WD40EFRX_2".to_owned(),
                    luks_uuid: "22222222-2222-2222-2222-222222222222".to_owned(),
                    devid: Some("2".to_owned()),
                    status: "present".to_owned(),
                    errors: Some(DiskErrors {
                        read: 0,
                        write: 0,
                        flush: 0,
                        corruption: 0,
                        generation: 0,
                    }),
                },
            ],
        }
    }

    #[test]
    fn snapshot_default() {
        let model = Model::default();
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_with_ticks() {
        let model = Model {
            tick_count: 42,
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_debug_panel() {
        let model = Model {
            show_debug: true,
            ..Model::default()
        };
        let terminal = render(&model, 80, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_daemon_pong() {
        let model = Model {
            daemon_status: DaemonStatus::Ok,
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_daemon_error() {
        let model = Model {
            daemon_status: DaemonStatus::Error("connection refused".to_string()),
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_pool_healthy() {
        let model = Model {
            daemon_status: DaemonStatus::Ok,
            status_report: Some(healthy_report()),
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_pool_degraded() {
        let mut report = healthy_report();
        report.status_code = StatusCode::Degraded;
        report.status = "DEGRADED (1 missing device)".to_owned();
        report.missing_count = Some(1);
        report.present_count = Some(1);
        report.disks.pop(); // remove one disk
        report.disks.push(DiskReport {
            mapper: "disk3".to_owned(),
            by_id: "/dev/disk/by-id/ata-SAM_SSD860_3".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: "missing".to_owned(),
            errors: None,
        });
        let model = Model {
            daemon_status: DaemonStatus::Ok,
            status_report: Some(report),
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_pool_not_mounted() {
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: StatusCode::NotMounted,
            status: "not mounted".to_owned(),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        };
        let model = Model {
            daemon_status: DaemonStatus::Ok,
            status_report: Some(report),
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_pool_loading() {
        let model = Model {
            daemon_status: DaemonStatus::Requesting,
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_pool_error() {
        let model = Model {
            daemon_status: DaemonStatus::Error("config: not found".to_string()),
            status_error: Some("config: not found".to_string()),
            ..Model::default()
        };
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(terminal.backend());
    }
}
