use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};

use crate::tui::app::{Model, PoolState, PoolStatus};

const BAR_WIDTH: usize = 28;

fn format_bytes(bytes: u64) -> String {
    const TIB: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let b = bytes as f64;
    if b >= TIB {
        format!("{:.1} TiB", b / TIB)
    } else {
        format!("{:.1} GiB", b / GIB)
    }
}

fn usage_bar(used: u64, total: u64) -> String {
    let ratio = if total == 0 {
        0.0
    } else {
        (used as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (ratio * BAR_WIDTH as f64).round() as usize;
    let empty = BAR_WIDTH - filled;
    format!(
        "{}{}  {} / {}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
        format_bytes(used),
        format_bytes(total),
    )
}

fn pool_view(pool: &PoolState) -> Paragraph<'_> {
    let lines = vec![
        Line::from(format!(
            "Pool: {} {} {}",
            pool.mount_point, pool.profile, pool.health
        )),
        Line::from(format!("Data: {}", usage_bar(pool.used, pool.total))),
    ];
    Paragraph::new(lines)
}

fn disk_list(model: &Model) -> Paragraph<'_> {
    let lines: Vec<Line> = std::iter::once(Line::from("Disks"))
        .chain(
            model
                .disk_keys
                .iter()
                .enumerate()
                .map(|(i, name)| Line::from(format!("  {}  {}", i + 1, name))),
        )
        .collect();
    Paragraph::new(lines)
}

pub fn view(model: &Model, frame: &mut Frame) {
    let area = frame.area();

    let outer = Block::bordered()
        .title(" braid ")
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let pool_lines: u16 = match &model.pool {
        PoolStatus::Mounted(_) => 2,
        PoolStatus::Loading | PoolStatus::NotMounted | PoolStatus::Error(_) => 1,
    };

    let chunks = Layout::vertical([
        Constraint::Length(pool_lines),
        Constraint::Length(1), // separator
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    match &model.pool {
        PoolStatus::Loading => {
            frame.render_widget(
                Paragraph::new("Pool: loading...").style(Style::default().fg(Color::DarkGray)),
                chunks[0],
            );
        }
        PoolStatus::NotMounted => {
            frame.render_widget(
                Paragraph::new("Pool: not mounted").style(Style::default().fg(Color::Yellow)),
                chunks[0],
            );
        }
        PoolStatus::Mounted(pool) => {
            frame.render_widget(pool_view(pool), chunks[0]);
        }
        PoolStatus::Error(msg) => {
            frame.render_widget(
                Paragraph::new(format!("Pool error: {msg}")).style(Style::default().fg(Color::Red)),
                chunks[0],
            );
        }
    }

    frame.render_widget(disk_list(model), chunks[2]);

    frame.render_widget(
        Paragraph::new("press q to quit").style(Style::default().fg(Color::DarkGray)),
        chunks[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

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
        let model = Model::new_for_test(sample_disk_keys(), PoolStatus::Loading);
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_not_mounted() {
        let model = Model::new_for_test(sample_disk_keys(), PoolStatus::NotMounted);
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_with_pool() {
        let pool = PoolState {
            mount_point: "/mnt/storage".to_owned(),
            profile: "RAID1".to_owned(),
            health: "healthy".to_owned(),
            used: 2_308_094_370_816,  // ~2.1 TiB
            total: 5_937_955_045_376, // ~5.4 TiB
        };
        let model = Model::new_for_test(sample_disk_keys(), PoolStatus::Mounted(pool));
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_error() {
        let model = Model::new_for_test(
            sample_disk_keys(),
            PoolStatus::Error("command failed: findmnt exited 1".to_owned()),
        );
        let terminal = render(&model, 60, 20);
        snap!(buffer_to_string(&terminal));
    }
}
