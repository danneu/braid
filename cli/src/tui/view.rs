use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Row, Table, TableState};
use ratatui::Frame;

use crate::parse::types::ScrubState;
use crate::tui::model::{Model, PoolState, PoolStatus};

#[derive(Clone, Copy)]
enum ByteUnit {
    TiB,
    GiB,
    MiB,
}

impl ByteUnit {
    fn friendliest(bytes: u64) -> Self {
        const TIB: u64 = 1024 * 1024 * 1024 * 1024;
        const GIB: u64 = 1024 * 1024 * 1024;
        if bytes >= TIB {
            ByteUnit::TiB
        } else if bytes >= GIB {
            ByteUnit::GiB
        } else {
            ByteUnit::MiB
        }
    }

    fn format(self, bytes: u64) -> String {
        let b = bytes as f64;
        match self {
            ByteUnit::TiB => format!("{:.1}", b / (1024.0 * 1024.0 * 1024.0 * 1024.0)),
            ByteUnit::GiB => format!("{:.1}", b / (1024.0 * 1024.0 * 1024.0)),
            ByteUnit::MiB => format!("{:.0}", b / (1024.0 * 1024.0)),
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            ByteUnit::TiB => "TiB",
            ByteUnit::GiB => "GiB",
            ByteUnit::MiB => "MiB",
        }
    }
}

fn format_bytes(bytes: u64) -> String {
    let unit = ByteUnit::friendliest(bytes);
    format!("{} {}", unit.format(bytes), unit.suffix())
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64) * 100.0
    }
}

fn pool_view(pool: &PoolState, unit: ByteUnit) -> Paragraph<'_> {
    let lines = vec![
        Line::from(format!("Path: {} ({})", pool.mount_point, pool.health)),
        Line::from(format!(
            "Redundancy: {}",
            match pool.profile.as_str() {
                "RAID1" => "2x (RAID1)",
                "single" => "None (single)",
                other => other,
            }
        )),
        Line::from(format!(
            "Usage: {:.0}% ({} / {} {})",
            percent(pool.used, pool.total),
            unit.format(pool.used),
            unit.format(pool.total),
            unit.suffix(),
        )),
    ];
    Paragraph::new(lines)
}

fn scrub_view(scrub: &ScrubState) -> Paragraph<'_> {
    match scrub {
        ScrubState::Never => Paragraph::new("Last run: never"),
        ScrubState::Running { pct } => {
            let detail = match pct {
                Some(p) => format!("Last run: now ({}% completed)", p),
                None => "Last run: now".to_owned(),
            };
            Paragraph::new(detail)
        }
        ScrubState::Completed {
            started_at,
            error_count,
        } => {
            let lines = vec![
                Line::from(format!("Last run: {}", started_at.0)),
                Line::from(format!("Errors: {}", error_count)),
            ];
            Paragraph::new(lines)
        }
        ScrubState::Unknown => {
            Paragraph::new("Last run: unknown").style(Style::default().fg(Color::DarkGray))
        }
    }
}

fn scrub_lines(scrub: &ScrubState) -> u16 {
    match scrub {
        ScrubState::Completed { .. } => 2,
        _ => 1,
    }
}

fn disk_table(model: &Model, unit: ByteUnit) -> Table<'_> {
    let disk_usage = match &model.pool {
        PoolStatus::Mounted(p) => Some(&p.disk_usage),
        _ => None,
    };
    let rows: Vec<Row> = model
        .disk_keys
        .iter()
        .enumerate()
        .map(|(i, name)| match disk_usage.and_then(|u| u.get(name)) {
            Some(usage) => Row::new([
                format!("{}", i + 1),
                name.clone(),
                format!("{:.0}%", percent(usage.data, usage.size)),
                format!(
                    "{} / {} {}",
                    unit.format(usage.data),
                    unit.format(usage.size),
                    unit.suffix()
                ),
            ]),
            None => Row::new([
                format!("{}", i + 1),
                name.clone(),
                String::new(),
                String::new(),
            ]),
        })
        .collect();
    let widths = [
        Constraint::Length(2),
        Constraint::Length(10),
        Constraint::Length(4),
        Constraint::Min(10),
    ];
    Table::new(rows, widths).row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
}

fn page_unit(model: &Model) -> ByteUnit {
    let max_bytes = match &model.pool {
        PoolStatus::Mounted(p) => p
            .disk_usage
            .values()
            .map(|u| u.size)
            .chain(Some(p.total))
            .max()
            .unwrap_or(0),
        _ => 0,
    };
    ByteUnit::friendliest(max_bytes)
}

