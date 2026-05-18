use std::collections::HashMap;
use std::time::Instant;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
mod help;

use ratatui::Frame;
use ratatui::widgets::{Block, Borders, Cell, Clear, Padding, Paragraph, Row, Table, TableState};
use time::PrimitiveDateTime;
use time::macros::format_description;

use crate::config::FanControl;
use crate::parse::types::{BtrfsBgType, ScrubState, SmartHealth, UpsStatusFlag};
use crate::status::{BalanceReport, DiskErrors};
use crate::tui::model::{
    DaemonStatus, DiskLockState, DiskLuksState, DrivingDrive, FanReading, Model, PoolState,
    PoolStatus, Tab, TemperatureDiskId, TemperatureReading, TemperatureWatermark,
    UnpooledDiskRender, UpsSnapshot,
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

fn section_block_with_status<'a>(
    title: &'a str,
    status_text: &'a str,
    status_color: Color,
) -> Block<'a> {
    let title_line = Line::from(vec![
        Span::raw(format!(" {title} -- daemon: ")),
        Span::styled(status_text, Style::default().fg(status_color)),
        Span::raw(" "),
    ]);
    Block::new()
        .borders(Borders::TOP)
        .title(title_line)
        .border_style(Style::default().fg(Color::Cyan))
        .padding(Padding::left(1))
}

fn format_pwm(reading: &Option<FanReading>) -> String {
    match reading {
        Some(r) => {
            let pct = (r.pwm_raw as u32 * 100) / 255;
            format!("{}/255 {}%", r.pwm_raw, pct)
        }
        None => "-/-".to_owned(),
    }
}

fn format_rpm(reading: &Option<FanReading>) -> String {
    match reading {
        Some(r) => r.rpm.to_string(),
        None => "-".to_owned(),
    }
}

fn format_driving(d: &Option<DrivingDrive>) -> String {
    match d {
        Some(d) => format!("{}° {}", d.celsius, d.label),
        None => "-".to_owned(),
    }
}

fn format_curve(fc: &FanControl) -> String {
    format!(
        "{}-{}° -> {}-100%",
        fc.min_temp, fc.max_temp, fc.min_fan_speed_percent
    )
}

fn daemon_status_display(status: DaemonStatus) -> (&'static str, Color) {
    match status {
        DaemonStatus::Active => ("active", Color::Green),
        DaemonStatus::Transitioning => ("activating", Color::Yellow),
        DaemonStatus::Inactive => ("inactive", Color::Yellow),
        DaemonStatus::Failed => ("failed", Color::Red),
        DaemonStatus::Unknown => ("unknown", Color::DarkGray),
    }
}

/// Render the single-row Fans table. Sensor cells render dim when the
/// daemon is `Failed` or `Inactive` to signal "values are real but the
/// control loop isn't acting on them". Caller is responsible for wrapping
/// in `section_block_with_status`.
///
/// Precondition: `model.fan_control.is_some()` — the view_data layout
/// branch only reaches here when fan control is configured.
fn fan_section(model: &Model) -> Table<'_> {
    let header = Row::new(["  ", "PWM", "RPM", "Driving", "Curve"])
        .style(Style::default().fg(Color::DarkGray));

    let (fan, driving, daemon) = match &model.fan {
        Some(s) => (s.fan.clone(), s.driving.clone(), s.daemon),
        // Before first probe completes: render placeholders and leave
        // daemon as Unknown. The section still appears so its footprint
        // doesn't pop in/out as probes fire.
        None => (None, None, DaemonStatus::Unknown),
    };

    let fc = model
        .fan_control
        .as_ref()
        .expect("fan_section called without fan_control");

    let dim = matches!(daemon, DaemonStatus::Failed | DaemonStatus::Inactive);
    let sensor_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let row = Row::new(vec![
        Cell::from("1"),
        Cell::from(format_pwm(&fan)).style(sensor_style),
        Cell::from(format_rpm(&fan)).style(sensor_style),
        Cell::from(format_driving(&driving)).style(sensor_style),
        Cell::from(format_curve(fc)),
    ]);

    // Driving cell renders as "{celsius}° {label}". Budget the column at
    // (longest disk label) + 6 -- five for "999° " (3-digit temp + degree
    // + space) plus one of slack -- with a floor of 7 so the "Driving"
    // header doesn't clip on a system with very short or no disk names.
    let max_disk_name_len = model.disk_names.iter().map(|n| n.len()).max().unwrap_or(0);
    let driving_col_width = u16::try_from(max_disk_name_len + 6)
        .unwrap_or(u16::MAX)
        .max(7);

    let widths = [
        Constraint::Length(2),
        Constraint::Length(13),
        Constraint::Length(6),
        Constraint::Length(driving_col_width),
        Constraint::Min(20),
    ];
    Table::new(vec![row], widths).header(header)
}

