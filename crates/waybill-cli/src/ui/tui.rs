//! 全屏运单面板（ratatui-kit）。
//!
//! 传输任务由 put 在 tokio 上独立驱动；这里只消费事件流——
//! 事件写 State 自动唤醒渲染，按键经 StopToken 请求优雅停止。
//! 组件每帧重建，非 Clone 的 receiver 只能经进程级 Atom 一次性交接。
use crate::transfer::Event as TransferEvent;
use ratatui_kit::{
    crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers},
    prelude::*,
    ratatui::{
        layout::{Constraint, Direction},
        style::{Color, Style, Stylize},
        text::{Line, Span},
    },
};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedReceiver;
use waybill::upload::StopToken;

use crate::error::CliError;

/// 交接给面板的一次性资源。
pub(crate) struct Handoff {
    pub receiver: UnboundedReceiver<TransferEvent>,
    pub stop: StopToken,
    pub banner: String,
    pub total_files: usize,
}

static HANDOFF: Atom<Option<Handoff>> = Atom::new(|| None);

// 品牌参考色的终端适配：文档值 #1E4C4C 在深色终端上作文字不可读，
// 文字与边框用提亮青绿，填充与选中底色保留品牌青绿。
const TEAL: Color = Color::Rgb(30, 76, 76);
const TEAL_LIGHT: Color = Color::Rgb(94, 151, 151);
const PAPER: Color = Color::Rgb(245, 240, 229);
const ORANGE: Color = Color::Rgb(216, 122, 55);
const INK_DIM: Color = Color::Rgb(191, 181, 166);

pub(crate) async fn run(handoff: Handoff) -> Result<(), CliError> {
    HANDOFF.set(Some(handoff));
    element!(App)
        .fullscreen()
        .await
        .map_err(|error| CliError::Message(format!("面板运行失败：{error}")))
}

fn brand_palette() -> Palette {
    let mut palette = Palette::default();
    palette.accent = TEAL;
    palette.selection = TEAL;
    palette.on_accent = PAPER;
    palette.border = TEAL_LIGHT;
    palette.border_active = TEAL_LIGHT;
    palette.warning = ORANGE;
    palette
}

/// 单个文件的展示状态；由事件流驱动。
#[derive(Clone)]
struct RowState {
    name: String,
    target: String,
    operation: String,
    size: u64,
    persisted: u64,
    speed: f64,
    status: Status,
    seen_at: Option<(Instant, u64)>,
}

#[derive(Clone)]
enum Status {
    Waiting,
    Running,
    Delivered { object: String },
    Failed { message: String },
    Paused { message: String },
}