pub fn view(model: &Model, frame: &mut Frame) {
    let page_unit = page_unit(model);
    let area = frame.area();

    let outer = Block::bordered()
        .title(" braid ")
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let pool_detail_lines: u16 = match &model.pool {
        PoolStatus::Mounted(_) => 3,
        PoolStatus::Loading | PoolStatus::NotMounted | PoolStatus::Error(_) => 1,
    };

    let scrub_detail_lines: u16 = match &model.pool {
        PoolStatus::Mounted(p) => scrub_lines(&p.scrub),
        _ => 0,
    };
    let scrub_section_lines = if scrub_detail_lines > 0 {
        1 + scrub_detail_lines // header + details
    } else {
        0
    };

    let scrub_with_sep = if scrub_section_lines > 0 {
        1 + scrub_section_lines // separator + section
    } else {
        0
    };

    let chunks = Layout::vertical([
        Constraint::Length(1),                            // [0] "Pool" header
        Constraint::Length(pool_detail_lines),            // [1] pool details
        Constraint::Length(1),                            // [2] separator
        Constraint::Length(1),                            // [3] "Disks" header
        Constraint::Length(model.disk_keys.len() as u16), // [4] disk table
        Constraint::Length(scrub_with_sep),               // [5] separator + scrub section
        Constraint::Min(0),                               // [6] spacer
        Constraint::Length(1),                            // [7] footer
    ])
    .split(inner);

    frame.render_widget(Paragraph::new("Pool"), chunks[0]);

    match &model.pool {
        PoolStatus::Loading => {
            frame.render_widget(
                Paragraph::new("loading...").style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        }
        PoolStatus::NotMounted => {
            frame.render_widget(
                Paragraph::new("not mounted").style(Style::default().fg(Color::Yellow)),
                chunks[1],
            );
        }
        PoolStatus::Mounted(pool) => {
            frame.render_widget(pool_view(pool, page_unit), chunks[1]);
        }
        PoolStatus::Error(msg) => {
            frame.render_widget(
                Paragraph::new(format!("error: {msg}")).style(Style::default().fg(Color::Red)),
                chunks[1],
            );
        }
    }

    frame.render_widget(Paragraph::new("Disks"), chunks[3]);
    let mut table_state = TableState::default().with_selected(Some(model.selected_disk));
    frame.render_stateful_widget(disk_table(model, page_unit), chunks[4], &mut table_state);

    if let PoolStatus::Mounted(pool) = &model.pool {
        let scrub_area = chunks[5];
        let scrub_chunks = Layout::vertical([
            Constraint::Length(1), // separator
            Constraint::Length(1), // "Scrub" header
            Constraint::Min(1),    // scrub details
        ])
        .split(scrub_area);
        frame.render_widget(Paragraph::new("Scrub"), scrub_chunks[1]);
        frame.render_widget(scrub_view(&pool.scrub), scrub_chunks[2]);
    }

    let footer = match model.probe_duration {
        Some(d) => format!("press q to quit  {}ms", d.as_millis()),
        None => "press q to quit".to_owned(),
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        chunks[7],
    );
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::parse::types::{ScrubState, ScrubTimestamp};
    use crate::tui::model::DiskUsage;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| view(model, frame)).unwrap();
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

    fn sample_disk_keys() -> Vec<String> {
        vec![
            "toshiba".to_owned(),
            "ironwolf".to_owned(),
            "wdc".to_owned(),
        ]
    }

    macro_rules! snap {
        ($value:expr) => {
            insta::with_settings!({ prepend_module_to_snapshot => false }, {
                insta::assert_snapshot!($value);
            });
        };
    }

    #[test]
    fn snapshot_loading() {
        let model = Model::new_demo(sample_disk_keys(), PoolStatus::Loading);
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_not_mounted() {
        let model = Model::new_demo(sample_disk_keys(), PoolStatus::NotMounted);
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_with_pool() {
        let disk_usage = HashMap::from([
            (
                "toshiba".to_owned(),
                DiskUsage {
                    size: 6_001_175_126_016, // ~5.5 TiB
                    data: 1_483_734_958_080, // ~1.3 TiB
                    metadata: 1_610_612_736, // ~1.5 GiB
                },
            ),
            (
                "ironwolf".to_owned(),
                DiskUsage {
                    size: 6_001_175_126_016,
                    data: 1_483_734_958_080,
                    metadata: 1_610_612_736,
                },
            ),
            (
                "wdc".to_owned(),
                DiskUsage {
                    size: 4_000_787_030_016, // ~3.6 TiB
                    data: 824_633_720_832,   // ~0.7 TiB
                    metadata: 1_073_741_824, // ~1.0 GiB
                },
            ),
        ]);
        let pool = PoolState {
            mount_point: "/mnt/storage".to_owned(),
            profile: "RAID1".to_owned(),
            health: "healthy".to_owned(),
            used: 2_308_094_370_816,  // ~2.1 TiB
            total: 5_937_955_045_376, // ~5.4 TiB
            disk_usage,
            scrub: ScrubState::Completed {
                started_at: ScrubTimestamp("Tue Feb 24 02:00:07 2026".to_owned()),
                error_count: 0,
            },
        };
        let model = Model::new_demo(sample_disk_keys(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_error() {
        let model = Model::new_demo(
            sample_disk_keys(),
            PoolStatus::Error("command failed: findmnt exited 1".to_owned()),
        );
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }
}
