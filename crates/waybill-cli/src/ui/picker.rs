//! 文件选择器：ratatui 渲染与 crossterm 输入；网络和目录读取由宿主拥有。
use crate::error::CliError;
use std::collections::BTreeSet;
use {
    crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers},
    ratatui::{
        layout::{Constraint, Layout},
        style::{Color, Style},
        widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    },
};

pub struct Row {
    pub label: String,
    pub selectable: bool,
    pub marked: bool,
}
pub enum Action {
    Open(usize),
    Parent,
    Confirm,
    Cancel,
}
pub struct Pick {
    pub action: Action,
    pub marked: BTreeSet<usize>,
}
pub fn require_interactive(json: bool, no_tui: bool) -> Result<(), CliError> {
    if json || no_tui || !super::interactive_terminal() {
        return Err(CliError::Message(
            "参数不完整；交互需要终端。请指定路径、目标和 --drive（或先配置默认盘）".into(),
        ));
    }
    Ok(())
}
pub async fn choose(
    ui: &mut super::Session,
    title: String,
    rows: Vec<Row>,
    help: &'static str,
) -> Result<Pick, CliError> {
    let mut cursor = 0;
    let mut marked: BTreeSet<usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| r.marked.then_some(i))
        .collect();
    loop {
        let count = if rows.iter().any(|row| row.selectable) {
            format!(" · 当前已选 {}", marked.len())
        } else {
            String::new()
        };
        ui.draw(|frame| {
            let areas =
                Layout::vertical([Constraint::Min(2), Constraint::Length(2)]).split(frame.area());
            let items: Vec<_> = rows
                .iter()
                .enumerate()
                .map(|(i, row)| {
                    let marker = if marked.contains(&i) {
                        "[x]"
                    } else if row.selectable {
                        "[ ]"
                    } else {
                        "   "
                    };
                    ListItem::new(format!("{marker} {}", row.label.escape_debug()))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("{}{count}", title.escape_debug())),
                )
                .highlight_style(Style::default().fg(Color::Cyan))
                .highlight_symbol("▶ ");
            let mut state =
                ListState::default().with_selected((!rows.is_empty()).then_some(cursor));
            frame.render_stateful_widget(list, areas[0], &mut state);
            frame.render_widget(Paragraph::new(help), areas[1]);
        })?;
        let event::Event::Key(key) = ui.next().await? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let action = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                Some(Action::Cancel)
            }
            KeyCode::Char('c') => Some(Action::Confirm),
            KeyCode::Backspace | KeyCode::Left => Some(Action::Parent),
            KeyCode::Down | KeyCode::Char('j') => {
                cursor = (cursor + 1).min(rows.len().saturating_sub(1));
                None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                cursor = cursor.saturating_sub(1);
                None
            }
            KeyCode::Char(' ') if rows.get(cursor).is_some_and(|r| r.selectable) => {
                if !marked.remove(&cursor) {
                    marked.insert(cursor);
                }
                None
            }
            KeyCode::Enter if !rows.is_empty() => Some(Action::Open(cursor)),
            _ => None,
        };
        if let Some(action) = action {
            return Ok(Pick { action, marked });
        }
    }
}
pub fn cancelled() -> CliError {
    CliError::Waybill(waybill::error::Error::new(
        waybill::error::ErrorKind::Paused,
        "selection cancelled",
    ))
}
