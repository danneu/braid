use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};

pub fn view_help(frame: &mut Frame, area: Rect) {
    let lines = vec![
        Line::from(vec![
            Span::styled("q       ", Style::default().fg(Color::Cyan)),
            Span::raw("quit"),
        ]),
        Line::from(vec![
            Span::styled("r       ", Style::default().fg(Color::Cyan)),
            Span::raw("reload pool data"),
        ]),
        Line::from(vec![
            Span::styled("<tab>   ", Style::default().fg(Color::Cyan)),
            Span::raw("next tab"),
        ]),
        Line::from(vec![
            Span::styled("S-<tab> ", Style::default().fg(Color::Cyan)),
            Span::raw("previous tab"),
        ]),
        Line::from(vec![
            Span::styled("j/k     ", Style::default().fg(Color::Cyan)),
            Span::raw("select disk"),
        ]),
        Line::from(vec![
            Span::styled("<enter> ", Style::default().fg(Color::Cyan)),
            Span::raw("disk detail"),
        ]),
        Line::from(vec![
            Span::styled("<esc>   ", Style::default().fg(Color::Cyan)),
            Span::raw("close detail"),
        ]),
        Line::from(vec![
            Span::styled("?       ", Style::default().fg(Color::Cyan)),
            Span::raw("toggle this help"),
        ]),
    ];

    let width = 30u16;
    let height = lines.len() as u16 + 2; // +2 for borders
    let x = area.width.saturating_sub(width) / 2;
    let y = area.height.saturating_sub(height) / 2;
    let popup = Rect::new(x, y, width.min(area.width), height.min(area.height));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Help ")
                .padding(Padding::horizontal(1)),
        ),
        popup,
    );
}
