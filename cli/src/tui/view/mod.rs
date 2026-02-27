use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
mod help;

use ratatui::widgets::{Block, Gauge, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use time::macros::format_description;
use time::PrimitiveDateTime;

use crate::hdparm::DrivePowerState;
use crate::parse::types::{ScrubState, SmartHealth};
use crate::tui::model::{Model, PoolState, PoolStatus, Tab};

fn format_timestamp(dt: &PrimitiveDateTime) -> String {
    let fmt = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    dt.format(&fmt).unwrap_or_else(|_| "unknown".to_owned())
}

fn timeago(dt: &PrimitiveDateTime, now: PrimitiveDateTime) -> Option<String> {
    let diff = now - *dt;
    if diff.is_negative() {
        return None;
    }
    let days = diff.whole_days();
    let minutes = diff.whole_minutes();
    Some(if days > 1 {
        format!("{days} days ago")
    } else if days == 1 {
        "1 day ago".to_owned()
    } else if minutes < 1 {
        "<1 min ago".to_owned()
    } else {
        format!("{minutes} min ago")
    })
}

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

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64) * 100.0
    }
}

fn section_block(title: &str) -> Block<'_> {
    Block::new()
        .borders(ratatui::widgets::Borders::TOP)
        .title(format!(" {title} "))
        .border_style(Style::default().fg(Color::Cyan))
        .padding(ratatui::widgets::Padding::left(1))
}

fn pool_table(pool: &PoolState, unit: ByteUnit) -> Table<'_> {
    let redundancy = match pool.profile.as_str() {
        "RAID1" => "2x (RAID1)",
        "single" => "None (single)",
        other => other,
    };
    let rows = [
        Row::new([
            "Path".to_owned(),
            format!("{} ({})", pool.mount_point, pool.health),
        ]),
        Row::new(["Redundancy".to_owned(), redundancy.to_owned()]),
        Row::new([
            "Usage".to_owned(),
            format!(
                "{:.0}% ({} / {} {})",
                percent(pool.used, pool.total),
                unit.format(pool.used),
                unit.format(pool.total),
                unit.suffix(),
            ),
        ]),
    ];
    let widths = [Constraint::Length(12), Constraint::Min(10)];
    Table::new(rows, widths).block(section_block("Pool"))
}

fn scrub_table(scrub: &ScrubState, now: PrimitiveDateTime) -> Table<'_> {
    let (rows, style) = match scrub {
        ScrubState::Never => (
            vec![Row::new(["Last run".to_owned(), "never".to_owned()])],
            None,
        ),
        ScrubState::Running { pct, total, rate } => {
            let detail = match pct {
                Some(p) => format!("now ({}% completed)", p),
                None => "now".to_owned(),
            };
            let mut rows = vec![Row::new(["Last run".to_owned(), detail])];
            if let Some(t) = total {
                rows.push(Row::new(["Total".to_owned(), t.clone()]));
            }
            if let Some(r) = rate {
                rows.push(Row::new(["Rate".to_owned(), r.clone()]));
            }
            (rows, None)
        }
        ScrubState::Completed {
            started_at,
            error_count,
            duration,
            total,
            rate,
        } => {
            let display = match timeago(&started_at.0, now) {
                Some(ago) => format!("{} ({})", format_timestamp(&started_at.0), ago),
                None => format_timestamp(&started_at.0),
            };
            let mut rows = vec![
                Row::new(["Last run".to_owned(), display]),
                Row::new(["Errors".to_owned(), error_count.to_string()]),
            ];
            if let Some(t) = total {
                rows.push(Row::new(["Total".to_owned(), t.clone()]));
            }
            if let Some(r) = rate {
                rows.push(Row::new(["Rate".to_owned(), r.clone()]));
            }
            if let Some(d) = duration {
                rows.push(Row::new(["Duration".to_owned(), d.clone()]));
            }
            (rows, None)
        }
        ScrubState::Unknown => (
            vec![Row::new(["Last run".to_owned(), "unknown".to_owned()])],
            Some(Style::default().fg(Color::DarkGray)),
        ),
    };
    let widths = [Constraint::Length(12), Constraint::Min(10)];
    let t = Table::new(rows, widths).block(section_block("Scrub"));
    match style {
        Some(s) => t.style(s),
        None => t,
    }
}

fn scrub_lines(scrub: &ScrubState) -> u16 {
    match scrub {
        ScrubState::Running { total, rate, .. } => {
            1 + total.is_some() as u16 + rate.is_some() as u16
        }
        ScrubState::Completed {
            total,
            rate,
            duration,
            ..
        } => 2 + total.is_some() as u16 + rate.is_some() as u16 + duration.is_some() as u16,
        _ => 1,
    }
}

