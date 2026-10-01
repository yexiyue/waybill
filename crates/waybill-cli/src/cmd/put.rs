//! `wb put`：多文件顺序投递；重跑同一命令即续传或回执幂等跳过。
use crate::{
    cli::PutArgs,
    cmd::session,
    error::CliError,
    gdrive_host,
    paths::Layout,
    transfer::{UploadJob, UploadRunner},
    uri,
};
use std::collections::HashSet;
use waybill::{
    service::Service,
    transfer::{StopToken, TransferEngine},
    upload::UploadIntent,
};
use waybill_service_fs::FileCheckpointStore;

pub async fn run(
    ui: &mut crate::ui::Session,
    args: PutArgs,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
    let PutArgs {
        sources,
        dest,
        operation,
        conflict,
        root,
        no_tui,
    } = args;
    let dest = uri::parse(&dest)?;
    if !dest.directory && sources.len() > 1 {
        return Err(CliError::Message(
            "多源投递要求目标 URI 以 / 结尾（目录前缀）".into(),
        ));
    }
    if operation.is_some() && sources.len() > 1 {
        return Err(CliError::Message("--operation 仅支持单文件投递".into()));
    }
    let mut jobs = Vec::with_capacity(sources.len());
    let mut seen = HashSet::new();
    for path in sources {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                CliError::Message(format!("源路径没有有效的 UTF-8 文件名：{}", path.display()))
            })?;
        if !path.is_file() {
            return Err(CliError::Message(format!(
                "源路径不是可读的普通文件：{}",
                path.display()
            )));
        }
        let target = if dest.directory {
            dest.join(&name)?
        } else {
            dest.target.clone()
        };
        UploadIntent {
            operation: operation.clone().unwrap_or_else(|| "wb-preflight".into()),
            target: target.clone(),
            conflict: conflict.into(),
        }
        .validate()?;
        if !seen.insert(target.clone()) {
            return Err(CliError::Message(format!("多个源映射到同一目标：{target}")));
        }
        jobs.push(UploadJob { path, name, target });
    }
    let root = root.as_deref().unwrap_or("root");
    let layout = Layout::discover()?;
    let drive = gdrive_host::build(&layout, &dest.account, root).await?;
    let sink = drive.upload_sink()?;
    let store = FileCheckpointStore::new(layout.checkpoints());
    let engine = TransferEngine::new(store);
    let stop = StopToken::default();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let queued_files = jobs
        .iter()
        .map(|job| (job.name.clone(), job.target.clone()))
        .collect();
    let handle = tokio::spawn(
        UploadRunner {
            sink,
            engine,
            stop: stop.clone(),
            conflict: conflict.into(),
            events: tx,
        }
        .run(jobs, operation),
    );
    session::run(
        ui,
        handle,
        rx,
        stop,
        session::Settings {
            banner: format!(
                "gdrive · {} · {}",
                dest.account,
                if dest.directory {
                    format!("{}/", dest.target)
                } else {
                    dest.target.clone()
                }
            ),
            queued_files,
            json,
            verbose: verbose > 0,
            no_tui,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Conflict;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    use waybill::error::ErrorKind;

    fn args(sources: Vec<std::path::PathBuf>, dest: &str) -> PutArgs {
        PutArgs {
            sources,
            dest: dest.into(),
            operation: None,
            conflict: Conflict::Reject,
            root: None,
            no_tui: true,
        }
    }

    #[tokio::test]
    async fn batch_target_error_precedes_source_and_credentials() {
        let result = run(
            &mut crate::ui::Session::default(),
            args(
                vec!["missing-a".into(), "missing-b".into()],
                "gdrive://bill/file",
            ),
            false,
            0,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(message)) if message.contains("以 / 结尾")));
    }

    #[tokio::test]
    async fn invalid_sources_and_intents_fail_before_account_loading() {
        let dir = tempfile::tempdir().unwrap();
        let result = run(
            &mut crate::ui::Session::default(),
            args(vec![dir.path().into()], "gdrive://bill/file"),
            false,
            0,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(message)) if message.contains("普通文件")));
        let path = dir.path().join("source");
        std::fs::write(&path, b"content").unwrap();
        let result = run(
            &mut crate::ui::Session::default(),
            args(vec![path], "gdrive://bill/../file"),
            false,
            0,
        )
        .await;
        assert!(
            matches!(result, Err(CliError::Waybill(error)) if error.kind == ErrorKind::InvalidInput)
        );
        let path = dir.path().join(OsString::from_vec(vec![b'a', 0xff]));
        let result = run(
            &mut crate::ui::Session::default(),
            args(vec![path], "gdrive://bill/"),
            false,
            0,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(message)) if message.contains("UTF-8")));
    }
}
