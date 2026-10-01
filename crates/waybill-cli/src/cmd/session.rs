//! 终端会话生命周期：输出、停止信号与传输任务收割由同一处拥有。
use crate::{
    error::CliError,
    transfer::{Event, Outcome},
    ui,
};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use waybill::{
    error::{Error, ErrorKind},
    transfer::StopToken,
};

pub(super) struct Settings {
    pub banner: String,
    pub queued_files: Vec<(String, String)>,
    pub json: bool,
    pub verbose: bool,
    pub no_tui: bool,
}

pub(super) async fn run(
    ui: &mut ui::Session,
    handle: JoinHandle<Outcome>,
    receiver: Receiver<Event>,
    stop: StopToken,
    settings: Settings,
) -> Result<(), CliError> {
    let abort = handle.abort_handle();
    let interactive = !settings.json && !settings.no_tui && ui::interactive_terminal();
    let signal = if interactive {
        None
    } else {
        Some(install_ctrl_c(stop.clone(), abort.clone()))
    };
    let rendered = if settings.json {
        ui::json::run(receiver).await
    } else if interactive {
        ui::tui::run(
            ui,
            ui::tui::Handoff {
                receiver,
                stop: stop.clone(),
                banner: settings.banner,
                queued_files: settings.queued_files,
            },
        )
        .await
        .map(|forced| {
            if forced {
                abort.abort();
            }
        })
    } else {
        ui::plain::run(receiver, settings.queued_files.len(), settings.verbose).await
    };
    if rendered.is_err() {
        stop.stop();
    }
    let outcome = handle.await;
    if let Some(signal) = signal {
        signal.abort();
    }
    rendered?;
    match outcome {
        Ok(outcome) => finish(outcome),
        Err(error) if error.is_cancelled() => Err(paused()),
        Err(_) => Err(CliError::Message("传输任务异常退出".into())),
    }
}

fn finish(outcome: Outcome) -> Result<(), CliError> {
    if outcome.stopped {
        return Err(paused());
    }
    if outcome.failures > 0 {
        return Err(CliError::Message(format!(
            "{} 个文件传输失败；重跑同一命令可续传或幂等跳过",
            outcome.failures
        )));
    }
    Ok(())
}

/// 首次 Ctrl-C 优雅停止；再次按下中止任务。会话结束时取消信号监听。
fn install_ctrl_c(stop: StopToken, abort: tokio::task::AbortHandle) -> JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.stop();
            if tokio::signal::ctrl_c().await.is_ok() {
                abort.abort();
            }
        }
    })
}

fn paused() -> CliError {
    CliError::Waybill(Error::new(
        ErrorKind::Paused,
        "stopped; rerun the same command to resume",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exit_status_distinguishes_failure_pause_and_completion() {
        assert!(
            finish(Outcome {
                failures: 0,
                stopped: false
            })
            .is_ok()
        );
        assert!(matches!(
            finish(Outcome {
                failures: 1,
                stopped: false
            }),
            Err(CliError::Message(_))
        ));
        assert!(
            matches!(finish(Outcome { failures: 0, stopped: true }), Err(CliError::Waybill(error)) if error.kind == ErrorKind::Paused)
        );
    }
    #[tokio::test]
    async fn aborted_transfer_returns_pause() {
        let (events, receiver) = tokio::sync::mpsc::channel(1);
        drop(events);
        let handle = tokio::spawn(std::future::pending::<Outcome>());
        handle.abort();
        let result = run(
            &mut ui::Session::default(),
            handle,
            receiver,
            StopToken::default(),
            Settings {
                banner: String::new(),
                queued_files: Vec::new(),
                json: true,
                verbose: false,
                no_tui: true,
            },
        )
        .await;
        assert!(matches!(result, Err(CliError::Waybill(error)) if error.kind == ErrorKind::Paused));
    }
}