/// Render severity color for the UPS status set. Ordering matters:
/// a critical flag (see `UpsStatusFlag::is_critical`) is red, OB alone
/// is yellow (on battery, not yet critical), OL is green (utility
/// power), everything else (including empty-set) is DarkGray.
///
/// Shares `UpsStatusFlag::is_critical` with
/// `preflight::check_ups_not_on_battery` so the two surfaces never
/// disagree about which tokens count as critical.
fn ups_severity_color(flags: &[UpsStatusFlag]) -> Color {
    if flags.iter().any(UpsStatusFlag::is_critical) {
        return Color::Red;
    }
    if flags.contains(&UpsStatusFlag::Ob) {
        return Color::Yellow;
    }
    if flags.contains(&UpsStatusFlag::Ol) {
        return Color::Green;
    }
    Color::DarkGray
}

/// Format ups.status tokens in `upsc` emission order.
fn format_ups_flags(flags: &[UpsStatusFlag]) -> String {
    if flags.is_empty() {
        return "--".into();
    }
    flags
        .iter()
        .map(UpsStatusFlag::as_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn format_ups_charge(snapshot: &UpsSnapshot) -> String {
    match snapshot.battery_charge_pct {
        Some(pct) => format!("{}%", pct),
        None => "--".into(),
    }
}

fn format_ups_runtime(snapshot: &UpsSnapshot) -> String {
    match snapshot.runtime_secs {
        Some(secs) => crate::ups::format_runtime(secs),
        None => "--".into(),
    }
}

fn format_ups_load(snapshot: &UpsSnapshot) -> String {
    match (snapshot.load_pct, snapshot.watts_estimated) {
        (Some(pct), Some(w)) => format!("{}% ({} W est.)", pct, w),
        (Some(pct), None) => format!("{}%", pct),
        _ => "--".into(),
    }
}

/// Render the single-row UPS table. Sensor cells render dim when the
/// UPS daemon is Failed or Inactive (mirror of the Fans section).
///
/// Precondition: `model.ups_config` is Some.
fn ups_section(model: &Model) -> Table<'_> {
    let header = Row::new(["Status", "Battery", "Runtime", "Load"])
        .style(Style::default().fg(Color::DarkGray));

    let snapshot = model.ups.as_ref();
    let daemon = snapshot.map(|s| s.daemon).unwrap_or(DaemonStatus::Unknown);
    let dim = matches!(daemon, DaemonStatus::Failed | DaemonStatus::Inactive);
    let sensor_style = if dim {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let status_text = snapshot
        .map(|s| format_ups_flags(&s.flags))
        .unwrap_or_else(|| "--".into());
    let status_color = snapshot
        .map(|s| ups_severity_color(&s.flags))
        .unwrap_or(Color::DarkGray);

    let row = Row::new(vec![
        Cell::from(status_text).style(Style::default().fg(status_color)),
        Cell::from(
            snapshot
                .map(format_ups_charge)
                .unwrap_or_else(|| "--".into()),
        )
        .style(sensor_style),
        Cell::from(
            snapshot
                .map(format_ups_runtime)
                .unwrap_or_else(|| "--".into()),
        )
        .style(sensor_style),
        Cell::from(snapshot.map(format_ups_load).unwrap_or_else(|| "--".into()))
            .style(sensor_style),
    ]);

    let widths = [
        Constraint::Length(10),
        Constraint::Length(9),
        Constraint::Length(9),
        Constraint::Min(18),
    ];
    Table::new(vec![row], widths).header(header)
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

fn format_duration_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{}h {}m {}s", h, m, s)
    } else if m > 0 {
        format!("{}m {}s", m, s)
    } else {
        format!("{}s", s)
    }
}

fn scrub_terminal_rows(
    status: Option<&str>,
    started_at: &crate::parse::types::ScrubTimestamp,
    error_count: u64,
    duration_secs: Option<u64>,
    total_bytes: Option<u64>,
    rate_bytes_per_sec: Option<u64>,
    now: PrimitiveDateTime,
) -> Vec<Row<'static>> {
    let display = match timeago(&started_at.0, now) {
        Some(ago) => format!("{} ({})", format_timestamp(&started_at.0), ago),
        None => format_timestamp(&started_at.0),
    };
    let mut rows = vec![Row::new(["Last run".to_owned(), display])];
    if let Some(status) = status {
        rows.push(Row::new(["Status".to_owned(), status.to_owned()]));
    }
    rows.push(Row::new(["Errors".to_owned(), error_count.to_string()]));
    if let Some(t) = total_bytes {
        let u = ByteUnit::friendliest(t);
        rows.push(Row::new([
            "Total".to_owned(),
            format!("{} {}", u.format(t), u.suffix()),
        ]));
    }
    if let Some(r) = rate_bytes_per_sec {
        let u = ByteUnit::friendliest(r);
        rows.push(Row::new([
            "Rate".to_owned(),
            format!("{} {}/s", u.format(r), u.suffix()),
        ]));
    }
    if let Some(secs) = duration_secs {
        rows.push(Row::new([
            "Duration".to_owned(),
            format_duration_secs(secs),
        ]));
    }
    rows
}

