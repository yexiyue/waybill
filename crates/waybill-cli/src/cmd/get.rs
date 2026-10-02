//! `wb get`：从云盘取回单个文件；重跑同一命令即续传或回执幂等跳过。
use crate::{
    cli::GetArgs,
    cloud_host,
    cmd::session,
    error::CliError,
    paths::Layout,
    transfer::{DownloadJob, DownloadRunner},
    uri,
};
use std::path::{Path, PathBuf};
use waybill::object::ObjectMetadata;
use waybill::{
    download::DownloadIntent,
    error::{Error, ErrorKind},
    service::Service,
    transfer::TransferEngine,
};
use waybill_service_fs::{FileCheckpointStore, FsService};

/// CLI 本地目标的稳定实例命名空间；重跑必须命中同一实例。
const LOCAL_INSTANCE: &str = "wb-local-v1";

pub async fn run(
    ui: &mut crate::ui::Session,
    args: GetArgs,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
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
    // 稳定绝对路径和意图校验先于账户加载；不存在的父目录同样可规范化。
    let dest = normalize_destination(&dest)?;
    let preflight_target = if dest.is_dir() {
        dest.join("wb-preflight")
    } else {
        dest.clone()
    };
    DownloadIntent {
        operation: operation.clone().unwrap_or_else(|| "wb-preflight".into()),
        target: utf8_path(&preflight_target)?.into(),
        conflict: conflict.into(),
    }
    .validate()?;
    let layout = Layout::discover()?;
    let root = root.as_deref().unwrap_or(source.provider.default_root());
    let drive = cloud_host::build(&layout, source.provider, &source.account, root).await?;
    // 解析云盘路径；目录、同名多义与缺失都在下载前拒绝。
    let resolved = drive
        .resolve(&source.target)
        .await
        .map_err(CliError::from)?;
    if resolved.is_directory() {
        return Err(CliError::Message(
            "云端路径是目录；使用 wb list 浏览".into(),
        ));
    }
    let file = resolved;
    run_files(
        ui,
        drive.as_ref(),
        vec![file],
        dest,
        crate::cli::TransferArgs {
            operation,
            conflict,
            root: None,
            no_tui,
        },
        json,
        verbose,
    )
    .await
}

pub(super) async fn run_files(
    ui: &mut crate::ui::Session,
    drive: &dyn Service,
    files: Vec<ObjectMetadata>,
    dest: PathBuf,
    options: crate::cli::TransferArgs,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
    if files.is_empty() {
        return Err(CliError::Message("没有选择文件".into()));
    }
    if files.len() > 1 && (!dest.is_dir() || options.operation.is_some()) {
        return Err(CliError::Message(
            "多文件下载需要现有本地目录，且不能指定 --operation".into(),
        ));
    }
    let dest = normalize_destination(&dest)?;
    let mut jobs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // 全部目标预检完成后才开始传输，避免选择集合中的重名造成部分交付。
    for file in files {
        let target_path = if dest.is_dir() {
            dest.join(sanitize_remote_name(&file.name)?)
        } else {
            dest.clone()
        };
        let target = utf8_path(&target_path)?.to_owned();
        DownloadIntent {
            operation: options
                .operation
                .clone()
                .unwrap_or_else(|| "wb-preflight".into()),
            target: target.clone(),
            conflict: options.conflict.into(),
        }
        .validate()?;
        if !seen.insert(target.clone()) {
            return Err(CliError::Message(format!(
                "所选文件存在同名本地目标：{target}"
            )));
        }
        jobs.push(DownloadJob {
            source: drive.download_source(&file.reference).await?,
            name: file.name,
            target,
        });
    }
    let local = FsService::new(LOCAL_INSTANCE)?;
    let stop = waybill::transfer::StopToken::default();
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let queued_files = jobs
        .iter()
        .map(|job| (job.name.clone(), job.target.clone()))
        .collect();
    let handle = tokio::spawn(
        DownloadRunner {
            target: local.download_target()?,
            engine: TransferEngine::new(FileCheckpointStore::new(
                Layout::discover()?.checkpoints(),
            )),
            stop: stop.clone(),
            conflict: options.conflict.into(),
            events: tx,
        }
        .run(jobs, options.operation),
    );
    session::run(
        ui,
        handle,
        rx,
        stop,
        session::Settings {
            banner: format!("get · {}", dest.display()),
            queued_files,
            json,
            verbose: verbose > 0,
            no_tui: options.no_tui,
        },
    )
    .await
}

fn utf8_path(path: &Path) -> Result<&str, CliError> {
    path.to_str()
        .ok_or_else(|| CliError::Message("本地路径必须是有效 UTF-8".into()))
}

pub(super) fn normalize_destination(path: &Path) -> Result<PathBuf, CliError> {
    let raw = utf8_path(path)?;
    if raw.is_empty() || (raw.ends_with('/') && !path.is_dir()) {
        return Err(CliError::Message(
            "以 / 结尾的本地目标必须是已存在目录".into(),
        ));
    }
    soft_canonicalize::soft_canonicalize(path)
        .map_err(|error| CliError::Message(format!("无法解析本地目标：{error}")))
}

/// 目录展开只能使用单个安全文件名，禁止云端名称改变目标路径。
fn sanitize_remote_name(name: &str) -> Result<String, CliError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
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
        let result = run(
            &mut crate::ui::Session::default(),
            args("gdrive://bill", "./a.iso"),
            false,
            0,
        )
        .await;
        assert!(
            matches!(result, Err(CliError::Waybill(error)) if error.kind == ErrorKind::InvalidInput)
        );
    }

    #[tokio::test]
    async fn directory_source_and_dest_forms_are_rejected_early() {
        let result = run(
            &mut crate::ui::Session::default(),
            args("gdrive://bill/backup/", "./a.iso"),
            false,
            0,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(m)) if m.contains("目录")));
        let result = run(
            &mut crate::ui::Session::default(),
            args("gdrive://bill/backup/a.iso", "dir/"),
            false,
            0,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(m)) if m.contains("/")));
    }

    #[test]
    fn local_paths_are_absolute_and_resolve_parent_components() {
        let dir = tempfile::tempdir().unwrap();
        let path = normalize_destination(&dir.path().join("missing/../out.bin")).unwrap();
        assert!(path.is_absolute());
        assert_eq!(path, dir.path().canonicalize().unwrap().join("out.bin"));
        assert_eq!(
            normalize_destination(&dir.path().join("./")).unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn invalid_operation_fails_before_account_loading() {
        let mut input = args("gdrive://missing-account/a.bin", "./a.bin");
        input.operation = Some("../invalid".into());
        assert!(
            matches!(run(&mut crate::ui::Session::default(), input, false, 0).await,
            Err(CliError::Waybill(e)) if e.kind == ErrorKind::InvalidInput)
        );
    }

    #[test]
    fn remote_names_are_sanitized_for_local_use() {
        assert_eq!(sanitize_remote_name("a.zip").unwrap(), "a.zip");
        for bad in [
            "",
            ".",
            "..",
            "../escape",
            "/absolute",
            "a/b",
            "a\\b",
            "a\tb",
            &"a".repeat(256),
        ] {
            assert!(sanitize_remote_name(bad).is_err(), "{bad:?}");
        }
    }
}
