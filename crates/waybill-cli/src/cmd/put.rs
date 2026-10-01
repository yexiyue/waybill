//! `wb put`：多文件顺序投递；重跑同一命令即续传或回执幂等跳过。
use crate::{
    cli::Conflict,
    error::CliError,
    gdrive_host,
    paths::Layout,
    transfer::{self, Job},
    ui, uri,
};
use std::{collections::HashSet, io::IsTerminal, path::PathBuf, sync::Arc};
use waybill::{
    budget::ResourceBudget,
    error::{Error, ErrorKind},
    service::Service,
    upload::{StopToken, UploadEngine},
};
use waybill_service_fs::FileCheckpointStore;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    sources: Vec<PathBuf>,
    dest: String,
    operation: Option<String>,
    conflict: Conflict,
    root: Option<String>,
    no_tui: bool,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
    let dest = uri::parse(&dest)?;
    let mut jobs = Vec::with_capacity(sources.len());
    let mut seen = HashSet::new();
    for path in sources {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| CliError::Message(format!("源路径没有文件名：{}", path.display())))?;
        if !path.exists() {
            return Err(CliError::Message(format!(
                "源文件不存在：{}",
                path.display()
            )));
        }
        let target = if dest.directory {
            dest.join(&name)?
        } else {
            dest.target.clone()
        };
        if !seen.insert(target.clone()) {
            return Err(CliError::Message(format!("多个源映射到同一目标：{target}")));
        }
        jobs.push(Job { path, name, target });
    }
    if !dest.directory && jobs.len() > 1 {
        return Err(CliError::Message(
            "多源投递要求目标 URI 以 / 结尾（目录前缀）".into(),
        ));
    }
    if operation.is_some() && jobs.len() > 1 {
        return Err(CliError::Message("--operation 仅支持单文件投递".into()));
    }
    let root = root.as_deref().unwrap_or("root");
    let layout = Layout::discover()?;
    let (drive, _host) = gdrive_host::build(&layout, &dest.account, root)?;
    let sink = drive.upload_sink()?;
    let store = FileCheckpointStore::new(layout.checkpoints());
    let engine = UploadEngine::new(Arc::new(ResourceBudget::default()));
    let stop = StopToken::default();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let total_files = jobs.len();
    let handle = tokio::spawn(transfer::run(
        jobs,
        sink,
        store,
        engine,
        stop.clone(),
        conflict.into(),
        operation,
        tx,
    ));

    let interactive =
        !json && !no_tui && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let rendered = if json {
        install_ctrl_c(stop.clone());
        ui::json::run(rx).await
    } else if interactive {
        // 面板接管按键与 Ctrl-C；非 TTY 或 --no-tui 走行式输出。
        ui::tui::run(ui::tui::Handoff {
            receiver: rx,
            stop: stop.clone(),
            banner: format!(
                "gdrive · {} · {}",
                dest.account,
                if dest.directory {
                    format!("{}/", dest.target)
                } else {
                    dest.target.clone()
                }
            ),
            total_files,
        })
        .await
    } else {
        // 行式模式由 Ctrl-C 触发优雅停止。
        install_ctrl_c(stop.clone());
        ui::plain::run(rx, total_files, verbose > 0).await
    };
    if rendered.is_err() {
        // 输出通道断裂：先请求停止再收割任务，避免孤儿传输。
        stop.stop();
    }
    let outcome = handle
        .await
        .map_err(|_| CliError::Message("传输任务异常退出".into()))?;
    rendered?;
    if outcome.failures > 0 {
        if outcome.stopped {
            return Err(CliError::Waybill(Error::new(
                ErrorKind::Paused,
                "stopped; rerun the same command to resume",
            )));
        }
        return Err(CliError::Message(format!(
            "{} 个文件投递失败；重跑同一命令可续传或幂等跳过",
            outcome.failures
        )));
    }
    Ok(())
}

/// 首次 Ctrl-C 优雅停止；再次按下走默认终止。
fn install_ctrl_c(stop: StopToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            stop.stop();
        }
    });
}