fn scrub_table(scrub: &ScrubState, now: PrimitiveDateTime) -> Table<'_> {
    let (rows, style) = match scrub {
        ScrubState::Never => (
            vec![Row::new(["Last run".to_owned(), "never".to_owned()])],
            None,
        ),
        ScrubState::Running {
            bytes_scrubbed,
            total_bytes,
            rate_bytes_per_sec,
            time_left_secs,
            eta,
            error_count,
            ..
        } => {
            // Status row: "running" or "running (14.78%)"
            let status_detail = match (*bytes_scrubbed, *total_bytes) {
                (Some(scrubbed), Some(total)) if total > 0 => {
                    let pct = scrubbed as f64 / total as f64 * 100.0;
                    format!("running ({:.2}%)", pct)
                }
                _ => "running".to_owned(),
            };
            let mut rows = vec![Row::new(["Status".to_owned(), status_detail])];

            // Progress row: "82.1 GiB / 555.4 GiB"
            if let Some(scrubbed) = bytes_scrubbed {
                let su = ByteUnit::friendliest(*scrubbed);
                let progress = match total_bytes {
                    Some(total) => {
                        let tu = ByteUnit::friendliest(*total);
                        format!(
                            "{} {} / {} {}",
                            su.format(*scrubbed),
                            su.suffix(),
                            tu.format(*total),
                            tu.suffix()
                        )
                    }
                    None => format!("{} {}", su.format(*scrubbed), su.suffix()),
                };
                rows.push(Row::new(["Progress".to_owned(), progress]));
            }

            if let Some(r) = rate_bytes_per_sec {
                let u = ByteUnit::friendliest(*r);
                rows.push(Row::new([
                    "Rate".to_owned(),
                    format!("{} {}/s", u.format(*r), u.suffix()),
                ]));
            }
            if let Some(secs) = time_left_secs {
                rows.push(Row::new([
                    "Time left".to_owned(),
                    format_duration_secs(*secs),
                ]));
            }
            if let Some(eta_ts) = eta {
                rows.push(Row::new(["ETA".to_owned(), format_timestamp(&eta_ts.0)]));
            }
            rows.push(Row::new(["Errors".to_owned(), error_count.to_string()]));
            (rows, None)
        }
        ScrubState::Finished {
            started_at,
            error_count,
            duration_secs,
            total_bytes,
            rate_bytes_per_sec,
        } => (
            scrub_terminal_rows(
                None,
                started_at,
                *error_count,
                *duration_secs,
                *total_bytes,
                *rate_bytes_per_sec,
                now,
            ),
            None,
        ),
        ScrubState::Aborted {
            started_at,
            error_count,
            duration_secs,
            total_bytes,
            rate_bytes_per_sec,
        } => (
            scrub_terminal_rows(
                Some("cancelled (will resume)"),
                started_at,
                *error_count,
                *duration_secs,
                *total_bytes,
                *rate_bytes_per_sec,
                now,
            ),
            None,
        ),
        ScrubState::Interrupted {
            started_at,
            error_count,
            duration_secs,
            total_bytes,
            rate_bytes_per_sec,
        } => (
            scrub_terminal_rows(
                Some("interrupted"),
                started_at,
                *error_count,
                *duration_secs,
                *total_bytes,
                *rate_bytes_per_sec,
                now,
            ),
            None,
        ),
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
            bytes_scrubbed,
            rate_bytes_per_sec,
            time_left_secs,
            eta,
            ..
        } => {
            // Status + Errors are always shown
            2 + bytes_scrubbed.is_some() as u16
                + rate_bytes_per_sec.is_some() as u16
                + time_left_secs.is_some() as u16
                + eta.is_some() as u16
        }
        ScrubState::Finished {
            total_bytes,
            rate_bytes_per_sec,
            duration_secs,
            ..
        } => {
            2 + total_bytes.is_some() as u16
                + rate_bytes_per_sec.is_some() as u16
                + duration_secs.is_some() as u16
        }
        ScrubState::Aborted {
            total_bytes,
            rate_bytes_per_sec,
            duration_secs,
            ..
        }
        | ScrubState::Interrupted {
            total_bytes,
            rate_bytes_per_sec,
            duration_secs,
            ..
        } => {
            3 + total_bytes.is_some() as u16
                + rate_bytes_per_sec.is_some() as u16
                + duration_secs.is_some() as u16
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
        UnpooledDiskRender::MapperHijacked => {
            Span::styled("mapper conflict", Style::default().fg(Color::Red))
        }
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
    let fan_enabled = model.fan_control.is_some();
    let ups_enabled = model.ups_config.is_some();
    // border + header + single data row
    let fan_height: u16 = 3;
    let ups_height: u16 = 3;

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(pool_height),
        Constraint::Length(disk_height),
    ];
    let fan_idx = if fan_enabled {
        constraints.push(Constraint::Length(fan_height));
        Some(constraints.len() - 1)
    } else {
        None
    };
    let ups_idx = if ups_enabled {
        constraints.push(Constraint::Length(ups_height));
        Some(constraints.len() - 1)
    } else {
        None
    };
    constraints.push(Constraint::Min(0));
    let chunks = Layout::vertical(constraints).spacing(1).split(area);

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

    if let Some(idx) = fan_idx {
        let daemon = model
            .fan
            .as_ref()
            .map(|s| s.daemon)
            .unwrap_or(DaemonStatus::Unknown);
        let (status_text, status_color) = daemon_status_display(daemon);
        frame.render_widget(
            fan_section(model).block(section_block_with_status("Fans", status_text, status_color)),
            chunks[idx],
        );
    }

    if let Some(idx) = ups_idx {
        let daemon = model
            .ups
            .as_ref()
            .map(|s| s.daemon)
            .unwrap_or(DaemonStatus::Unknown);
        let (status_text, status_color) = daemon_status_display(daemon);
        frame.render_widget(
            ups_section(model).block(section_block_with_status("UPS", status_text, status_color)),
            chunks[idx],
        );
    }
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
    let state = model.disk_luks_states.get(&disk_name);
    let lock_status = match state.map(|s| &s.lock) {
        Some(DiskLockState::Unlocked) => "unlocked",
        Some(DiskLockState::Locked) => "locked",
        Some(DiskLockState::Unknown) | None => "unknown",
    };
    let luks = state.and_then(|s| s.metadata.as_ref());
    let show_underlying_gone = matches!(
        state,
        Some(DiskLuksState {
            lock: DiskLockState::Unlocked,
            underlying_present: None,
            ..
        })
    );

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

    if show_underlying_gone {
        lines.push(Line::from(Span::styled(
            "underlying device gone",
            Style::default().fg(Color::Yellow),
        )));
    }

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
        .map(|p| p.alert_state.active())
        .unwrap_or(false);
    let alert_height: u16 = if alert_active { 1 } else { 0 };
    let stale_msg = model.pool.stale_error();
    let stale_height: u16 = if stale_msg.is_some() { 1 } else { 0 };

    let mut constraints = Vec::new();
    if alert_height > 0 {
        constraints.push(Constraint::Length(alert_height));
    }
    if advisory_height > 0 {
        constraints.push(Constraint::Length(advisory_height));
    }
    if stale_height > 0 {
        constraints.push(Constraint::Length(stale_height));
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

    if let Some(msg) = stale_msg {
        let line = Line::from(Span::styled(
            format!(" pool data stale -- last pool refresh failed: {msg} "),
            Style::default()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ));
        frame.render_widget(Paragraph::new(line), outer[off]);
        off += 1;
    }

    frame.render_widget(tab_bar(model.tab), outer[off]);

    match model.tab {
        Tab::Data => view_data(model, frame, outer[off + 2], now),
        Tab::Scrub => view_scrub(model, frame, outer[off + 2], now),
        Tab::Browse => crate::tui::browse::view::view_browse(model, frame, outer[off + 2]),
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
    use crate::parse::types::{BtrfsBgType, BtrfsDfEntry, BtrfsProfile};
    use crate::parse::types::{ScrubState, ScrubTimestamp};
    use crate::tui::demo::{sample_disk_luks_states, sample_disk_names, sample_pool};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
        fn uuid(raw: &str) -> LuksUuid {
            LuksUuid::parse(raw).expect("valid UUID in temperature fixture")
        }

        let mut pool = sample_pool();
        pool.disk_temperature_readings = HashMap::from([
            (
                "toshiba".to_owned(),
                TemperatureReading {
                    id: TemperatureDiskId::LuksUuid(uuid("11111111-1111-1111-1111-111111111111")),
                    celsius: 38,
                },
            ),
            (
                "ironwolf".to_owned(),
                TemperatureReading {
                    id: TemperatureDiskId::LuksUuid(uuid("22222222-2222-2222-2222-222222222222")),
                    celsius: 41,
                },
            ),
            // wdc intentionally absent -- simulates USB drive / SMART unavailable.
        ]);
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
        model.session_temperature_stats = HashMap::from([
            (
                TemperatureDiskId::LuksUuid(uuid("11111111-1111-1111-1111-111111111111")),
                TemperatureWatermark {
                    min_celsius: 32,
                    max_celsius: 45,
                    sample_count: 7,
                },
            ),
            (
                TemperatureDiskId::LuksUuid(uuid("22222222-2222-2222-2222-222222222222")),
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
        model.disk_luks_states = sample_disk_luks_states();
        model.show_disk_detail = true;
        let terminal = render(&model, 60, 30);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: disk detail uses model-level LUKS state even when the pool is
    //         not mounted.
    // Why it exists: the old view inferred lock state from mounted btrfs
    //      allocations, so an unmounted pool made open mappers look locked.
    // Scenario: toshiba is open with metadata while ironwolf is closed and
    //           the pool itself is `NotMounted`.
    #[test]
    fn snapshot_disk_detail_unmounted_mixed() {
        let mut states = sample_disk_luks_states();
        let ironwolf = states.get_mut("ironwolf").expect("ironwolf state");
        ironwolf.lock = DiskLockState::Locked;
        ironwolf.underlying_present = None;

        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::NotMounted);
        model.disk_luks_states = states;
        model.show_disk_detail = true;

        let unlocked = buffer_to_string(&render(&model, 60, 30));
        model.selected_disk = 1;
        let locked = buffer_to_string(&render(&model, 60, 30));
        snap!(format!(
            "-- toshiba --\n{unlocked}\n-- ironwolf --\n{locked}"
        ));
    }

    // Intent: disk detail surfaces hot-unplugged backing devices without
    //         reclassifying the mapper as locked.
    // Why it exists: mounted btrfs can still attest a null-underlying member
    //      by persisted devid, so lock state and physical presence must stay
    //      as separate render axes.
    // Scenario: mounted pool; toshiba is unlocked, but cryptsetup reports no
    //           backing device for the mapper.
    #[test]
    fn snapshot_disk_detail_null_underlying() {
        let mut states = sample_disk_luks_states();
        let toshiba = states.get_mut("toshiba").expect("toshiba state");
        toshiba.lock = DiskLockState::Unlocked;
        toshiba.underlying_present = None;

        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.disk_luks_states = states;
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

    // Intent: A failed pool re-probe renders a stale-data banner above the
    // tab bar, keeps the stale pool body visible, and styles the banner as a
    // bold yellow warning.
    // Why it exists: ErrorStale preserves the last good pool snapshot through
    // a transient probe failure. Dropping the error message would make stale
    // data look fresh, and a text-only snapshot cannot pin the visual style.
    // Scenario: User pressed 'r'; btrfs spawn failed transiently; the model is
    // now ErrorStale("btrfs spawn failed: ENOENT", prev_pool).
    #[test]
    fn snapshot_stale_banner() {
        let model = Model::new_demo(
            sample_disk_names(),
            PoolStatus::ErrorStale("btrfs spawn failed: ENOENT".to_owned(), sample_pool()),
        );
        let terminal = render(&model, 80, 24);
        let out = buffer_to_string(&terminal);

        assert!(
            out.contains("pool data stale -- last pool refresh failed: btrfs spawn failed: ENOENT"),
            "stale banner text missing from rendered output:\n{out}"
        );

        let buf = terminal.backend().buffer();
        let banner_y: u16 = 0;
        let mut checked = 0;
        for x in 0..buf.area.width {
            let cell = buf.cell((x, banner_y)).expect("cell in bounds");
            if cell.symbol() == " " {
                continue;
            }
            assert_eq!(cell.bg, Color::Yellow, "banner bg at x={x}");
            assert_eq!(cell.fg, Color::Black, "banner fg at x={x}");
            assert!(
                cell.modifier.contains(Modifier::BOLD),
                "banner BOLD modifier at x={x}"
            );
            checked += 1;
        }
        assert!(checked > 0, "banner row had no non-space cells");

        snap!(out);
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
     * Intent: Scrub tab renders all Running fields when a scrub is in progress.
     *
     * Why it exists: The Running variant was expanded with progress, time left,
     * ETA, and errors -- this verifies they all render in the scrub table.
     *
     * Scenario: User opens the Scrub tab mid-scrub on a healthy pool.
     */
    #[test]
    fn snapshot_scrub_tab_running() {
        let mut pool = sample_pool();
        pool.scrub = ScrubState::Running {
            started_at: Some(ScrubTimestamp(time::macros::datetime!(2026-04-16 18:28:44))),
            duration_secs: Some(358),
            time_left_secs: Some(2064),
            eta: Some(ScrubTimestamp(time::macros::datetime!(2026-04-16 19:09:10))),
            total_bytes: Some(596_353_253_376),
            bytes_scrubbed: Some(88_143_626_240),
            rate_bytes_per_sec: Some(246_211_246),
            error_count: 0,
        };
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(pool));
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

    // ----- Fan section snapshots -----

    use crate::config::{FanControl as FanControlCfg, Pwm};
    use crate::tui::model::{DaemonStatus, DrivingDrive, FanReading, FanSnapshot};

    fn sample_fan_control() -> FanControlCfg {
        FanControlCfg {
            pwm: Pwm {
                platform_device: "f71882fg.656".to_owned(),
                number: 2,
                min_start: 70,
                max_stop: 60,
            },
            min_temp: 30,
            max_temp: 40,
            min_fan_speed_percent: 20,
        }
    }

    fn sample_fan_snapshot_active() -> FanSnapshot {
        FanSnapshot {
            fan: Some(FanReading {
                pwm_raw: 215,
                rpm: 1240,
            }),
            driving: Some(DrivingDrive {
                label: "ironwolf".to_owned(),
                celsius: 38,
            }),
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        }
    }

    fn sample_fan_snapshot_no_hardware() -> FanSnapshot {
        FanSnapshot {
            fan: None,
            driving: Some(DrivingDrive {
                label: "ironwolf".to_owned(),
                celsius: 38,
            }),
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        }
    }

    fn sample_fan_snapshot_no_drives() -> FanSnapshot {
        FanSnapshot {
            fan: Some(FanReading {
                pwm_raw: 215,
                rpm: 1240,
            }),
            driving: None,
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        }
    }

    fn sample_fan_snapshot_daemon_failed() -> FanSnapshot {
        FanSnapshot {
            fan: Some(FanReading {
                pwm_raw: 215,
                rpm: 1240,
            }),
            driving: Some(DrivingDrive {
                label: "ironwolf".to_owned(),
                celsius: 38,
            }),
            daemon: DaemonStatus::Failed,
            probed_at: Instant::now(),
        }
    }

    // Intent: happy path -- pool mounted, fan_control set, snapshot
    //         populated, daemon active. Header shows "daemon: active",
    //         PWM/RPM/Driving/Curve cells render.
    // Why: this is the common-case render. Locking it in with a
    //      snapshot protects the layout against accidental breakage
    //      during unrelated refactors.
    // Scenario: running NAS with healthy fan control loop.
    #[test]
    fn snapshot_fans_section_active() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(sample_fan_snapshot_active());
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    /*
     * Intent: a 16-character drive label renders with both temperature
     *         and label visible in the Driving cell, not clipped.
     * Why it exists: prior to this fix the cell formatted as
     *         "{label} {celsius}°" inside a 16-col column, so a label
     *         like "toshiba-pro-02af" (exactly 16 chars) filled the
     *         column and the temperature was clipped. The user could
     *         not see the temperature of the hottest drive. The other
     *         snapshot fixtures all use the short label "ironwolf" (8
     *         chars), so they did not expose the bug. This test pins
     *         both the temp-leading format and the disk-name-driven
     *         column width jointly: reverting either one drops the
     *         asserted substring out of the rendered buffer.
     * Scenario: Toshiba N300 drives whose drivetemp labels are 16
     *         chars long.
     */
    #[test]
    fn fan_section_renders_long_label_with_temperature() {
        let disk_names = vec![
            "toshiba-pro-02af".to_owned(),
            "ironwolf".to_owned(),
            "wdc".to_owned(),
        ];
        let mut model = Model::new_demo(disk_names, PoolStatus::NotMounted);
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(FanSnapshot {
            fan: Some(FanReading {
                pwm_raw: 215,
                rpm: 1240,
            }),
            driving: Some(DrivingDrive {
                label: "toshiba-pro-02af".to_owned(),
                celsius: 35,
            }),
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        });
        let terminal = render(&model, 72, 28);
        let buf = buffer_to_string(&terminal);
        assert!(
            buf.contains("35° toshiba-pro-02af"),
            "expected '35° toshiba-pro-02af' in fans row, got:\n{buf}"
        );
    }

    // Intent: fan section still renders when the pool is NotMounted.
    // Why: fan control is a chassis safety loop, independent of LUKS
    //      or btrfs state (revision 1 of the plan). Hiding fan info
    //      when the pool is offline defeats the goal -- drives still
    //      generate heat while the pool is locked.
    // Scenario: user has booted but not yet run `braid unlock`.
    #[test]
    fn snapshot_fans_section_pool_offline() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::NotMounted);
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(sample_fan_snapshot_active());
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: fan hardware read failed but driving + daemon are fine.
    //         PWM and RPM render as "-/-" and "-"; Driving still shows
    //         the hottest drive.
    // Why: correlated failure (missing hwmon, fan disconnected) should
    //      degrade just the fan cells, not the whole section.
    // Scenario: kernel module for the Super I/O didn't load yet (or
    //           PWM glob matched zero paths).
    #[test]
    fn snapshot_fans_section_no_hardware() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(sample_fan_snapshot_no_hardware());
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: fan hardware fine but no drivetemp sensors -- Driving
    //         renders as "-"; PWM/RPM still render.
    // Why: drivetemp may be unavailable (kernel module not loaded, no
    //      SATA disks) even when the fan itself is readable. Row should
    //      still show actual PWM/RPM from the chassis fan.
    // Scenario: all-NVMe host that still runs a chassis fan.
    #[test]
    fn snapshot_fans_section_no_drives() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(sample_fan_snapshot_no_drives());
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: daemon failed -- status renders in red, sensor cells dim.
    // Why: daemon health is the source of truth for whether the control
    //      loop is actually running (revision 6). Without this signal a
    //      user looking at healthy-looking sensor values would miss that
    //      hddfancontrol crashed.
    // Scenario: `sudo systemctl stop hddfancontrol-braid.service` mid-session.
    #[test]
    fn snapshot_fans_section_daemon_failed() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan = Some(sample_fan_snapshot_daemon_failed());
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: pre-probe state (fan_control set, fan = None). Section
    //         renders with all-"-" placeholders and "daemon: unknown"
    //         in the header.
    // Why: avoids pop-in/pop-out of the whole section footprint on the
    //      first probe landing; the layout stabilizes at startup.
    // Scenario: TUI has just launched and the initial fan probe is
    //           still in flight.
    #[test]
    fn snapshot_fans_section_pre_probe() {
        let mut model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        model.fan_control = Some(sample_fan_control());
        model.fan = None;
        let terminal = render(&model, 72, 28);
        snap!(buffer_to_string(&terminal));
    }

    // Intent: no fan_control in config -> no Fans header anywhere in
    //         the buffer, layout unchanged from pre-feature.
    // Why: users who haven't opted in must not have a surprise new
    //      section take up screen space. "no fan_control in config"
    //      is the feature flag.
    // Scenario: default install without braid.fanControl.enable.
    #[test]
    fn snapshot_fans_section_disabled() {
        let model = Model::new_demo(sample_disk_names(), PoolStatus::Mounted(sample_pool()));
        let terminal = render(&model, 72, 28);
        let buf = buffer_to_string(&terminal);
        assert!(
            !buf.contains("Fans"),
            "Fans header should be absent:\n{buf}"
        );
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
            ("foxtrot".to_owned(), UnpooledDiskRender::MapperHijacked),
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
        assert_eq!(cell("foxtrot"), "mapper conflict");

        let foxtrot_span = unpooled_disk_status_cell(&pool, "foxtrot").expect("expected an entry");
        assert_eq!(foxtrot_span.style.fg, Some(Color::Red));
        assert!(
            unpooled_disk_status_cell(&pool, "hotel").is_none(),
            "names not in unpooled_disks must return None so callers can fall back"
        );
    }

    // --- UPS rendering tests ---

    fn flags_vec(tokens: &[UpsStatusFlag]) -> Vec<UpsStatusFlag> {
        tokens.to_vec()
    }

    // Intent: ups_severity_color routes LB/TESTFAIL/COMMBAD/FSD to Red
    // even when OL is simultaneously present.
    // Why: a driver reporting "OL LB" during a brief battery self-test
    // must not render green; the whole point of severity coloring is
    // that the worst flag wins. Regression here would give operators
    // false confidence.
    // Scenario: Covers each of the four critical flags alongside OL.
    #[test]
    fn ups_severity_critical_wins_over_ol() {
        for bad in [
            UpsStatusFlag::Lb,
            UpsStatusFlag::TestFail,
            UpsStatusFlag::CommBad,
            UpsStatusFlag::Fsd,
        ] {
            let flags = flags_vec(&[UpsStatusFlag::Ol, bad.clone()]);
            assert_eq!(
                ups_severity_color(&flags),
                Color::Red,
                "OL + {bad:?} must render Red"
            );
        }
    }

    // Intent: OB alone (without LB) renders Yellow.
    // Why: the "on battery, not yet critical" state is a meaningful
    // yellow-severity observation. A color regression here would
    // collapse the OB/LB distinction from the operator's point of
    // view.
    // Scenario: sustained utility outage before battery.charge.low
    // threshold is crossed.
    #[test]
    fn ups_severity_ob_alone_is_yellow() {
        let flags = flags_vec(&[UpsStatusFlag::Ob]);
        assert_eq!(ups_severity_color(&flags), Color::Yellow);
    }

    // Intent: OL alone renders Green.
    // Why: baseline health indicator must be green; anything else
    // would be a silent visual regression.
    // Scenario: UPS on utility power, healthy battery.
    #[test]
    fn ups_severity_ol_alone_is_green() {
        let flags = flags_vec(&[UpsStatusFlag::Ol]);
        assert_eq!(ups_severity_color(&flags), Color::Green);
    }

    // Intent: empty flag set renders DarkGray (unknown).
    // Why: the first-probe placeholder and the query-failed fail-closed
    // path both land here; both deserve the dim "nothing known yet"
    // color, not green or yellow.
    // Scenario: pre-first-probe Model::ups == None path, or UpsSnapshot
    // built from a query-failed fallback.
    #[test]
    fn ups_severity_empty_is_dark_gray() {
        let flags = Vec::new();
        assert_eq!(ups_severity_color(&flags), Color::DarkGray);
    }

    // Intent: OL + RB renders Green (RB is advisory, not critical).
    // Why: a battery-replace advisory is important for operator
    // awareness but does not imply imminent shutdown. Coloring it red
    // would cry wolf.
    // Scenario: old UPS with aging battery; utility power is fine.
    #[test]
    fn ups_severity_ol_plus_rb_is_green() {
        let flags = flags_vec(&[UpsStatusFlag::Ol, UpsStatusFlag::Rb]);
        assert_eq!(ups_severity_color(&flags), Color::Green);
    }

    // Intent: format_ups_flags renders tokens in input order, with no sort.
    // Why it exists: the Data tab is one UPS render surface; a future sort
    // here would diverge from `upsc`, `braid ups status`, --json, and the
    // Browse tab while single-flag snapshots kept passing.
    // Scenario: critical state with on-battery and low-battery flags in two
    // opposite arrival orders.
    #[test]
    fn format_ups_flags_preserves_insertion_order() {
        assert_eq!(
            format_ups_flags(&[UpsStatusFlag::Ob, UpsStatusFlag::Lb]),
            "OB LB"
        );
        assert_eq!(
            format_ups_flags(&[UpsStatusFlag::Lb, UpsStatusFlag::Ob]),
            "LB OB"
        );
    }

    // Intent: format_ups_load only annotates watts when both load% and
    // watts_estimated are available.
    // Why: partial data must not invent a watts figure (see plan --
    // "labeled 'estimated' when both ingredients are present").
    // Scenario: load present but no realpower.nominal -> no annotation.
    #[test]
    fn ups_format_load_skips_watts_when_unknown() {
        let mut s = UpsSnapshot {
            flags: Vec::new(),
            battery_charge_pct: None,
            runtime_secs: None,
            load_pct: Some(40),
            watts_estimated: None,
            raw_text: String::new(),
            daemon: DaemonStatus::Active,
            probed_at: Instant::now(),
        };
        assert_eq!(format_ups_load(&s), "40%");
        s.watts_estimated = Some(132);
        assert_eq!(format_ups_load(&s), "40% (132 W est.)");
    }
}
