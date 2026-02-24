use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Style},
    text::{Line, Text},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

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

    let daemon_line = match &model.daemon_status {
        DaemonStatus::Idle => "daemon: -".to_string(),
        DaemonStatus::Requesting => "daemon: requesting...".to_string(),
        DaemonStatus::Ok => "daemon: pong".to_string(),
        DaemonStatus::Error(e) => format!("daemon: error: {e}"),
    };

    let content = Text::from(vec![
        Line::from("braid"),
        Line::from(format!("tick: {}", model.tick_count)),
        Line::from(daemon_line),
    ]);

    let footer = Line::from("press q to quit | d debug | p ping");

    let inner = main_block.inner(chunks[0]);
    frame.render_widget(main_block, chunks[0]);

    // Center content vertically in the inner area
    let content_height = content.height() as u16;
    let vertical_pad = inner.height.saturating_sub(content_height + 1) / 2;

    let inner_chunks = Layout::vertical([
        Constraint::Length(vertical_pad),
        Constraint::Length(content_height),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(inner);

    let centered_content = Paragraph::new(content)
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(centered_content, inner_chunks[1]);

    let footer_widget = Paragraph::new(footer)
        .alignment(ratatui::layout::Alignment::Center);
    frame.render_widget(footer_widget, inner_chunks[3]);

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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    fn render(model: &Model, width: u16, height: u16) -> Terminal<TestBackend> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view(model, frame))
            .unwrap();
        terminal
    }

    #[test]
    fn snapshot_default() {
        let model = Model::default();
        let terminal = render(&model, 60, 16);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_with_ticks() {
        let model = Model {
            tick_count: 42,
            ..Model::default()
        };
        let terminal = render(&model, 60, 16);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_debug_panel() {
        let model = Model {
            show_debug: true,
            ..Model::default()
        };
        let terminal = render(&model, 60, 16);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_daemon_pong() {
        let model = Model {
            daemon_status: DaemonStatus::Ok,
            ..Model::default()
        };
        let terminal = render(&model, 60, 16);
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn snapshot_daemon_error() {
        let model = Model {
            daemon_status: DaemonStatus::Error("connection refused".to_string()),
            ..Model::default()
        };
        let terminal = render(&model, 60, 16);
        insta::assert_snapshot!(terminal.backend());
    }
}