#[component]
fn App(mut hooks: Hooks) -> impl Into<AnyElement<'static>> {
    let mut files = hooks.use_state(Vec::<RowState>::default);
    let mut stop_token = hooks.use_state(|| None::<StopToken>);
    let mut stopping = hooks.use_state(|| false);
    let mut finished = hooks.use_state(|| false);
    let mut banner = hooks.use_state(String::default);
    let mut detail_open = hooks.use_state(|| false);
    let mut help_open = hooks.use_state(|| false);
    let table_state = hooks.use_state(TableState::default);
    let mut exit_now = hooks.use_exit();
    let mut exit_when_settled = hooks.use_exit();

    // Ctrl-C 交给面板：先停引擎，等 Done 落定后再退出。
    {
        let mut system = hooks.use_context_mut::<SystemContext>();
        system.set_auto_quit_on_ctrl_c(false);
    }

    // 事件泵：一次性取走交接资源并驱动全部行状态。
    hooks.use_future(async move {
        let Some(Handoff {
            receiver,
            stop,
            banner: handoff_banner,
            total_files,
        }) = HANDOFF.state().write().take()
        else {
            return;
        };
        banner.set(handoff_banner);
        stop_token.set(Some(stop));
        let placeholder = (0..total_files)
            .map(|_| RowState {
                name: String::new(),
                target: String::new(),
                operation: String::new(),
                size: 0,
                persisted: 0,
                speed: 0.0,
                status: Status::Waiting,
                seen_at: None,
            })
            .collect::<Vec<_>>();
        files.set(placeholder);
        let mut receiver = receiver;
        while let Some(event) = receiver.recv().await {
            match event {
                TransferEvent::Started {
                    index,
                    name,
                    target,
                    size,
                    operation,
                } => {
                    let mut rows = files.write();
                    if let Some(row) = rows.get_mut(index) {
                        row.name = name;
                        row.target = target;
                        row.size = size;
                        row.operation = operation;
                        row.status = Status::Running;
                    }
                }
                TransferEvent::Progress {
                    index, persisted, ..
                } => {
                    let mut rows = files.write();
                    if let Some(row) = rows.get_mut(index) {
                        if let Some((at, previous)) = row.seen_at {
                            let elapsed = at.elapsed().as_secs_f64();
                            if elapsed > 0.0 {
                                row.speed = persisted.saturating_sub(previous) as f64 / elapsed;
                            }
                        }
                        row.seen_at = Some((Instant::now(), persisted));
                        row.persisted = persisted;
                    }
                }
                TransferEvent::Completed { index, receipt } => {
                    let mut rows = files.write();
                    if let Some(row) = rows.get_mut(index) {
                        row.persisted = receipt.size;
                        row.speed = 0.0;
                        row.seen_at = None;
                        row.status = Status::Delivered {
                            object: receipt.object,
                        };
                    }
                }
                TransferEvent::Failed { index, message, .. } => {
                    let mut rows = files.write();
                    if let Some(row) = rows.get_mut(index) {
                        row.status = if message.starts_with("Paused") {
                            Status::Paused { message }
                        } else {
                            Status::Failed { message }
                        };
                    }
                }
                TransferEvent::Done { .. } => {
                    finished.set(true);
                    if stopping.get() {
                        exit_when_settled();
                    }
                }
            }
        }
    });

    hooks.use_event_handler(EventScope::Global, EventPriority::High, move |event| {
        let Event::Key(key) = event else {
            return EventResult::Ignored;
        };
        if key.kind != KeyEventKind::Press {
            return EventResult::Ignored;
        }
        let wants_stop = matches!(
            key.code,
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc
        ) || (key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL));
        if wants_stop {
            if finished.get() {
                exit_now();
                return EventResult::Consumed;
            }
            if stopping.get() {
                // 第二次按下：不再等待对账落盘，立即交还（引擎随进程退出）。
                exit_now();
                return EventResult::Consumed;
            }
            stopping.set(true);
            if let Some(stop) = stop_token.read().as_ref() {
                stop.stop();
            }
            return EventResult::Consumed;
        }
        if matches!(key.code, KeyCode::Char('?')) {
            help_open.set(!help_open.get());
            return EventResult::Consumed;
        }
        if matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) && !files.read().is_empty() {
            detail_open.set(true);
            return EventResult::Consumed;
        }
        EventResult::Ignored
    });

    let rows: Vec<RowState> = files.read().clone();
    type RowRenderer = std::sync::Arc<dyn Fn(&RowState, bool) -> Vec<TableCell> + Send + Sync>;
    let render_row: RowRenderer = std::sync::Arc::new(render_row);
    let delivered = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Delivered { .. }))
        .count();
    let failed = rows
        .iter()
        .filter(|row| matches!(row.status, Status::Failed { .. } | Status::Paused { .. }))
        .count();
    let bytes_done: u64 = rows.iter().map(|row| row.persisted).sum();
    let bytes_total: u64 = rows.iter().map(|row| row.size).sum();

    let overview = format!(
        "已送达 {delivered}/{} · 失败 {failed} · {} / {}",
        rows.len(),
        crate::ui::human_bytes(bytes_done),
        crate::ui::human_bytes(bytes_total),
    );

    let footer = if stopping.get() && !finished.get() {
        Line::from(" 正在停止…等待服务端确认落盘（再按一次立即退出） ")
            .centered()
            .fg(ORANGE)
    } else if finished.get() {
        Line::from(" q 退出 ").centered().fg(INK_DIM)
    } else {
        Line::from(" q/Ctrl-C 停止 · j/k 选择 · d 详情 · ? 键位 ")
            .centered()
            .fg(INK_DIM)
    };

    let detail_message = selected_detail(&rows, table_state.read().selected());
    let banner_text = banner.read().clone();

    element!(
        PaletteProvider(palette: brand_palette()) {
            Border(
                flex_direction: Direction::Vertical,
                top_title: Line::from(format!(" waybill 运单 · {banner_text} ")).bold().fg(TEAL_LIGHT),
                bottom_title: footer,
            ) {
                View(height: Constraint::Length(1)) {
                    Text(text: Line::from(overview).fg(INK_DIM))
                }
                Table<RowState>(
                    width: Constraint::Fill(1),
                    state: table_state,
                    active: true,
                    default_index: Some(0),
                    columns: vec![
                        TableColumn::new("文件", Constraint::Length(30)),
                        TableColumn::new("大小", Constraint::Length(11)).alignment(TableCellAlignment::Right),
                        TableColumn::new("进度", Constraint::Length(20)),
                        TableColumn::new("速度", Constraint::Length(11)).alignment(TableCellAlignment::Right),
                        TableColumn::new("状态", Constraint::Length(8)),
                    ],
                    rows: rows,
                    render_row: Some(render_row),
                    header_style: Style::new().fg(TEAL_LIGHT),
                    highlight_style: Style::new().bg(TEAL).fg(PAPER),
                    on_select: move |_: RowState| { detail_open.set(true); },
                )
            }
            AlertModal(
                open: detail_open.get(),
                width: Constraint::Length(76),
                height: Constraint::Length(10),
                title: Line::from(" 运单详情 "),
                message: detail_message,
                close_hint: Line::from("Enter / Esc 关闭").centered(),
                on_close: move |_: ()| { detail_open.set(false); },
            )
            ShortcutInfoModal(
                open: help_open.get(),
                width: Constraint::Length(64),
                height: Constraint::Length(12),
                title: Line::from(" 键位 "),
                sections: vec![
                    ShortcutInfoSection::new("控制", [
                        ("停止并退出", "q / Esc / Ctrl-C"),
                        ("立即退出（停止中再按一次）", "q"),
                        ("详情", "d / Enter"),
                        ("键位说明", "?"),
                    ]),
                    ShortcutInfoSection::new("导航", [
                        ("上下选择", "j / k / ↑ / ↓"),
                    ]),
                ],
                on_close: move |_: ()| { help_open.set(false); },
            )
        }
    )
}

