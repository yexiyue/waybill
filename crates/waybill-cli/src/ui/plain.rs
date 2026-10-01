//! 行式输出：里程碑行到 stdout；进度以单行 `\r` 刷新写在 stderr。
//!
//! stdout 只承载结果（开始与回执），可安全重定向；进度噪声留给终端。
use crate::{error::CliError, transfer::Event, ui::human_bytes};
use std::io::{BufWriter, IsTerminal, Write};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Receiver;

pub(crate) async fn run(
    mut rx: Receiver<Event>,
    total_files: usize,
    verbose: bool,
) -> Result<(), CliError> {
    let mut stdout = BufWriter::new(std::io::stdout().lock());
    let mut stderr = std::io::stderr();
    let progress_tty = stderr.is_terminal() || verbose;
    // (persisted, 时刻)：当前文件最近一次进度，用于节流与速率。
    let mut last: Option<(u64, Instant)> = None;
    while let Some(event) = rx.recv().await {
        match event {
            Event::Started {
                index,
                name,
                operation,
                size,
                ..
            } => {
                clear_progress(&mut stderr, progress_tty && last.is_some());
                writeln!(
                    stdout,
                    "▶ [{}/{}] {}  {}  op={}",
                    index + 1,
                    total_files,
                    name,
                    human_bytes(size),
                    operation.chars().take(11).collect::<String>()
                )?;
                stdout.flush()?;
                last = None;
            }
            Event::Progress {
                persisted,
                total,
                complete,
                ..
            } => {
                if progress_tty && should_render(last.as_ref(), complete) {
                    let speed = last
                        .and_then(|(at, when)| {
                            persisted
                                .checked_sub(at)
                                .map(|delta| delta as f64 / when.elapsed().as_secs_f64())
                        })
                        .filter(|rate| rate.is_finite())
                        .unwrap_or(0.0);
                    let percent = (persisted * 100).checked_div(total).unwrap_or(100);
                    write!(
                        stderr,
                        "\r  {} / {}  {percent}%  {}/s  ",
                        human_bytes(persisted),
                        human_bytes(total),
                        human_bytes(speed as u64)
                    )?;
                    let _ = stderr.flush();
                    last = Some((persisted, Instant::now()));
                }
            }
            Event::Completed { index, receipt } => {
                clear_progress(&mut stderr, progress_tty && last.is_some());
                writeln!(
                    stdout,
                    "✓ [{}/{}] {}  object={}  size={}",
                    index + 1,
                    total_files,
                    receipt.target,
                    receipt.object,
                    human_bytes(receipt.size)
                )?;
                stdout.flush()?;
                last = None;
            }
            Event::Failed {
                index,
                name,
                message,
                paused,
            } => {
                clear_progress(&mut stderr, progress_tty && last.is_some());
                let marker = if paused { "⏸" } else { "✗" };
                let hint = if paused {
                    "（重跑同一命令继续传输）"
                } else {
                    ""
                };
                writeln!(
                    stdout,
                    "{marker} [{}/{}] {}：{message}{hint}",
                    index + 1,
                    total_files,
                    name
                )?;
                stdout.flush()?;
                last = None;
            }
            Event::Done {
                receipts,
                failures,
                stopped,
            } => {
                let state = if stopped { "已停止" } else { "队列结束" };
                writeln!(stdout, "{state}：运单完成 {receipts}，失败 {failures}")?;
                stdout.flush()?;
            }
        }
    }
    stdout.flush()?;
    Ok(())
}

/// ≥1 秒或首次或完成时刷新，避免行式输出退化成刷屏。
fn should_render(last: Option<&(u64, Instant)>, complete: bool) -> bool {
    complete || last.is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(1))
}

fn clear_progress(stderr: &mut std::io::Stderr, active: bool) {
    if !active {
        return;
    }
    let _ = write!(stderr, "\r{}\r", " ".repeat(72));
    let _ = stderr.flush();
}
