//! 传输面板借用命令会话；事件流与键盘由同一个异步循环驱动。
use crate::{error::CliError, transfer::Event as TransferEvent};
use std::time::Instant;
use tokio::sync::mpsc::Receiver;
use waybill::transfer::StopToken;
use {
    crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind},
    ratatui::{
        layout::{Constraint, Layout},
        style::{Color, Style},
        widgets::{Block, Borders, Paragraph, Row, Table, TableState},
    },
};

#[derive(Default, PartialEq, Debug)]
enum Status {
    #[default]
    Waiting,
    Running,
    Delivered,
    Failed,
    Paused,
}
impl Status {
    fn label(&self) -> &'static str {
        match self {
            Self::Waiting => "等待",
            Self::Running => "传输",
            Self::Delivered => "已送达",
            Self::Failed => "失败",
            Self::Paused => "已暂停",
        }
    }
}
pub(crate) struct Handoff {
    pub receiver: Receiver<TransferEvent>,
    pub stop: StopToken,
    pub banner: String,
    pub queued_files: Vec<(String, String)>,
}
#[derive(Default)]
struct FileRow {
    name: String,
    target: String,
    operation: String,
    total: u64,
    persisted: u64,
    speed: u64,
    status: Status,
    detail: String,
    seen: Option<(Instant, u64)>,
}
#[derive(Default)]
struct ViewState {
    finished: bool,
    stopping: bool,
    detail: bool,
    help: bool,
}
pub(crate) async fn run(ui: &mut super::Session, mut handoff: Handoff) -> Result<bool, CliError> {
    let mut rows: Vec<FileRow> = handoff
        .queued_files
        .into_iter()
        .map(|(name, target)| FileRow {
            name,
            target,
            status: Status::Waiting,
            ..Default::default()
        })
        .collect();
    let mut state = TableState::default().with_selected((!rows.is_empty()).then_some(0));
    let mut view = ViewState::default();
    loop {
        ui.draw(|frame| draw(frame, &handoff.banner, &rows, &mut state, &view))?;
        tokio::select! {
            event = handoff.receiver.recv(), if !view.finished => {
                let Some(event) = event else { return Ok(false) };
                view.finished = update(&mut rows, event);
                if view.finished && view.stopping { return Ok(false); }

            },
            event = ui.next() => {
                if let Event::Key(key) = event?
                    && let Some(forced) = on_key(&mut view, &mut state, rows.len(), key, &handoff.stop) {
                    return Ok(forced);
                }

            }
        }
    }
}
fn on_key(
    view: &mut ViewState,
    state: &mut TableState,
    count: usize,
    key: KeyEvent,
    stop: &StopToken,
) -> Option<bool> {
    if key.kind != KeyEventKind::Press {
        return None;
    }
    if (view.detail || view.help) && matches!(key.code, KeyCode::Enter | KeyCode::Esc) {
        view.detail = false;
        view.help = false;
        return None;
    }
    if super::widgets::exit_key(key) {
        if view.finished {
            return Some(false);
        }
        if view.stopping {
            return Some(true);
        }
        view.stopping = true;
        stop.stop();
        return None;
    }
    if view.detail || view.help {
        return None;
    }
    match key.code {
        KeyCode::Down | KeyCode::Char('j') => state.select(Some(
            (state.selected().unwrap_or(0) + 1).min(count.saturating_sub(1)),
        )),
        KeyCode::Up | KeyCode::Char('k') => {
            state.select(Some(state.selected().unwrap_or(0).saturating_sub(1)))
        }
        KeyCode::Enter | KeyCode::Char('d' | 'D') => view.detail = true,
        KeyCode::Char('?') => view.help = !view.help,
        _ => {}
    }
    None
}
fn update(rows: &mut [FileRow], event: TransferEvent) -> bool {
    match event {
        TransferEvent::Started {
            index,
            name,
            target,
            size,
            operation,
        } => {
            if let Some(row) = rows.get_mut(index) {
                row.name = name;
                row.target = target;
                row.total = size;
                row.operation = operation;
                row.status = Status::Running;
            }
        }
        TransferEvent::Progress {
            index, persisted, ..
        } => {
            if let Some(row) = rows.get_mut(index) {
                let now = Instant::now();
                if let Some((then, previous)) = row.seen {
                    let secs = now.duration_since(then).as_secs_f64();
                    if secs > 0.0 {
                        row.speed = (persisted.saturating_sub(previous) as f64 / secs) as u64;
                    }
                }
                row.seen = Some((now, persisted));
                row.persisted = persisted;
            }
        }
        TransferEvent::Completed { index, receipt } => {
            if let Some(row) = rows.get_mut(index) {
                row.total = receipt.size;
                row.persisted = receipt.size;
                row.status = Status::Delivered;
                row.detail = format!("对象：{}", receipt.object);
                row.speed = 0;
            }
        }
        TransferEvent::Failed {
            index,
            message,
            paused,
            ..
        } => {
            if let Some(row) = rows.get_mut(index) {
                row.status = if paused {
                    Status::Paused
                } else {
                    Status::Failed
                };
                row.detail = message;
                row.speed = 0;
            }
        }
        TransferEvent::Done { .. } => return true,
    }
    false
}
fn progress(row: &FileRow) -> String {
    if row.status == Status::Waiting {
        return "—".into();
    }
    let fraction = if row.total == 0 {
        1.0
    } else {
        (row.persisted as f64 / row.total as f64).clamp(0.0, 1.0)
    };
    let filled = (fraction * 12.0).round() as usize;
    format!(
        "{}{} {:>3.0}%",
        "█".repeat(filled),
        "░".repeat(12 - filled),
        fraction * 100.0
    )
}
// 渲染不持有输入或传输资源，便于用无头终端验证。
fn draw(
    frame: &mut ratatui::Frame,
    banner: &str,
    rows: &[FileRow],
    state: &mut TableState,
    view: &ViewState,
) {
    let areas = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(2),
        Constraint::Length(2),
    ])
    .split(frame.area());
    let done = rows
        .iter()
        .filter(|r| r.status == Status::Delivered)
        .count();
    frame.render_widget(
        Paragraph::new(format!(
            "waybill 运单 · {}\n已送达 {done}/{}",
            banner.escape_debug(),
            rows.len()
        )),
        areas[0],
    );
    let table_rows = rows.iter().map(|r| {
        Row::new(vec![
            r.name.escape_debug().to_string(),
            super::human_bytes(r.total),
            progress(r),
            format!("{}/s", super::human_bytes(r.speed)),
            r.status.label().to_string(),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Fill(2),
            Constraint::Length(11),
            Constraint::Length(25),
            Constraint::Length(13),
            Constraint::Length(8),
        ],
    )
    .header(Row::new(["文件", "大小", "进度", "速度", "状态"]))
    .block(Block::default().borders(Borders::ALL))
    .row_highlight_style(Style::default().bg(Color::Rgb(30, 76, 76)))
    .highlight_symbol("▶ ");
    frame.render_stateful_widget(table, areas[1], state);
    let footer = if view.stopping && !view.finished {
        "正在停止…等待确认落盘；再按 q/Ctrl-C 立即退出"
    } else if view.finished {
        "已结束 · q 退出 · ↑↓/j/k 选择 · Enter/d 详情 · ? 键位"
    } else {
        "q/Ctrl-C 停止 · ↑↓/j/k 选择 · Enter/d 详情 · ? 键位"
    };
    frame.render_widget(Paragraph::new(footer), areas[2]);
    if view.detail || view.help {
        let text = if view.help {
            "↑↓ / j/k：选择\nEnter / d：详情\nq / Esc / Ctrl-C：停止并退出；再次按下强制退出\n详情页 Enter / Esc：关闭".to_string()
        } else {
            rows.get(state.selected().unwrap_or(0))
                .map(|r| {
                    format!(
                        "文件：{}\n目标：{}\n操作：{}\n进度：{} / {}\n状态：{}\n{}",
                        r.name.escape_debug(),
                        r.target.escape_debug(),
                        r.operation.escape_debug(),
                        super::human_bytes(r.persisted),
                        super::human_bytes(r.total),
                        r.status.label(),
                        r.detail.escape_debug()
                    )
                })
                .unwrap_or_default()
        };
        super::widgets::popup(frame, "运单详情 / 键位", text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_progress_samples_do_not_hide_receipt_or_failure() {
        let mut rows = vec![FileRow::default(), FileRow::default()];
        let receipt = waybill::transfer::Receipt {
            operation: "op".into(),
            service: waybill::service::ServiceIdentity {
                service: waybill::service::ServiceId::parse("test:ui").unwrap(),
                instance: "test".into(),
            },
            target: "target".into(),
            object: "object".into(),
            size: 17,
            verified: waybill::content::Verification::Length,
        };
        update(&mut rows, TransferEvent::Completed { index: 0, receipt });
        assert_eq!(
            (rows[0].total, rows[0].persisted, rows[0].status.label()),
            (17, 17, "已送达")
        );
        update(
            &mut rows,
            TransferEvent::Failed {
                index: 1,
                name: "b".into(),
                message: "paused".into(),
                paused: true,
            },
        );
        assert_eq!(rows[1].status.label(), "已暂停");
        assert!(update(
            &mut rows,
            TransferEvent::Done {
                receipts: 1,
                failures: 1,
                stopped: true
            }
        ));
    }
    #[test]
    fn stopping_is_graceful_then_forced_and_modal_escape_only_closes() {
        let mut view = ViewState {
            detail: true,
            ..Default::default()
        };
        let mut table = TableState::default();
        let stop = StopToken::default();
        let escape = KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE);
        assert_eq!(on_key(&mut view, &mut table, 0, escape, &stop), None);
        assert!(!view.detail);
        assert!(!view.stopping);
        assert_eq!(on_key(&mut view, &mut table, 0, escape, &stop), None);
        assert!(view.stopping);
        assert_eq!(on_key(&mut view, &mut table, 0, escape, &stop), Some(true));
        view.finished = true;
        assert_eq!(on_key(&mut view, &mut table, 0, escape, &stop), Some(false));
    }
    #[test]
    fn narrow_terminal_and_empty_queue_render_without_panicking() {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(12, 4)).unwrap();
        terminal
            .draw(|frame| {
                draw(
                    frame,
                    "test",
                    &[],
                    &mut TableState::default(),
                    &ViewState {
                        detail: true,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
    }
}