fn smart_cell(health: &SmartHealth) -> Span<'static> {
    match health {
        SmartHealth::Healthy => Span::styled("ok", Style::default().fg(Color::DarkGray)),
        SmartHealth::Degraded => Span::styled("warning", Style::default().fg(Color::Yellow)),
        SmartHealth::Failing => Span::styled("failing", Style::default().fg(Color::Red)),
        SmartHealth::Unknown => Span::styled("-", Style::default().fg(Color::DarkGray)),
    }
}

fn power_label(state: &DrivePowerState) -> &'static str {
    match state {
        DrivePowerState::Active => "active",
        DrivePowerState::Idle => "idle",
        DrivePowerState::Standby => "standby",
        DrivePowerState::Unknown(_) => "unknown",
    }
}

fn disk_table(model: &Model, unit: ByteUnit) -> Table<'_> {
    let pool = model.pool.current();
    let disk_usage = pool.map(|p| &p.disk_usage);
    let smart_health = pool.map(|p| &p.smart_health);
    let power_state = pool.map(|p| &p.power_state);
    let header = Row::new(["", "Name", "SMART", "Power", "Usage"])
        .style(Style::default().fg(Color::DarkGray));
    let rows: Vec<Row> = model
        .disk_keys
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let smart_val = smart_health.and_then(|s| s.get(name));
            let smart_line = Line::from(match smart_val {
                Some(h) => smart_cell(h),
                None => Span::raw(""),
            });
            let power_val = power_state.and_then(|p| p.get(name));
            let power_cell = match power_val {
                Some(DrivePowerState::Standby) => Line::from(Span::styled(
                    "standby",
                    Style::default().fg(Color::DarkGray),
                )),
                Some(DrivePowerState::Idle) => {
                    Line::from(Span::styled("idle", Style::default().fg(Color::DarkGray)))
                }
                Some(s) => Line::from(power_label(s)),
                None => Line::from("\u{2014}"),
            };
            let num = Line::from(format!("{}", i + 1));
            let name_cell = Line::from(name.clone());
            match disk_usage.and_then(|u| u.get(name)) {
                Some(usage) => Row::new(vec![
                    num,
                    name_cell,
                    smart_line,
                    power_cell,
                    Line::from(format!(
                        "{:.0}%  {} / {} {}",
                        percent(usage.data, usage.size),
                        unit.format(usage.data),
                        unit.format(usage.size),
                        unit.suffix()
                    )),
                ]),
                None if disk_usage.is_some() => Row::new(vec![
                    num,
                    name_cell,
                    smart_line,
                    power_cell,
                    Line::from(Span::styled("missing", Style::default().fg(Color::Yellow))),
                ])
                .style(Style::default().add_modifier(Modifier::DIM)),
                None => Row::new(vec![
                    num,
                    name_cell,
                    smart_line,
                    power_cell,
                    Line::default(),
                ]),
            }
        })
        .collect();
    let longest_name_len = model
        .disk_keys
        .iter()
        .map(|k| k.len())
        .max()
        .unwrap_or(4)
        .max("Name".len()) as u16;
    let smart_width = smart_health
        .map(|s| {
            model
                .disk_keys
                .iter()
                .filter_map(|name| s.get(name))
                .map(|h| smart_cell(h).width())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("SMART".len()) as u16;
    let power_width = power_state
        .map(|p| {
            model
                .disk_keys
                .iter()
                .filter_map(|name| p.get(name))
                .map(|s| power_label(s).len())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("Power".len()) as u16;
    let widths = [
        Constraint::Length(1),
        Constraint::Length(longest_name_len),
        Constraint::Length(smart_width),
        Constraint::Length(power_width),
        Constraint::Min(10),
    ];
    Table::new(rows, widths)
        .header(header)
        .block(section_block("Disks"))
        .highlight_symbol("▶ ")
        .row_highlight_style(Style::default().fg(Color::Cyan))
}

fn page_unit(model: &Model) -> ByteUnit {
    let max_bytes = match model.pool.current() {
        Some(p) => p
            .disk_usage
            .values()
            .map(|u| u.size)
            .chain(Some(p.total))
            .max()
            .unwrap_or(0),
        None => 0,
    };
    ByteUnit::friendliest(max_bytes)
}

fn tab_bar(active: Tab) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        if *tab == active {
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

fn view_data(model: &Model, frame: &mut Frame, area: Rect, now: PrimitiveDateTime) {
    let page_unit = page_unit(model);

    // +1 per section for top border
    let pool_height: u16 = match model.pool.current() {
        Some(_) => 3 + 1 + 1, // +1 gauge
        None => 1 + 1,
    };
    let disk_height: u16 = model.disk_keys.len() as u16 + 2; // +1 border, +1 header
    let scrub_height: u16 = match model.pool.current() {
        Some(p) => scrub_lines(&p.scrub) + 1,
        None => 0,
    };

    let chunks = Layout::vertical([
        Constraint::Length(pool_height),  // [0] pool
        Constraint::Length(disk_height),  // [1] disks
        Constraint::Length(scrub_height), // [2] scrub
        Constraint::Min(0),               // [3] spacer
    ])
    .spacing(1)
    .split(area);

    match &model.pool {
        PoolStatus::Loading => {
            frame.render_widget(
                Paragraph::new("loading...")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(section_block("Pool")),
                chunks[0],
            );
        }
        PoolStatus::NotMounted => {
            frame.render_widget(
                Paragraph::new("not mounted")
                    .style(Style::default().fg(Color::Yellow))
                    .block(section_block("Pool")),
                chunks[0],
            );
        }
        PoolStatus::Mounted(pool)
        | PoolStatus::Refreshing(pool)
        | PoolStatus::ErrorStale(_, pool) => {
            let pool_inner = Layout::vertical([
                Constraint::Min(0),    // table
                Constraint::Length(1), // gauge
            ])
            .split(chunks[0]);
            frame.render_widget(pool_table(pool, page_unit), pool_inner[0]);
            let ratio = if pool.total > 0 {
                pool.used as f64 / pool.total as f64
            } else {
                0.0
            };
            frame.render_widget(
                Gauge::default()
                    .ratio(ratio)
                    .label("")
                    .gauge_style(Style::default().fg(Color::Cyan).bg(Color::DarkGray))
                    .block(Block::new().padding(ratatui::widgets::Padding::left(1))),
                pool_inner[1],
            );
        }
        PoolStatus::Error(msg) => {
            frame.render_widget(
                Paragraph::new(format!("error: {msg}"))
                    .style(Style::default().fg(Color::Red))
                    .block(section_block("Pool")),
                chunks[0],
            );
        }
    }

    let mut table_state = TableState::default().with_selected(Some(model.selected_disk));
    frame.render_stateful_widget(disk_table(model, page_unit), chunks[1], &mut table_state);

    if let Some(pool) = model.pool.current() {
        frame.render_widget(scrub_table(&pool.scrub, now), chunks[2]);
    }
}

fn view_placeholder(frame: &mut Frame, area: Rect, name: &str) {
    frame.render_widget(
        Paragraph::new(format!("{name} — coming soon")).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

pub fn view(model: &Model, frame: &mut Frame, now: PrimitiveDateTime) {
    let area = frame.area();

    let outer = Layout::vertical([
        Constraint::Length(1), // [0] tab bar
        Constraint::Length(1), // [1] spacer
        Constraint::Min(0),    // [2] tab body
        Constraint::Length(1), // [3] footer
    ])
    .split(area);

    frame.render_widget(tab_bar(model.tab), outer[0]);

    match model.tab {
        Tab::Data => view_data(model, frame, outer[2], now),
        Tab::Encryption => view_placeholder(frame, outer[2], "Encryption"),
        Tab::Sharing => view_placeholder(frame, outer[2], "Sharing"),
    }

    let hints = "q quit │ r reload │ <tab> switch tabs │ hjkl move selection │ ? help";
    let footer = match model.probe_duration {
        Some(d) => format!("{hints}  {}ms", d.as_millis()),
        None => hints.to_owned(),
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        outer[3],
    );

    if model.show_help {
        help::view_help(frame, area);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use super::*;
    use crate::hdparm::DrivePowerState;
    use crate::parse::types::{ScrubState, ScrubTimestamp, SmartHealth};
    use crate::tui::model::DiskUsage;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let now = time::macros::datetime!(2026-02-24 02:12:00);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| view(model, frame, now)).unwrap();
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
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_not_mounted() {
        let model = Model::new_demo(sample_disk_keys(), PoolStatus::NotMounted);
        let terminal = render(&model, 60, 22);
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
        let smart_health = HashMap::from([
            ("toshiba".to_owned(), SmartHealth::Healthy),
            ("ironwolf".to_owned(), SmartHealth::Degraded),
            ("wdc".to_owned(), SmartHealth::Unknown),
        ]);
        let power_state = HashMap::from([
            ("toshiba".to_owned(), DrivePowerState::Active),
            ("ironwolf".to_owned(), DrivePowerState::Standby),
            ("wdc".to_owned(), DrivePowerState::Idle),
        ]);
        let pool = PoolState {
            mount_point: "/mnt/storage".to_owned(),
            profile: "RAID1".to_owned(),
            health: "healthy".to_owned(),
            used: 2_308_094_370_816,  // ~2.1 TiB
            total: 5_937_955_045_376, // ~5.4 TiB
            disk_usage,
            smart_health,
            power_state,
            scrub: ScrubState::Completed {
                started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
                error_count: 0,
                duration: Some("0:00:00".to_owned()),
                total: Some("32.36MiB".to_owned()),
                rate: Some("32.34MiB/s".to_owned()),
            },
            probed_at: Instant::now(),
        };
        let model = Model::new_demo(sample_disk_keys(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_error() {
        let model = Model::new_demo(
            sample_disk_keys(),
            PoolStatus::Error("command failed: findmnt exited 1".to_owned()),
        );
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }
}
