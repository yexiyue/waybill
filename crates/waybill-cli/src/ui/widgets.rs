//! 各页面共用的键盘约定与详情浮层。
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
pub(crate) fn exit_key(key: crossterm::event::KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q' | 'Q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}
pub(crate) fn popup(frame: &mut ratatui::Frame, title: &str, text: String) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(86);
    let height = area.height.saturating_sub(2).min(13);
    let rect = ratatui::layout::Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(text).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .title_bottom("Enter / Esc 关闭"),
        ),
        rect,
    );
}
