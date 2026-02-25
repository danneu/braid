use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;

use crate::tui::app::Model;
use crate::tui::state::CmdStatus;

pub fn view(model: &Model, frame: &mut Frame) {
    let area = frame.area();

    let outer = Block::bordered()
        .title(" braid ")
        .border_style(Style::default().fg(Color::Cyan));

    let inner = outer.inner(area);
    frame.render_widget(outer, area);

    let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);

    let content = if model.commands.is_empty() {
        "No commands running.".to_string()
    } else {
        let running = model
            .commands
            .values()
            .filter(|c| matches!(c.status, CmdStatus::Running))
            .count();
        format!("{running} command(s) running")
    };
    frame.render_widget(Paragraph::new(content), chunks[0]);

    frame.render_widget(
        Paragraph::new("press q to quit").style(Style::default().fg(Color::DarkGray)),
        chunks[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::{CmdStatus, CommandState};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use std::collections::VecDeque;

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

    #[test]
    fn snapshot_default() {
        let model = Model::default();
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(buffer_to_string(&terminal));
    }

    #[test]
    fn snapshot_with_running_command() {
        let mut model = Model::default();
        model.commands.insert(
            1,
            CommandState {
                cmd: "braid status".to_string(),
                status: CmdStatus::Running,
                output: VecDeque::new(),
            },
        );
        let terminal = render(&model, 60, 20);
        insta::assert_snapshot!(buffer_to_string(&terminal));
    }
}
