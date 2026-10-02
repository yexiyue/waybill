//! 参数补全与盘选择：完整参数走脚本路径，缺参时交给文件选择器。
use super::{
    browse::{self, CloudMode},
    get, list, put,
};
use crate::{
    cli::{DriveCommand, GetInput, ListArgs, ListInput, PutArgs, PutInput},
    cloud_host,
    drives::{self, Drive, Drives},
    error::CliError,
    paths::Layout,
    ui::picker::{self, Action, Row},
    uri::{self, DriveUri},
};

pub(super) async fn drive(
    ui: &mut crate::ui::Session,
    command: DriveCommand,
    json: bool,
) -> Result<(), CliError> {
    let layout = Layout::discover()?;
    let mut config = Drives::load(&layout)?;
    match command {
        DriveCommand::Add {
            name,
            provider,
            account,
            root,
            default,
        } => {
            let drive = Drive {
                provider,
                account,
                root: root.unwrap_or_else(|| provider.default_root().into()),
            };
            drives::validate(&name, &drive)?;
            config.drives.insert(name.clone(), drive);
            if default || config.default.is_none() {
                config.default = Some(name);
            }
            config.save(&layout)?;
        }
        DriveCommand::Use { name } => {
            config.select(Some(&name))?;
            config.default = Some(name);
            config.save(&layout)?;
        }
        DriveCommand::Root { name, root } => {
            let mut drive = config.select(Some(&name))?;
            drive.root = match root {
                Some(root) => root,
                None => {
                    picker::require_interactive(json, false)?;
                    let service = cloud_host::build(
                        &layout,
                        drive.provider,
                        &drive.account,
                        drive.provider.default_root(),
                    )
                    .await?;
                    browse::cloud(ui, service.as_ref(), "", CloudMode::Directory)
                        .await?
                        .folder
                }
            };
            drives::validate(&name, &drive)?;
            config.drives.insert(name, drive);
            config.save(&layout)?;
        }
        DriveCommand::Remove { name } => {
            if config.drives.remove(&name).is_none() {
                return Err(CliError::Message(format!("盘 {name} 不存在")));
            }
            if config.default.as_deref() == Some(&name) {
                config.default = None;
            }
            config.save(&layout)?;
        }
        DriveCommand::List => {}
    }
    ui.close();
    if json {
        println!("{}", serde_json::to_string(&config)?);
    } else {
        for (name, drive) in config.drives {
            println!(
                "{} {name} · {} · {} · 根 {}",
                if config.default.as_deref() == Some(&name) {
                    "*"
                } else {
                    " "
                },
                drive.provider.name(),
                drive.account,
                drive.root
            );
        }
    }
    Ok(())
}
async fn location(
    ui: &mut crate::ui::Session,
    raw: Option<&str>,
    name: Option<&str>,
    root: Option<&str>,
    interactive: bool,
) -> Result<(DriveUri, String), CliError> {
    if let Some(raw) = raw.filter(|raw| raw.contains("://")) {
        if name.is_some() {
            return Err(CliError::Message("完整 URI 与 --drive 不能同时指定".into()));
        }
        let parsed = uri::parse(raw)?;
        let root = root.unwrap_or(parsed.provider.default_root()).to_string();
        drives::validate(
            "selected",
            &Drive {
                provider: parsed.provider,
                account: parsed.account.clone(),
                root: root.clone(),
            },
        )?;
        return Ok((parsed, root));
    }
    let layout = Layout::discover()?;
    let config = Drives::load(&layout)?;
    let mut selected = match config.select(name) {
        Ok(drive) => drive,
        Err(error) if name.is_some() || !interactive => return Err(error),
        Err(_) => {
            let candidates: Vec<(String, Drive)> = if config.drives.is_empty() {
                cloud_host::accounts(&layout)?
            } else {
                config.drives.into_iter().collect()
            };
            if candidates.is_empty() {
                return Err(CliError::Message(
                    "尚未登录；先运行 wb login gdrive 或 wb login webdav".into(),
                ));
            }
            let rows = candidates
                .iter()
                .map(|(name, drive)| Row {
                    label: format!("{name} · {} · {}", drive.provider.name(), drive.account),
                    selectable: false,
                    marked: false,
                })
                .collect();
            let picked = picker::choose(
                ui,
                "选择盘 / 已登录账户".into(),
                rows,
                "↑↓ 选择 · Enter 确认 · q 取消",
            )
            .await?;
            match picked.action {
                Action::Open(index) => candidates[index].1.clone(),
                _ => return Err(picker::cancelled()),
            }
        }
    };
    if let Some(root) = root {
        selected.root = root.into();
    }
    drives::validate("selected", &selected)?;
    let raw = raw.unwrap_or("");
    let parsed = uri::parse(&format!(
        "{}://{}/{}",
        selected.provider.name(),
        selected.account,
        raw.trim_start_matches('/')
    ))?;
    Ok((parsed, selected.root))
}
pub(super) async fn list(
    ui: &mut crate::ui::Session,
    input: ListInput,
    name: Option<&str>,
    json: bool,
) -> Result<(), CliError> {
    let interactive = input.path.is_none();
    if interactive {
        picker::require_interactive(json, input.no_tui)?;
    }
    let raw = input.path.as_ref().map(|path| {
        if path.ends_with('/') {
            path.clone()
        } else {
            format!("{path}/")
        }
    });
    let (location, root) =
        location(ui, raw.as_deref(), name, input.root.as_deref(), interactive).await?;
    if !interactive {
        return list::run(
            ListArgs {
                uri: location.to_uri(),
                root: Some(root),
            },
            json,
        )
        .await;
    }
    let drive = cloud_host::build(
        &Layout::discover()?,
        location.provider,
        &location.account,
        &root,
    )
    .await?;
    browse::cloud(ui, drive.as_ref(), &location.target, CloudMode::View).await?;
    Ok(())
}
pub(super) async fn put(
    ui: &mut crate::ui::Session,
    mut input: PutInput,
    name: Option<&str>,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
    if input
        .sources
        .last()
        .and_then(|p| p.to_str())
        .is_some_and(|p| p.contains("://"))
    {
        if input.dest.is_some() {
            return Err(CliError::Message("目标 URI 与 --to 不能同时指定".into()));
        }
        input.dest = input
            .sources
            .pop()
            .and_then(|p| p.into_os_string().into_string().ok());
    }
    let interactive = input.sources.is_empty() || input.dest.is_none();
    if interactive {
        picker::require_interactive(json, input.transfer.no_tui)?;
    }
    let (mut location, mut root) = location(
        ui,
        input.dest.as_deref(),
        name,
        input.transfer.root.as_deref(),
        interactive,
    )
    .await?;
    if input.sources.is_empty() {
        input.sources = browse::local(ui, false).await?;
    }
    if input.dest.is_none() {
        let drive = cloud_host::build(
            &Layout::discover()?,
            location.provider,
            &location.account,
            &root,
        )
        .await?;
        let selected = browse::cloud(ui, drive.as_ref(), "", CloudMode::Directory).await?;
        // 人工选择的目标目录作为显式 service 根，支持上传到已有可访问目录。
        root = cloud_host::selected_root(location.provider, &root, &selected.folder);
        location.target.clear();
        location.directory = true;
    }
    put::run(
        ui,
        PutArgs {
            allow_restart: input.allow_restart,
            sources: input.sources,
            dest: location.to_uri(),
            operation: input.transfer.operation,
            conflict: input.transfer.conflict,
            root: Some(root),
            no_tui: input.transfer.no_tui,
        },
        json,
        verbose,
    )
    .await
}
pub(super) async fn get(
    ui: &mut crate::ui::Session,
    mut input: GetInput,
    name: Option<&str>,
    json: bool,
    verbose: u8,
) -> Result<(), CliError> {
    input.dest = input.dest.or(input.into.take());
    let browsing = input
        .source
        .as_deref()
        .is_none_or(|path| path.ends_with('/'));
    let interactive = browsing || input.dest.is_none();
    if interactive {
        picker::require_interactive(json, input.transfer.no_tui)?;
    }
    let (location, root) = location(
        ui,
        input.source.as_deref(),
        name,
        input.transfer.root.as_deref(),
        interactive,
    )
    .await?;
    let dest = match input.dest {
        Some(dest) => get::normalize_destination(&dest)?,
        None => browse::local(ui, true).await?.remove(0),
    };
    if !browsing {
        return get::run(
            ui,
            crate::cli::GetArgs {
                source: location.to_uri(),
                dest,
                operation: input.transfer.operation,
                conflict: input.transfer.conflict,
                root: Some(root),
                no_tui: input.transfer.no_tui,
            },
            json,
            verbose,
        )
        .await;
    }
    if input.transfer.operation.is_some() {
        return Err(CliError::Message("交互多选下载不能指定 --operation".into()));
    }
    let drive = cloud_host::build(
        &Layout::discover()?,
        location.provider,
        &location.account,
        &root,
    )
    .await?;
    let files = browse::cloud(ui, drive.as_ref(), &location.target, CloudMode::Files)
        .await?
        .files;
    get::run_files(
        ui,
        drive.as_ref(),
        files,
        dest,
        input.transfer,
        json,
        verbose,
    )
    .await
}
