//! `wb get`：从云盘取回单个文件；重跑同一命令即续传或回执幂等跳过。
use crate::{
    cli::GetArgs,
    cmd::put::{install_ctrl_c, paused},
    error::CliError,
    gdrive_host,
    paths::Layout,
    transfer::{DownloadJob, DownloadRunner},
    ui, uri,
};
use std::{io::IsTerminal, sync::Arc};
use waybill::{
    budget::ResourceBudget,
    download::{DownloadEngine, DownloadIntent},
    error::{Error, ErrorKind},
    service::Service,
};
use waybill_service_fs::{FileCheckpointStore, FsService};
use waybill_service_gdrive::Resolved;

/// CLI 本地目标的稳定实例命名空间；重跑必须命中同一实例。
const LOCAL_INSTANCE: &str = "wb-local-v1";

pub async fn run(args: GetArgs, json: bool, verbose: u8) -> Result<(), CliError> {
    let GetArgs {
        source,
        dest,
        operation,
        conflict,
        root,
        no_tui,
    } = args;
    let source = uri::parse(&source)?;
    if source.directory {
        return Err(CliError::Message(
            "get 的源 URI 指向目录；去掉结尾 / 指向文件，或使用 wb list 浏览".into(),
        ));
    }
    // 预检先于凭证加载：URI 与本地目标在登录检查前给出明确错误。
    if dest.to_string_lossy().ends_with('/') {
        return Err(CliError::Message(
            "get 的本地目标不能以 / 结尾；目录会自动展开为 远端文件名".into(),
        ));
    }
    let layout = Layout::discover()?;
    let root = root.as_deref().unwrap_or("root");
    let drive = gdrive_host::build(&layout, &source.account, root).await?;
    // 解析云盘路径；目录、同名多义与缺失都在下载前拒绝。
    let resolved = drive
        .resolve(&source.target)
        .await
        .map_err(CliError::from)?;
    let file = match resolved {
        Resolved::File(file) => file,
        Resolved::Folder { .. } => {
            return Err(CliError::Message(format!(
                "云端路径 {} 是目录；wb list 可浏览内容",
                source.target
            )));
        }
    };
    let name = sanitize_remote_name(&file.name)?;
    // 本地目标：已存在目录时展开远端文件名，否则按给出的路径。
    let target_path = if dest.is_dir() {
        dest.join(&name)
    } else {
        dest.clone()
    };
    let target = target_path.to_string_lossy().into_owned();
    DownloadIntent {
        operation: operation.clone().unwrap_or_else(|| "wb-preflight".into()),
        target: target.clone(),
        conflict: conflict.into(),
    }
    .validate()?;
    let download_source = drive.download_source(&file.id).await?;
    let local = FsService::new(LOCAL_INSTANCE)?;
    let target_sink = local.download_target()?;
    let store = FileCheckpointStore::new(layout.checkpoints());
    let engine = DownloadEngine::new(Arc::new(ResourceBudget::default()));
    let stop = waybill::upload::StopToken::default();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let handle = tokio::spawn(
        DownloadRunner {
            source: download_source,
            target: target_sink,
            store,
            engine,
            stop: stop.clone(),
            conflict: conflict.into(),
            events: tx,
        }
        .run(
            DownloadJob {
                reference: file.id.clone(),
                name,
                target: target.clone(),
            },
            operation,
        ),
    );
    let abort = handle.abort_handle();
    let interactive =
        !json && !no_tui && std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    let rendered = if json {
        install_ctrl_c(stop.clone(), abort.clone());
        ui::json::run(rx).await
    } else if interactive {
        // 面板接管按键与 Ctrl-C；非 TTY 或 --no-tui 走行式输出。
        ui::tui::run(ui::tui::Handoff {
            receiver: rx,
            stop: stop.clone(),
            banner: format!("get · {} · {} → {}", source.account, source.target, target),
            queued_files: vec![(file.name.clone(), target.clone())],
        })
        .await
        .map(|forced| {
            if forced {
                abort.abort();
            }
        })
    } else {
        install_ctrl_c(stop.clone(), abort.clone());
        ui::plain::run(rx, 1, verbose > 0).await
    };
    if rendered.is_err() {
        // 输出通道断裂：先请求停止再收割任务，避免孤儿传输。
        stop.stop();
    }
    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(error) if error.is_cancelled() => return Err(paused()),
        Err(_) => return Err(CliError::Message("传输任务异常退出".into())),
    };
    rendered?;
    if outcome.stopped {
        return Err(paused());
    }
    if outcome.failures > 0 {
        return Err(CliError::Message(
            "取回失败；重跑同一命令可续传或幂等跳过".into(),
        ));
    }
    Ok(())
}

/// 远端文件名落到本地：Drive 名不含 `/`，仍拒绝路径分量与控制字符。
fn sanitize_remote_name(name: &str) -> Result<String, CliError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.chars().any(|c| c.is_control())
        || name.len() > 255
    {
        return Err(CliError::Waybill(Error::new(
            ErrorKind::InvalidInput,
            "remote file name is not a safe local name",
        )));
    }
    Ok(name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Conflict;

    fn args(source: &str, dest: &str) -> GetArgs {
        GetArgs {
            source: source.into(),
            dest: dest.into(),
            operation: None,
            conflict: Conflict::Reject,
            root: None,
            no_tui: true,
        }
    }

    #[tokio::test]
    async fn invalid_source_uri_fails_before_account_loading() {
        let result = run(args("gdrive://bill", "./a.iso"), false, 0).await;
        assert!(
            matches!(result, Err(CliError::Waybill(error)) if error.kind == ErrorKind::InvalidInput)
        );
    }

    #[tokio::test]
    async fn directory_source_and_dest_forms_are_rejected_early() {
        let result = run(args("gdrive://bill/backup/", "./a.iso"), false, 0).await;
        assert!(matches!(result, Err(CliError::Message(m)) if m.contains("目录")));
        let result = run(args("gdrive://bill/backup/a.iso", "dir/"), false, 0).await;
        assert!(matches!(result, Err(CliError::Message(m)) if m.contains("/")));
    }

    #[test]
    fn remote_names_are_sanitized_for_local_use() {
        assert_eq!(sanitize_remote_name("a.zip").unwrap(), "a.zip");
        for bad in ["", ".", "..", "a\tb", &"a".repeat(256)] {
            assert!(sanitize_remote_name(bad).is_err(), "{bad:?}");
        }
    }
}
