use std::collections::HashMap;
use std::time::Instant;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
mod help;

use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use time::macros::format_description;
use time::PrimitiveDateTime;

use crate::parse::types::{BtrfsBgType, ScrubState, SmartHealth};
use crate::status::{BalanceReport, DiskErrors};
use crate::tui::model::{
    Model, PoolState, PoolStatus, Tab, TemperatureDiskId, TemperatureReading, TemperatureWatermark,
    UnpooledDiskRender,
};

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
            ByteUnit::MiB => format!("{:.1}", b / (1024.0 * 1024.0)),
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

fn pool_info(pool: &PoolState) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut lines = vec![Line::from(vec![
        Span::styled("Path       ", dim),
        Span::raw(pool.mount_point.0.clone()),
    ])];

    match &pool.balance {
        BalanceReport::Running {
            done_chunks,
            estimated_total_chunks,
            pct_left,
            ..
        } => {
            let pct_complete = 100u8.saturating_sub(*pct_left);
            lines.push(Line::from(vec![
                Span::styled("Balance    ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        "rebalancing -- {done_chunks}/{estimated_total_chunks} chunks ({pct_complete}% complete)"
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }
        BalanceReport::Paused {
            done_chunks,
            estimated_total_chunks,
            pct_left,
            ..
        } => {
            let pct_complete = 100u8.saturating_sub(*pct_left);
            lines.push(Line::from(vec![
                Span::styled("Balance    ", Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        "paused -- {done_chunks}/{estimated_total_chunks} chunks ({pct_complete}% complete)"
                    ),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }
        BalanceReport::Unknown => {
            lines.push(Line::from(Span::styled(
                "Balance    unknown",
                Style::default().fg(Color::Yellow),
            )));
        }
        BalanceReport::Idle => {}
    }

    if let Some(total) = pool.capacity_total_bytes {
        let pct = percent(pool.capacity_used_bytes, total);
        let used_unit = ByteUnit::friendliest(pool.capacity_used_bytes);
        let total_unit = ByteUnit::friendliest(total);
        lines.push(Line::from(vec![
            Span::styled("Usage      ", dim),
            Span::raw(format!(
                "{:.0}% {} {} / {} {} (Estimated)",
                pct,
                used_unit.format(pool.capacity_used_bytes),
                used_unit.suffix(),
                total_unit.format(total),
                total_unit.suffix(),
            )),
        ]));
    }

    Paragraph::new(lines)
}

fn pool_df_table(pool: &PoolState) -> Table<'_> {
    let header = Row::new(["Type", "Profile", "Used", "Allocated"])
        .style(Style::default().fg(Color::DarkGray));

    let mut entries: Vec<_> = pool
        .df_entries
        .iter()
        .filter(|e| e.bg_type != BtrfsBgType::GlobalReserve)
        .collect();
    entries.sort();

    let rows: Vec<Row> = entries
        .iter()
        .map(|entry| {
            let used_unit = ByteUnit::friendliest(entry.bg_used);
            let total_unit = ByteUnit::friendliest(entry.bg_total);
            Row::new([
                entry.bg_type.to_string(),
                entry.bg_profile.to_string(),
                format!("{} {}", used_unit.format(entry.bg_used), used_unit.suffix()),
                format!(
                    "{} {}",
                    total_unit.format(entry.bg_total),
                    total_unit.suffix()
                ),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Min(10),
    ];
    Table::new(rows, widths).header(header)
}

fn pool_balance_rows(pool: &PoolState) -> u16 {
    match pool.balance {
        BalanceReport::Running { .. } | BalanceReport::Paused { .. } | BalanceReport::Unknown => 1,
        BalanceReport::Idle => 0,
    }
}

fn scrub_table(scrub: &ScrubState, now: PrimitiveDateTime) -> Table<'_> {
    let (rows, style) = match scrub {
        ScrubState::Never => (
            vec![Row::new(["Last run".to_owned(), "never".to_owned()])],
            None,
        ),
        ScrubState::Running {
            pct,
            total_bytes,
            rate_bytes_per_sec,
        } => {
            let detail = match pct {
                Some(p) => format!("now ({}% completed)", p),
                None => "now".to_owned(),
            };
            let mut rows = vec![Row::new(["Last run".to_owned(), detail])];
            if let Some(t) = total_bytes {
                let u = ByteUnit::friendliest(*t);
                rows.push(Row::new([
                    "Total".to_owned(),
                    format!("{} {}", u.format(*t), u.suffix()),
                ]));
            }
            if let Some(r) = rate_bytes_per_sec {
                let u = ByteUnit::friendliest(*r);
                rows.push(Row::new([
                    "Rate".to_owned(),
                    format!("{} {}/s", u.format(*r), u.suffix()),
                ]));
            }
            (rows, None)
        }
        ScrubState::Completed {
            started_at,
            error_count,
            duration,
            total_bytes,
            rate_bytes_per_sec,
        } => {
            let display = match timeago(&started_at.0, now) {
                Some(ago) => format!("{} ({})", format_timestamp(&started_at.0), ago),
                None => format_timestamp(&started_at.0),
            };
            let mut rows = vec![
                Row::new(["Last run".to_owned(), display]),
                Row::new(["Errors".to_owned(), error_count.to_string()]),
            ];
            if let Some(t) = total_bytes {
                let u = ByteUnit::friendliest(*t);
                rows.push(Row::new([
                    "Total".to_owned(),
                    format!("{} {}", u.format(*t), u.suffix()),
                ]));
            }
            if let Some(r) = rate_bytes_per_sec {
                let u = ByteUnit::friendliest(*r);
                rows.push(Row::new([
                    "Rate".to_owned(),
                    format!("{} {}/s", u.format(*r), u.suffix()),
                ]));
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
        ScrubState::Running {
            total_bytes,
            rate_bytes_per_sec,
            ..
        } => 1 + total_bytes.is_some() as u16 + rate_bytes_per_sec.is_some() as u16,
        ScrubState::Completed {
            total_bytes,
            rate_bytes_per_sec,
            duration,
            ..
        } => {
            2 + total_bytes.is_some() as u16
                + rate_bytes_per_sec.is_some() as u16
                + duration.is_some() as u16
        }
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

/// Temperature cell for the disks table.
///
/// - `reading` is `None` (no temp reported this tick) -> `-`.
/// - `reading` present with `sample_count < 2` -> `<C>° --/--`.
/// - `reading` present with `sample_count >= 2` -> `<C>° <min>/<max>`.
fn temperature_cell(
    reading: Option<&TemperatureReading>,
    stats: &HashMap<TemperatureDiskId, TemperatureWatermark>,
) -> Line<'static> {
    let style = Style::default().fg(Color::DarkGray);
    match reading {
        None => Line::from(Span::styled("-", style)),
        Some(r) => {
            let range = match stats.get(&r.id) {
                Some(w) if w.sample_count >= 2 => {
                    format!("{}/{}", w.min_celsius, w.max_celsius)
                }
                _ => "--/--".to_owned(),
            };
            Line::from(Span::styled(format!("{}° {range}", r.celsius), style))
        }
    }
}

fn btrfs_cell(errors: &DiskErrors) -> Span<'static> {
    let total = errors.total();
    if total == 0 {
        Span::styled("0 err", Style::default().fg(Color::DarkGray))
    } else {
        Span::styled(format!("{total} err"), Style::default().fg(Color::Red))
    }
}

/// Status cell for a disk that is NOT in the live pool's `disk_usage`.
/// Resolves the configured render classification populated by
/// `tui::probe::probe_pool_for_tui` and returns the styled span.
///
/// Returns `None` when the disk has no entry in `unpooled_disks` — caller
/// should fall back to its existing "missing" rendering, which preserves
/// behavior for disks the unpooled probe couldn't classify (e.g. probe
/// errors).
fn unpooled_disk_status_cell(state: &PoolState, name: &str) -> Option<Span<'static>> {
    state.unpooled_disks.get(name).map(|render| match render {
        UnpooledDiskRender::Missing => Span::styled("missing", Style::default().fg(Color::Yellow)),
        UnpooledDiskRender::UnknownLuks => {
            Span::styled("unknown", Style::default().fg(Color::Yellow))
        }
        UnpooledDiskRender::LuksHeaderUnreadable => {
            Span::styled("LUKS header unreadable", Style::default().fg(Color::Red))
        }
        UnpooledDiskRender::LuksHeaderDamaged => {
            Span::styled("LUKS header damaged", Style::default().fg(Color::Red))
        }
        UnpooledDiskRender::WrongLuksVersion(v) => Span::styled(
            format!("LUKS{v} (unsupported)"),
            Style::default().fg(Color::Red),
        ),
    })
}

fn disk_table(model: &Model, unit: ByteUnit) -> Table<'_> {
    let pool = model.pool.current();
    let disk_usage = pool.map(|p| &p.disk_usage);
    let disk_transport = pool.map(|p| &p.disk_transport);
    let smart_health = pool.map(|p| &p.smart_health);
    let device_errors = pool.map(|p| &p.device_errors);
    let header = Row::new(["", "Name", "Bus", "SMART", "Temp", "btrfs", "Allocated"])
        .style(Style::default().fg(Color::DarkGray));
    let rows: Vec<Row> = model
        .disk_names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let transport_cell = Line::from(
                disk_transport
                    .and_then(|t| t.get(name))
                    .map(|s| s.as_str())
                    .unwrap_or("--"),
            );
            let smart_val = smart_health.and_then(|s| s.get(name));
            let smart_line = Line::from(match smart_val {
                Some(h) => smart_cell(h),
                None => Span::raw(""),
            });
            let temperature_line = match pool {
                Some(p) => temperature_cell(
                    p.disk_temperature_readings.get(name),
                    &model.session_temperature_stats,
                ),
                None => Line::from(Span::raw("")),
            };
            let num = Line::from(format!("{}", i + 1));
            let name_cell = Line::from(name.clone());
            let btrfs_line = Line::from(match device_errors.and_then(|e| e.get(name)) {
                Some(errors) => btrfs_cell(errors),
                None => Span::raw(""),
            });
            match disk_usage.and_then(|u| u.get(name)) {
                Some(usage) => Row::new(vec![
                    num,
                    name_cell,
                    transport_cell,
                    smart_line,
                    temperature_line,
                    btrfs_line,
                    Line::from(format!(
                        "{:.0}%  {} / {} {}",
                        percent(usage.allocated(), usage.size),
                        unit.format(usage.allocated()),
                        unit.format(usage.size),
                        unit.suffix()
                    )),
                ]),
                None if disk_usage.is_some() => {
                    let status_span = pool
                        .and_then(|p| unpooled_disk_status_cell(p, name))
                        .unwrap_or_else(|| {
                            Span::styled("missing", Style::default().fg(Color::Yellow))
                        });
                    Row::new(vec![
                        num,
                        name_cell,
                        transport_cell,
                        smart_line,
                        temperature_line,
                        btrfs_line,
                        Line::from(status_span),
                    ])
                    .style(Style::default().add_modifier(Modifier::DIM))
                }
                None => Row::new(vec![
                    num,
                    name_cell,
                    transport_cell,
                    smart_line,
                    temperature_line,
                    btrfs_line,
                    Line::default(),
                ]),
            }
        })
        .collect();
    let longest_name_len = model
        .disk_names
        .iter()
        .map(|k| k.len())
        .max()
        .unwrap_or(4)
        .max("Name".len()) as u16;
    let transport_width = disk_transport
        .map(|t| {
            model
                .disk_names
                .iter()
                .filter_map(|name| t.get(name))
                .map(|s| s.len())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("Bus".len()) as u16;
    let smart_width = smart_health
        .map(|s| {
            model
                .disk_names
                .iter()
                .filter_map(|name| s.get(name))
                .map(|h| smart_cell(h).width())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("SMART".len()) as u16;
    let temperature_width = pool
        .map(|p| {
            model
                .disk_names
                .iter()
                .map(|name| {
                    temperature_cell(
                        p.disk_temperature_readings.get(name),
                        &model.session_temperature_stats,
                    )
                    .width()
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("Temp".len()) as u16;
    let btrfs_width = device_errors
        .map(|e| {
            model
                .disk_names
                .iter()
                .filter_map(|name| e.get(name))
                .map(|err| btrfs_cell(err).width())
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        .max("btrfs".len()) as u16;
    let widths = [
        Constraint::Length(1),
        Constraint::Length(longest_name_len),
        Constraint::Length(transport_width),
        Constraint::Length(smart_width),
        Constraint::Length(temperature_width),
        Constraint::Length(btrfs_width),
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
        Some(p) => p.disk_usage.values().map(|u| u.size).max().unwrap_or(0),
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

fn view_data(model: &Model, frame: &mut Frame, area: Rect, _now: PrimitiveDateTime) {
    let page_unit = page_unit(model);

    // +1 per section for top border
    let pool_height: u16 = match model.pool.current() {
        Some(p) => {
            let df_rows = p
                .df_entries
                .iter()
                .filter(|e| e.bg_type != BtrfsBgType::GlobalReserve)
                .count() as u16;
            let usage_row = p.capacity_total_bytes.is_some() as u16;
            // border + Path + balance + usage + blank + header + entries
            1 + 1 + pool_balance_rows(p) + usage_row + 1 + 1 + df_rows
        }
        None => 1 + 1,
    };
    let disk_height: u16 = model.disk_names.len() as u16 + 2; // +1 border, +1 header
    let chunks = Layout::vertical([
        Constraint::Length(pool_height), // [0] pool
        Constraint::Length(disk_height), // [1] disks
        Constraint::Min(0),              // [2] spacer
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
            let usage_row = pool.capacity_total_bytes.is_some() as u16;
            let info_rows = 1 + pool_balance_rows(pool) + usage_row; // Path + balance + usage
            let df_rows = pool
                .df_entries
                .iter()
                .filter(|e| e.bg_type != BtrfsBgType::GlobalReserve)
                .count() as u16
                + 1; // +1 header
            let pool_inner = Layout::vertical([
                Constraint::Length(info_rows + 1), // section border + info lines
                Constraint::Length(1),             // blank line
                Constraint::Length(df_rows),       // header + data rows
            ])
            .split(chunks[0]);
            frame.render_widget(pool_info(pool).block(section_block("Pool")), pool_inner[0]);
            frame.render_widget(
                pool_df_table(pool).block(Block::new().padding(Padding::left(1))),
                pool_inner[2],
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
}

fn view_placeholder(frame: &mut Frame, area: Rect, name: &str) {
    frame.render_widget(
        Paragraph::new(format!("{name} -- coming soon"))
            .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn view_scrub(model: &Model, frame: &mut Frame, area: Rect, now: PrimitiveDateTime) {
    match &model.pool {
        PoolStatus::Loading => {
            frame.render_widget(
                Paragraph::new("loading...")
                    .style(Style::default().fg(Color::DarkGray))
                    .block(section_block("Scrub")),
                area,
            );
        }
        PoolStatus::NotMounted => {
            frame.render_widget(
                Paragraph::new("not mounted")
                    .style(Style::default().fg(Color::Yellow))
                    .block(section_block("Scrub")),
                area,
            );
        }
        PoolStatus::Mounted(pool)
        | PoolStatus::Refreshing(pool)
        | PoolStatus::ErrorStale(_, pool) => {
            let scrub_height = scrub_lines(&pool.scrub) + 1;
            let chunks = Layout::vertical([Constraint::Length(scrub_height), Constraint::Min(0)])
                .split(area);
            frame.render_widget(scrub_table(&pool.scrub, now), chunks[0]);
        }
        PoolStatus::Error(msg) => {
            frame.render_widget(
                Paragraph::new(format!("error: {msg}"))
                    .style(Style::default().fg(Color::Red))
                    .block(section_block("Scrub")),
                area,
            );
        }
    }
}

fn view_disk_detail(model: &Model, frame: &mut Frame, area: Rect) {
    let disk_name = match model.disk_names.get(model.selected_disk) {
        Some(name) => name.clone(),
        None => return,
    };
    let pool = model.pool.current();
    let has_usage = pool
        .map(|p| p.disk_usage.contains_key(&disk_name))
        .unwrap_or(false);
    let lock_status = if has_usage { "unlocked" } else { "locked" };
    let luks = pool.and_then(|p| p.luks_info.get(&disk_name));

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Disk       ", Style::default().fg(Color::DarkGray)),
            Span::raw(&disk_name),
        ]),
        Line::from(vec![
            Span::styled("Status     ", Style::default().fg(Color::DarkGray)),
            Span::raw(lock_status),
        ]),
    ];

    if let Some(info) = luks {
        lines.push(Line::from(vec![
            Span::styled("Cipher     ", Style::default().fg(Color::DarkGray)),
            Span::raw(&info.cipher),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Key size   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} bits", info.key_size_bits)),
        ]));
        lines.push(Line::from(vec![
            Span::styled("Keyslots   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!("{} of 32 slots used", info.keyslot_count)),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "LUKS metadata unavailable",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let dim = Style::default().fg(Color::DarkGray);
    let footer = Paragraph::new(vec![
        Line::default(),
        Line::from(Span::styled("r reload · Esc to go back", dim)),
    ]);

    let alloc_table = pool
        .and_then(|p| p.disk_usage.get(&disk_name))
        .map(|usage| {
            let mut rows: Vec<Row> = usage
                .allocations
                .iter()
                .map(|a| {
                    let unit = ByteUnit::friendliest(a.bytes);
                    Row::new([
                        a.alloc_type.clone(),
                        a.profile.clone(),
                        format!("{} {}", unit.format(a.bytes), unit.suffix()),
                    ])
                })
                .collect();
            let unalloc_unit = ByteUnit::friendliest(usage.unallocated);
            rows.push(
                Row::new([
                    "Unallocated".to_owned(),
                    String::new(),
                    format!(
                        "{} {}",
                        unalloc_unit.format(usage.unallocated),
                        unalloc_unit.suffix()
                    ),
                ])
                .style(dim),
            );
            let header = Row::new(["Type", "Profile", "Size"]).style(dim);
            Table::new(
                rows,
                [
                    Constraint::Length(11),
                    Constraint::Length(11),
                    Constraint::Min(7),
                ],
            )
            .header(header)
            .block(
                Block::new()
                    .borders(Borders::TOP)
                    .title("Allocations ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
        });

    let errors_table = pool
        .and_then(|p| p.device_errors.get(&disk_name))
        .map(|errors| {
            let err_rows: Vec<Row> = [
                ("read", errors.read),
                ("write", errors.write),
                ("flush", errors.flush),
                ("corruption", errors.corruption),
                ("generation", errors.generation),
            ]
            .into_iter()
            .map(|(label, count)| {
                let style = if count > 0 {
                    Style::default().fg(Color::Red)
                } else {
                    Style::default()
                };
                Row::new([Span::raw(label), Span::styled(count.to_string(), style)])
            })
            .collect();
            Table::new(err_rows, [Constraint::Length(15), Constraint::Min(5)]).block(
                Block::new()
                    .borders(Borders::TOP)
                    .title("btrfs Device Errors ")
                    .border_style(Style::default().fg(Color::Cyan)),
            )
        });

    let info_height = lines.len() as u16;
    // alloc section: 1 border + 1 header + data rows (incl. unallocated) + 1 padding left
    let alloc_height = alloc_table
        .as_ref()
        .map(|_| {
            let data_rows = pool
                .and_then(|p| p.disk_usage.get(&disk_name))
                .map(|u| u.allocations.len() as u16 + 1) // +1 for unallocated
                .unwrap_or(0);
            1 + 1 + 1 + data_rows // spacer + border + header + rows
        })
        .unwrap_or(0);
    let errors_height = if errors_table.is_some() {
        1 + 1 + 5u16 // spacer + border + 5 error rows
    } else {
        0
    };
    let footer_height = 2u16; // blank line + text
    let total_content = info_height + alloc_height + errors_height + footer_height;

    let width = 48u16.min(area.width);
    let height = (total_content + 2).min(area.height); // +2 for popup border
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup);
    let outer_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Disk Detail ")
        .padding(Padding::horizontal(1));
    let inner = outer_block.inner(popup);
    frame.render_widget(outer_block, popup);

    match (alloc_table, errors_table) {
        (Some(alloc), Some(errors)) => {
            let regions = Layout::vertical([
                Constraint::Length(info_height),
                Constraint::Length(1), // spacer
                Constraint::Length(alloc_height - 1),
                Constraint::Length(1), // spacer
                Constraint::Length(errors_height - 1),
                Constraint::Length(footer_height),
            ])
            .split(inner);
            frame.render_widget(Paragraph::new(lines), regions[0]);
            frame.render_widget(alloc, regions[2]);
            frame.render_widget(errors, regions[4]);
            frame.render_widget(footer, regions[5]);
        }
        (Some(alloc), None) => {
            let regions = Layout::vertical([
                Constraint::Length(info_height),
                Constraint::Length(1),
                Constraint::Length(alloc_height - 1),
                Constraint::Length(footer_height),
            ])
            .split(inner);
            frame.render_widget(Paragraph::new(lines), regions[0]);
            frame.render_widget(alloc, regions[2]);
            frame.render_widget(footer, regions[3]);
        }
        (None, Some(errors)) => {
            let regions = Layout::vertical([
                Constraint::Length(info_height),
                Constraint::Length(1),
                Constraint::Length(errors_height - 1),
                Constraint::Length(footer_height),
            ])
            .split(inner);
            frame.render_widget(Paragraph::new(lines), regions[0]);
            frame.render_widget(errors, regions[2]);
            frame.render_widget(footer, regions[3]);
        }
        (None, None) => {
            let regions = Layout::vertical([
                Constraint::Length(info_height),
                Constraint::Length(footer_height),
            ])
            .split(inner);
            frame.render_widget(Paragraph::new(lines), regions[0]);
            frame.render_widget(footer, regions[1]);
        }
    }
}

pub fn view(model: &Model, frame: &mut Frame, now: PrimitiveDateTime) {
    let area = frame.area();
    let advisory_height = model.advisories.len() as u16;
    let alert_active = model
        .pool
        .current()
        .map(|p| p.alert_state.active)
        .unwrap_or(false);
    let alert_height: u16 = if alert_active { 1 } else { 0 };

    let mut constraints = Vec::new();
    if alert_height > 0 {
        constraints.push(Constraint::Length(alert_height));
    }
    if advisory_height > 0 {
        constraints.push(Constraint::Length(advisory_height));
    }
    constraints.push(Constraint::Length(1)); // tab bar
    constraints.push(Constraint::Length(1)); // spacer
    constraints.push(Constraint::Min(0)); // tab body
    constraints.push(Constraint::Length(1)); // footer

    let outer = Layout::vertical(constraints).split(area);
    let mut off: usize = 0;

    if alert_active {
        let alert_line = Line::from(Span::styled(
            " ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence. ",
            Style::default()
                .bg(Color::Red)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(alert_line), outer[off]);
        off += 1;
    }

    if advisory_height > 0 {
        let lines: Vec<Line> = model
            .advisories
            .iter()
            .map(|a| {
                Line::from(Span::styled(
                    format!("warning: {a}"),
                    Style::default().fg(Color::Yellow),
                ))
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), outer[off]);
        off += 1;
    }

    frame.render_widget(tab_bar(model.tab), outer[off]);

    match model.tab {
        Tab::Data => view_data(model, frame, outer[off + 2], now),
        Tab::Scrub => view_scrub(model, frame, outer[off + 2], now),
        Tab::Sharing => view_placeholder(frame, outer[off + 2], "Sharing"),
    }

    let spinning =
        model.pool.is_inflight() || model.spinner_deadline.is_some_and(|d| Instant::now() < d);

    let reload = if spinning {
        const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let ch = SPINNER[(model.frame as usize / 8) % SPINNER.len()];
        format!("Reload: r {ch}")
    } else {
        match model.probe_duration {
            Some(d) => format!("Reload: r ({}ms)", d.as_millis()),
            None => "Reload: r".to_owned(),
        }
    };
    let footer = format!("Quit: q │ Help: ? │ Reset temp hi/lo: R │ {reload}");
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        outer[off + 3],
    );

    if model.show_disk_detail {
        view_disk_detail(model, frame, area);
    }

    if model.show_help {
        help::view_help(frame, area);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::parse::types::{BtrfsBgType, BtrfsDfEntry, BtrfsProfile, DeviceAllocation};
    use crate::parse::types::{ScrubState, ScrubTimestamp, SmartHealth};
    use crate::tui::model::{DiskLuksInfo, DiskUsage};
    use crate::types::MountPoint;
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

    pub(crate) fn sample_disk_names() -> Vec<String> {
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
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Loading);
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_not_mounted() {
        let model = Model::new_demo(sample_disk_names(), PoolStatus::NotMounted);
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    pub(crate) fn sample_pool() -> PoolState {
        let disk_usage = HashMap::from([
            (
                "toshiba".to_owned(),
                DiskUsage {
                    size: 6_001_175_126_016,
                    allocations: vec![
                        DeviceAllocation {
                            alloc_type: "Data".into(),
                            profile: "RAID1".into(),
                            bytes: 1_483_734_958_080,
                        },
                        DeviceAllocation {
                            alloc_type: "Metadata".into(),
                            profile: "DUP".into(),
                            bytes: 1_610_612_736,
                        },
                        DeviceAllocation {
                            alloc_type: "System".into(),
                            profile: "DUP".into(),
                            bytes: 16_777_216,
                        },
                    ],
                    unallocated: 4_515_816_777_984,
                },
            ),
            (
                "ironwolf".to_owned(),
                DiskUsage {
                    size: 6_001_175_126_016,
                    allocations: vec![
                        DeviceAllocation {
                            alloc_type: "Data".into(),
                            profile: "RAID1".into(),
                            bytes: 1_483_734_958_080,
                        },
                        DeviceAllocation {
                            alloc_type: "Metadata".into(),
                            profile: "DUP".into(),
                            bytes: 1_610_612_736,
                        },
                        DeviceAllocation {
                            alloc_type: "System".into(),
                            profile: "DUP".into(),
                            bytes: 16_777_216,
                        },
                    ],
                    unallocated: 4_515_816_777_984,
                },
            ),
            (
                "wdc".to_owned(),
                DiskUsage {
                    size: 4_000_787_030_016,
                    allocations: vec![
                        DeviceAllocation {
                            alloc_type: "Data".into(),
                            profile: "RAID1".into(),
                            bytes: 824_633_720_832,
                        },
                        DeviceAllocation {
                            alloc_type: "Metadata".into(),
                            profile: "DUP".into(),
                            bytes: 1_073_741_824,
                        },
                        DeviceAllocation {
                            alloc_type: "System".into(),
                            profile: "DUP".into(),
                            bytes: 16_777_216,
                        },
                    ],
                    unallocated: 3_175_062_790_144,
                },
            ),
        ]);
        let smart_health = HashMap::from([
            ("toshiba".to_owned(), SmartHealth::Healthy),
            ("ironwolf".to_owned(), SmartHealth::Degraded),
            ("wdc".to_owned(), SmartHealth::Unknown),
        ]);
        let luks_info = HashMap::from([
            (
                "toshiba".to_owned(),
                DiskLuksInfo {
                    cipher: "aes-xts-plain64".to_owned(),
                    key_size_bits: 512,
                    keyslot_count: 1,
                },
            ),
            (
                "ironwolf".to_owned(),
                DiskLuksInfo {
                    cipher: "aes-xts-plain64".to_owned(),
                    key_size_bits: 512,
                    keyslot_count: 1,
                },
            ),
            (
                "wdc".to_owned(),
                DiskLuksInfo {
                    cipher: "aes-xts-plain64".to_owned(),
                    key_size_bits: 512,
                    keyslot_count: 1,
                },
            ),
        ]);
        let disk_transport = HashMap::from([
            ("toshiba".to_owned(), "sata".to_owned()),
            ("ironwolf".to_owned(), "sata".to_owned()),
            ("wdc".to_owned(), "usb".to_owned()),
        ]);
        PoolState {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            df_entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 2_308_094_370_816,
                    bg_total: 5_937_955_045_376,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Metadata,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 1_610_612_736,
                    bg_total: 2_147_483_648,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::System,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 16_384,
                    bg_total: 16_777_216,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::GlobalReserve,
                    bg_profile: BtrfsProfile::Single,
                    bg_used: 0,
                    bg_total: 5_767_168,
                },
            ],
            disk_usage,
            disk_transport,
            smart_health,
            disk_temperature_readings: HashMap::new(),
            luks_info,
            device_errors: HashMap::from([
                (
                    "toshiba".to_owned(),
                    DiskErrors {
                        read: 0,
                        write: 0,
                        flush: 0,
                        corruption: 0,
                        generation: 0,
                    },
                ),
                (
                    "ironwolf".to_owned(),
                    DiskErrors {
                        read: 3,
                        write: 0,
                        flush: 0,
                        corruption: 0,
                        generation: 0,
                    },
                ),
                (
                    "wdc".to_owned(),
                    DiskErrors {
                        read: 0,
                        write: 0,
                        flush: 0,
                        corruption: 0,
                        generation: 0,
                    },
                ),
            ]),
            unpooled_disks: HashMap::new(),
            alert_state: crate::alert::AlertState {
                active: false,
                causes: vec![],
            },
            scrub: ScrubState::Completed {
                started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
                error_count: 0,
                duration: Some("0:00:00".to_owned()),
                total_bytes: Some(33_931_264),
                rate_bytes_per_sec: Some(33_910_682),
            },
            balance: BalanceReport::Idle,
            capacity_total_bytes: Some(8_001_568_641_024),
            capacity_used_bytes: 2_308_094_370_816,
            probed_at: Instant::now(),
        }
    }

    #[test]
    fn snapshot_with_pool() {
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        let terminal = render(&model, 60, 24);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: verify that the Temp column renders all three of its branches
    //         in the same frame -- `<C>° <min>/<max>` once we have >=2
    //         samples, `<C>° --/--` right after the first sample, and `-`
    //         when smartctl didn't surface a temperature for the drive.
    // Why: these three render rules are the contract with users testing
    //      fan setups; a silent regression in any of them (e.g. showing
    //      a degenerate `38/38` before history exists, or dropping the
    //      current temperature when history is missing) would quietly
    //      break the feature without tripping any other test.
    // Scenario: 3-drive pool; toshiba has two samples recorded (32/45);
    //           ironwolf has one sample (no range yet); wdc is USB and
    //           smartctl returned no temperature at all.
    #[test]
    fn snapshot_temperature_column() {
        use crate::types::LuksUuid;
        let mut pool = sample_pool();
        pool.disk_temperature_readings = HashMap::from([
            (
                "toshiba".to_owned(),
                TemperatureReading {
                    id: TemperatureDiskId::LuksUuid(LuksUuid(
                        "11111111-1111-1111-1111-111111111111".to_owned(),
                    )),
                    celsius: 38,
                },
            ),
            (
                "ironwolf".to_owned(),
                TemperatureReading {
                    id: TemperatureDiskId::LuksUuid(LuksUuid(
                        "22222222-2222-2222-2222-222222222222".to_owned(),
                    )),
                    celsius: 41,
                },
            ),
            // wdc intentionally absent -- simulates USB drive / SMART unavailable.
        ]);
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
        model.session_temperature_stats = HashMap::from([
            (
                TemperatureDiskId::LuksUuid(LuksUuid(
                    "11111111-1111-1111-1111-111111111111".to_owned(),
                )),
                TemperatureWatermark {
                    min_celsius: 32,
                    max_celsius: 45,
                    sample_count: 7,
                },
            ),
            (
                TemperatureDiskId::LuksUuid(LuksUuid(
                    "22222222-2222-2222-2222-222222222222".to_owned(),
                )),
                TemperatureWatermark {
                    min_celsius: 41,
                    max_celsius: 41,
                    sample_count: 1,
                },
            ),
        ]);
        let terminal = render(&model, 70, 24);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_disk_detail() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.show_disk_detail = true;
        let terminal = render(&model, 60, 30);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_balance_running() {
        let mut pool = sample_pool();
        pool.balance = BalanceReport::Running {
            done_chunks: 108,
            estimated_total_chunks: 160,
            considered_chunks: 120,
            pct_left: 32,
        };
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 26);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_balance_unknown() {
        let mut pool = sample_pool();
        pool.balance = BalanceReport::Unknown;
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 26);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_mixed_data_profile() {
        let mut pool = sample_pool();
        pool.df_entries = vec![
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Data,
                bg_profile: BtrfsProfile::Single,
                bg_used: 536_870_912,
                bg_total: 1_073_741_824,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Data,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 2_308_094_370_816,
                bg_total: 5_937_955_045_376,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Metadata,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 1_610_612_736,
                bg_total: 2_147_483_648,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::System,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 16_384,
                bg_total: 16_777_216,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::GlobalReserve,
                bg_profile: BtrfsProfile::Single,
                bg_used: 0,
                bg_total: 5_767_168,
            },
        ];
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 26);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_error() {
        let model = Model::new_demo(
            sample_disk_names(),
            PoolStatus::Error("command failed: findmnt exited 1".to_owned()),
        );
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Footer shows a braille spinner character when a probe is
     * inflight and the spinner deadline hasn't expired.
     *
     * Why it exists: Confirms the spinner is visible during reload so
     * the user gets visual feedback that a refresh is in progress.
     *
     * Scenario: User presses 'r', the probe is still running, and the
     * spinner deadline is in the future.
     */
    #[test]
    fn snapshot_footer_spinner_inflight() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Refreshing(sample_pool()));
        model.spinner_deadline = Some(Instant::now() + Duration::from_secs(10));
        model.frame = 0;
        let terminal = render(&model, 60, 24);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Footer shows probe duration (not a spinner) when the
     * spinner deadline has expired and the pool is mounted.
     *
     * Why it exists: After the minimum spinner duration elapses, the
     * footer should revert to showing the probe timing.
     *
     * Scenario: Probe completed 50ms ago, spinner deadline is in the
     * past, footer shows "(42ms)".
     */
    #[test]
    fn snapshot_footer_duration_after_spinner() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.probe_duration = Some(Duration::from_millis(42));
        model.spinner_deadline = None;
        let terminal = render(&model, 60, 24);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Advisory warning renders as yellow text below scrub section.
     * Why: users need a visible nudge to copy LUKS header backups offsite.
     * Scenario: braid add/enroll created header backups; tui shows warning.
     */
    #[test]
    fn snapshot_with_advisory() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.advisories = vec![
            "LUKS header backups exist in /var/lib/braid/luks-headers -- copy offsite and delete local copies".to_owned(),
        ];
        let terminal = render(&model, 80, 26);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Scrub tab renders scrub details when the pool is mounted.
     *
     * Why it exists: Scrub info moved from the Data tab to its own top-level
     * tab; this verifies the new tab renders the scrub table correctly.
     *
     * Scenario: User switches to the Scrub tab on a healthy mounted pool
     * that has completed a scrub.
     */
    #[test]
    fn snapshot_scrub_tab() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.tab = Tab::Scrub;
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: Scrub tab shows "not mounted" when the pool is offline.
     *
     * Why it exists: view_scrub must handle non-mounted states distinctly
     * rather than collapsing them into a generic loading message.
     *
     * Scenario: User switches to the Scrub tab while the pool is not mounted.
     */
    #[test]
    fn snapshot_scrub_tab_not_mounted() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::NotMounted);
        model.tab = Tab::Scrub;
        let terminal = render(&model, 60, 22);
        snap!(buffer_to_string(&terminal));
    }

    /// Intent: unpooled_disk_status_cell must surface a distinct label per
    /// UnpooledDiskRender variant so the disk table can differentiate
    /// "missing", "valid LUKS not in pool", "header unreadable", and
    /// "header damaged" instead of collapsing them all into "missing".
    ///
    /// Why: prior to the unpooled-disks plumbing, the TUI rendered every
    /// declared-but-unrepresented disk as plain "missing", hiding the
    /// distinction between an unplugged cable, a stale-LUKS disk, and a
    /// corrupted header. The helper is the single point that materializes
    /// the new vocabulary into ratatui spans.
    ///
    /// Scenario: a fake PoolState with one entry per UnpooledDiskRender
    /// variant in `unpooled_disks`. Each name resolves to its expected
    /// label.
    #[test]
    fn unpooled_disk_status_cell_renders_each_variant() {
        let mut pool = sample_pool();
        pool.unpooled_disks = HashMap::from([
            ("alpha".to_owned(), UnpooledDiskRender::Missing),
            ("bravo".to_owned(), UnpooledDiskRender::UnknownLuks),
            (
                "charlie".to_owned(),
                UnpooledDiskRender::LuksHeaderUnreadable,
            ),
            ("delta".to_owned(), UnpooledDiskRender::LuksHeaderDamaged),
            ("echo".to_owned(), UnpooledDiskRender::WrongLuksVersion(1)),
        ]);

        let cell = |name: &str| {
            unpooled_disk_status_cell(&pool, name)
                .expect("expected an entry")
                .content
                .into_owned()
        };

        assert_eq!(cell("alpha"), "missing");
        assert_eq!(cell("bravo"), "unknown");
        assert_eq!(cell("charlie"), "LUKS header unreadable");
        assert_eq!(cell("delta"), "LUKS header damaged");
        assert_eq!(cell("echo"), "LUKS1 (unsupported)");
        assert!(
            unpooled_disk_status_cell(&pool, "foxtrot").is_none(),
            "names not in unpooled_disks must return None so callers can fall back"
        );
    }
}
