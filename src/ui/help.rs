//! `?`: keybinding help overlay.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::app::App;

const BINDINGS: &[(&str, &str)] = &[
    ("q / Ctrl+C", "quit"),
    ("Tab / Shift+Tab", "next / previous view"),
    ("1", "fleet overview (all endpoints)"),
    ("2..9", "endpoint tabs (Tab reaches the rest)"),
    ("h or H", "history charts"),
    ("R", "raw metrics browser (capital R)"),
    ("r", "force refresh now (lowercase r)"),
    ("j/k or ↑/↓", "select / scroll"),
    ("PgUp/PgDn g/G", "page / jump to top / bottom"),
    ("Enter", "open selected endpoint (fleet view)"),
    ("s", "cycle fleet sort column"),
    ("/", "filter raw metrics (Enter apply, Esc clear)"),
    ("e", "cycle endpoint filter (history/raw views)"),
    ("m", "cycle model/engine series (history view)"),
    ("p", "pause display refresh (collection continues)"),
    ("+ / -", "faster / slower refresh interval"),
    ("?", "this help"),
];

pub fn draw(frame: &mut Frame, app: &App, area: Rect) {
    let t = &app.theme;
    let width = 62.min(area.width.saturating_sub(2));
    let height = (BINDINGS.len() as u16 + 4).min(area.height.saturating_sub(2));
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };

    let mut lines: Vec<Line> = vec![Line::default()];
    for (keycap, action) in BINDINGS {
        lines.push(Line::from(vec![
            Span::styled(format!("  {keycap:>16}  "), t.key),
            Span::styled(*action, t.text),
        ]));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled("  press Esc to close", t.dim)));

    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::new()
                .borders(Borders::ALL)
                .border_style(t.heading)
                .title(Span::styled(" vllmtop keys ", t.heading))
                .style(t.bg),
        ),
        popup,
    );
}
