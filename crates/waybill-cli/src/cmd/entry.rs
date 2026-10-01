//! 参数补全与盘选择：完整参数走脚本路径，缺参时交给文件选择器。
use super::{
    browse::{self, CloudMode},
    get, list, put,
};
use crate::{
    cli::{DriveCommand, GetInput, ListArgs, ListInput, PutArgs, PutInput},
    drives::{self, Drive, Drives},
    error::CliError,
    gdrive_host,
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
            account,
            root,
            default,
        } => {
            let drive = Drive { account, root };
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
        DriveCommand::Root { name, id } => {
            let mut drive = config.select(Some(&name))?;
            drive.root = match id {
                Some(id) => id,
                None => {
                    picker::require_interactive(json, false)?;
                    let service = gdrive_host::build(&layout, &drive.account, "root").await?;
                    browse::cloud(ui, &service, "", CloudMode::Directory)
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
                "{} {name} · {} · 根 {}",
                if config.default.as_deref() == Some(&name) {
                    "*"
                } else {
                    " "
                },
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
        let root = root.unwrap_or("root").to_string();
        drives::validate(
            "selected",
            &Drive {
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
                let dir = layout
                    .gdrive_account("placeholder")
                    .parent()
                    .ok_or_else(|| CliError::Message("账户目录无效".into()))?
                    .to_path_buf();
                let mut accounts = Vec::new();
                match std::fs::read_dir(dir) {
                    Ok(entries) => {
                        for entry in entries {
                            let entry = entry?;
                            let Some(account) = entry.file_name().to_str().map(str::to_string)
                            else {
                                continue;
                            };
                            if entry.file_type()?.is_dir() && uri::safe_account(&account) {
                                accounts.push((
                                    account.clone(),
                                    Drive {
                                        account,
                                        root: "root".into(),
                                    },
                                ));
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
                accounts.sort_by(|a, b| a.0.cmp(&b.0));
                accounts
            } else {
                config.drives.into_iter().collect()
            };
            if candidates.is_empty() {
                return Err(CliError::Message(
                    "尚未登录；先运行 wb login gdrive --client <FILE>".into(),
                ));
            }
            let rows = candidates
                .iter()
                .map(|(name, drive)| Row {
                    label: format!("{name} · {}", drive.account),
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
        "gdrive://{}/{}",
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
    let drive = gdrive_host::build(&Layout::discover()?, &location.account, &root).await?;
    browse::cloud(ui, &drive, &location.target, CloudMode::View).await?;
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
        let drive = gdrive_host::build(&Layout::discover()?, &location.account, &root).await?;
        let selected = browse::cloud(ui, &drive, "", CloudMode::Directory).await?;
        // 人工选择的目标目录作为显式 service 根，支持上传到已有可访问目录。
        root = selected.folder;
        location.target.clear();
        location.directory = true;
    }
    put::run(
        ui,
        PutArgs {
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
                source: format!("gdrive://{}/{}", location.account, location.target),
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
    let drive = gdrive_host::build(&Layout::discover()?, &location.account, &root).await?;
    let files = browse::cloud(ui, &drive, &location.target, CloudMode::Files)
        .await?
        .files;
    get::run_files(ui, &drive, files, dest, input.transfer, json, verbose).await
}
