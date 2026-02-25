use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::Model;

const BY_ID_PREFIX: &str = "/dev/disk/by-id/";

fn disk_list(model: &Model) -> Paragraph<'_> {
    let lines: Vec<Line> = std::iter::once(Line::from("Disks"))
        .chain(model.disks.iter().enumerate().map(|(i, disk)| {
            let label = disk.0.strip_prefix(BY_ID_PREFIX).unwrap_or(&disk.0);
            Line::from(format!("  {}  {}", i + 1, label))
        }))
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

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    frame.render_widget(disk_list(model), chunks[0]);

    frame.render_widget(
        Paragraph::new("press q to quit").style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ByIdPath;
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

    fn sample_disks() -> Vec<ByIdPath> {
        vec![
            ByIdPath("/dev/disk/by-id/ata-Toshiba_MN07_XXXX".to_owned()),
            ByIdPath("/dev/disk/by-id/ata-Ironwolf_ST12_YYYY".to_owned()),
            ByIdPath("/dev/disk/by-id/ata-WDC_WD120_ZZZZ".to_owned()),
        ]
    }

    #[test]
    fn snapshot_disk_list() {
        let model = Model::new(sample_disks());
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(buffer_to_string(&terminal));
    }
}