fn render_row(row: &RowState, _selected: bool) -> Vec<TableCell> {
    let status = match &row.status {
        Status::Waiting => Line::from("等待").fg(INK_DIM),
        Status::Running => Line::from("传输").fg(TEAL_LIGHT),
        Status::Delivered { .. } => Line::from("已送达").fg(TEAL_LIGHT),
        Status::Failed { .. } => Line::from("失败").fg(ORANGE),
        Status::Paused { .. } => Line::from("已暂停").fg(ORANGE),
    };
    let bar = progress_bar(row.persisted, row.size);
    let speed = if row.speed > 0.0 {
        format!("{}/s", crate::ui::human_bytes(row.speed as u64))
    } else {
        String::new()
    };
    vec![
        TableCell::new(row.name.clone()),
        TableCell::new(crate::ui::human_bytes(row.size)).alignment(TableCellAlignment::Right),
        TableCell::new(bar),
        TableCell::new(speed).alignment(TableCellAlignment::Right),
        TableCell::new(status),
    ]
}

/// 12 格 Unicode 块进度条：确认量用品牌青绿，剩余用羽毛灰。
fn progress_bar(persisted: u64, total: u64) -> Line<'static> {
    const WIDTH: u64 = 12;
    let percent = if total == 0 {
        1.0
    } else {
        persisted as f64 / total as f64
    };
    let filled = ((percent * WIDTH as f64).round() as u64).clamp(0, WIDTH);
    let done = "█".repeat(filled as usize);
    let rest = "░".repeat((WIDTH - filled) as usize);
    Line::from(vec![
        Span::styled(done, Style::new().fg(TEAL)),
        Span::styled(rest, Style::new().fg(INK_DIM)),
        Span::styled(
            format!(" {:>3.0}%", percent * 100.0),
            Style::new().fg(INK_DIM),
        ),
    ])
}

fn selected_detail(rows: &[RowState], selected: Option<usize>) -> String {
    let Some(index) = selected.filter(|index| *index < rows.len()) else {
        return "没有选中任何文件".into();
    };
    let row = &rows[index];
    let status = match &row.status {
        Status::Waiting => "等待中".into(),
        Status::Running => "传输中".into(),
        Status::Delivered { object } => format!("已送达 object={object}"),
        Status::Failed { message } | Status::Paused { message } => message.clone(),
    };
    format!(
        "文件：{}\n目标：{}\n操作：{}\n大小：{}（已确认 {}）\n状态：{}",
        row.name,
        row.target,
        row.operation,
        crate::ui::human_bytes(row.size),
        crate::ui::human_bytes(row.persisted),
        status,
    )
}
