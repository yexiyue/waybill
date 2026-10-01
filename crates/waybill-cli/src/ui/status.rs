//! 本机运单只读页面；记录扫描和错误分类由命令层提供。
use crate::{
    cmd::status::{SessionRow, Unreadable},
    error::CliError,
    ui::human_bytes,
};
pub(crate) async fn run(
    ui: &mut super::Session,
    mut refresh: impl FnMut() -> Result<(Vec<SessionRow>, Vec<Unreadable>), CliError>,
) -> Result<Vec<Unreadable>, CliError> {
    use {
        crossterm::event::{Event, KeyCode, KeyEventKind},
        ratatui::widgets::TableState,
    };
    let (mut rows, mut unreadable) = refresh()?;
    let mut state = TableState::default().with_selected((!rows.is_empty()).then_some(0));
    let mut detail = false;
    let mut diagnostics = false;
    let mut problem = 0;
    loop {
        ui.draw(|frame| {
            draw(
                frame,
                &rows,
                &unreadable,
                &mut state,
                detail,
                diagnostics.then_some(problem),
            )
        })?;

        let Event::Key(key) = ui.next().await? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if (detail || diagnostics) && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
            detail = false;
            diagnostics = false;
            continue;
        }
        if super::widgets::exit_key(key) {
            return Ok(unreadable);
        }
        if diagnostics {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    problem = (problem + 1).min(unreadable.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => problem = problem.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        if detail {
            continue;
        }
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => state.select(Some(
                (state.selected().unwrap_or(0) + 1).min(rows.len().saturating_sub(1)),
            )),
            KeyCode::Up | KeyCode::Char('k') => {
                state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
            }
            KeyCode::Enter | KeyCode::Char('d') => {
                detail = true;
                diagnostics = false;
            }
            KeyCode::Char('e') => {
                diagnostics = true;
                detail = false;
            }
            KeyCode::Char('r') => {
                let selected = state
                    .selected()
                    .and_then(|i| rows.get(i))
                    .map(|r| r.operation.clone());
                (rows, unreadable) = refresh()?;
                problem = 0;
                state.select(
                    selected
                        .and_then(|id| rows.iter().position(|r| r.operation == id))
                        .or_else(|| (!rows.is_empty()).then_some(0)),
                );
            }
            _ => {}
        }
    }
}

fn draw(
    frame: &mut ratatui::Frame,
    rows: &[SessionRow],
    unreadable: &[Unreadable],
    state: &mut ratatui::widgets::TableState,
    detail: bool,
    problem: Option<usize>,
) {
    use ratatui::{
        layout::{Constraint, Layout},
        style::{Color, Style},
        widgets::{Block, Borders, Paragraph, Row, Table},
    };

    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(2),
        Constraint::Length(2),
    ])
    .split(frame.area());
    frame.render_widget(
        Paragraph::new(format!(
            "waybill 本机运单 · {} 条
无法读取 {} 条（按 e 查看）",
            rows.len(),
            unreadable.len()
        )),
        areas[0],
    );
    let items = rows.iter().map(|r| {
        Row::new(vec![
            r.direction.to_string(),
            r.target.escape_debug().to_string(),
            format!("{} / {}", human_bytes(r.acknowledged), human_bytes(r.total)),
            r.state.to_string(),
        ])
    });
    frame.render_stateful_widget(
        Table::new(
            items,
            [
                Constraint::Length(9),
                Constraint::Fill(1),
                Constraint::Length(25),
                Constraint::Length(12),
            ],
        )
        .header(Row::new(["方向", "目标", "已确认 / 总量", "状态"]))
        .block(Block::default().borders(Borders::ALL))
        .row_highlight_style(Style::default().bg(Color::Rgb(30, 76, 76)))
        .highlight_symbol("▶ "),
        areas[1],
        state,
    );
    frame.render_widget(
        Paragraph::new("↑↓/j/k 选择 · Enter/d 详情 · r 刷新 · e 读取问题 · q 退出"),
        areas[2],
    );
    if detail {
        let text=rows.get(state.selected().unwrap_or(0)).map(|r| format!("操作：{}\n方向：{}\n服务：{}\n实例：{}\n目标：{}\n进度：{} / {}\n重启：{}\n状态：{}",r.operation.escape_debug(),r.direction,r.service.escape_debug(),r.instance.escape_debug(),r.target.escape_debug(),human_bytes(r.acknowledged),human_bytes(r.total),r.restarts,r.state)).unwrap_or_else(|| "暂无本机运单".into());
        super::widgets::popup(frame, "运单详情", text);
    } else if let Some(problem) = problem {
        let text = unreadable
            .get(problem)
            .map(|e| {
                format!(
                    "第 {} / {} 条\n文件：{}\n原因：{}\n\n↑↓ / j/k 查看其他记录",
                    problem + 1,
                    unreadable.len(),
                    e.file.escape_debug(),
                    e.reason
                )
            })
            .unwrap_or_else(|| "所有记录均可读取".into());
        super::widgets::popup(frame, "读取问题", text);
    }
}
