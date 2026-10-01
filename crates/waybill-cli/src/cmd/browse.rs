//! 云端与本地目录浏览，选择结果通过真实对象 ID / 本地路径交给传输层。
use crate::{
    error::CliError,
    ui::{
        human_bytes,
        picker::{self, Action, Row},
    },
};
use std::{collections::BTreeMap, path::PathBuf};
use waybill_service_gdrive::{Gdrive, RemoteFile, Resolved};

const SELECTION_LIMIT: usize = 4096;
const DIRECTORY_LIMIT: usize = 10000;

const FILE_HELP: &str =
    "↑↓/j/k 移动 · Enter 进入目录/选择文件 · Space 多选 · c 确认 · ←/Backspace 上级 · q 取消";
const DIR_HELP: &str = "↑↓/j/k 移动 · Enter 进入目录 · c 使用当前目录 · ←/Backspace 上级 · q 取消";
#[derive(Clone, Copy, PartialEq)]
pub enum CloudMode {
    Files,
    Directory,
    View,
}
pub struct CloudSelection {
    pub folder: String,
    pub files: Vec<RemoteFile>,
}
pub async fn cloud(
    drive: &Gdrive,
    start: &str,
    mode: CloudMode,
) -> Result<CloudSelection, CliError> {
    let initial = format!("{}/", start.trim_end_matches('/'));
    let Resolved::Folder { id } = drive.resolve(&initial).await? else {
        return Err(CliError::Message("浏览起点必须是目录".into()));
    };
    let mut stack = vec![(id, start.trim_matches('/').to_string())];
    let mut selected = BTreeMap::new();
    let mut listing: Option<(String, Vec<RemoteFile>)> = None;
    loop {
        if selected.len() > SELECTION_LIMIT {
            return Err(CliError::Message("选择数量超过 4096 项上限".into()));
        }
        let (folder, path) = stack.last().cloned().ok_or_else(picker::cancelled)?;
        if listing.as_ref().is_none_or(|(id, _)| id != &folder) {
            let mut files = drive.list(&folder).await?;
            files.sort_by_cached_key(|file| {
                (!file.folder, file.name.to_lowercase(), file.id.clone())
            });
            listing = Some((folder.clone(), files));
        }
        let files = &listing.as_ref().ok_or_else(picker::cancelled)?.1;
        let rows = files
            .iter()
            .map(|file| Row {
                label: format!(
                    "{} {}  {}",
                    if file.folder { "▸" } else { " " },
                    file.name,
                    file.size
                        .map(human_bytes)
                        .unwrap_or_else(|| if file.folder {
                            String::new()
                        } else {
                            "不可下载".into()
                        })
                ),
                selectable: mode == CloudMode::Files && !file.folder && file.size.is_some(),
                marked: selected.contains_key(&file.id),
            })
            .collect();
        let current_selected = files
            .iter()
            .filter(|file| selected.contains_key(&file.id))
            .count();
        let title = if mode == CloudMode::Files {
            format!(
                "云盘 /{path} · 其他目录已选 {}",
                selected.len().saturating_sub(current_selected)
            )
        } else {
            format!("云盘 /{path}")
        };
        let picked = picker::choose(
            title,
            rows,
            if mode == CloudMode::Files {
                FILE_HELP
            } else {
                DIR_HELP
            },
        )
        .await?;
        for (index, file) in files.iter().enumerate().filter(|(_, f)| !f.folder) {
            if picked.marked.contains(&index) {
                selected.insert(file.id.clone(), file.clone());
            } else {
                selected.remove(&file.id);
            }
        }
        match picked.action {
            Action::Cancel => return Err(picker::cancelled()),
            Action::Parent => {
                if stack.len() > 1 {
                    stack.pop();
                } else if !path.is_empty() {
                    let parent = path
                        .rsplit_once('/')
                        .map(|(parent, _)| parent)
                        .unwrap_or("");
                    let Resolved::Folder { id } = drive.resolve(&format!("{parent}/")).await?
                    else {
                        return Err(CliError::Message("上级目录不可用".into()));
                    };
                    stack[0] = (id, parent.into());
                }
            }
            Action::Confirm => {
                if selected.len() > SELECTION_LIMIT {
                    return Err(CliError::Message("选择数量超过 4096 项上限".into()));
                }
                if mode == CloudMode::Files && selected.is_empty() {
                    continue;
                }
                return Ok(CloudSelection {
                    folder,
                    files: selected.into_values().collect(),
                });
            }
            Action::Open(index) => {
                let file = &files[index];
                if file.folder {
                    // 目录名仅用于显示；导航和上传选择始终绑定对象 ID。
                    let child = if path.is_empty() {
                        file.name.clone()
                    } else {
                        format!("{path}/{}", file.name)
                    };
                    if stack.len() >= 32 {
                        return Err(CliError::Message("目录浏览超过 32 层上限".into()));
                    }
                    stack.push((file.id.clone(), child));
                } else if mode == CloudMode::Files
                    && file.size.is_some()
                    && selected.remove(&file.id).is_none()
                {
                    selected.insert(file.id.clone(), file.clone());
                }
            }
        }
    }
}
pub async fn local(directory_only: bool) -> Result<Vec<PathBuf>, CliError> {
    let mut path = std::env::current_dir()?.canonicalize()?;
    let mut selected = std::collections::BTreeSet::new();
    loop {
        if selected.len() > SELECTION_LIMIT {
            return Err(CliError::Message("选择数量超过 4096 项上限".into()));
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_dir() || (!directory_only && meta.is_file()) {
                if entries.len() >= DIRECTORY_LIMIT {
                    return Err(CliError::Message("本地目录超过 10000 项上限".into()));
                }
                entries.push((entry.path(), meta.is_dir()));
            }
        }
        entries.sort_by_cached_key(|(p, dir)| (!*dir, p.file_name().map(|n| n.to_os_string())));
        let rows = entries
            .iter()
            .map(|(p, dir)| Row {
                label: format!(
                    "{} {}",
                    if *dir { "▸" } else { " " },
                    p.file_name().unwrap_or_default().to_string_lossy()
                ),
                selectable: !directory_only && !dir,
                marked: selected.contains(p),
            })
            .collect();
        let current_selected = entries.iter().filter(|(p, _)| selected.contains(p)).count();
        let title = if directory_only {
            format!("本地 {}", path.display())
        } else {
            format!(
                "本地 {} · 其他目录已选 {}",
                path.display(),
                selected.len().saturating_sub(current_selected)
            )
        };
        let picked = picker::choose(
            title,
            rows,
            if directory_only { DIR_HELP } else { FILE_HELP },
        )
        .await?;
        for (index, (p, _)) in entries.iter().enumerate().filter(|(_, (_, dir))| !dir) {
            if picked.marked.contains(&index) {
                selected.insert(p.clone());
            } else {
                selected.remove(p);
            }
        }
        match picked.action {
            Action::Cancel => return Err(picker::cancelled()),
            Action::Parent => {
                if let Some(parent) = path.parent() {
                    path = parent.to_path_buf();
                }
            }
            Action::Confirm => {
                if selected.len() > SELECTION_LIMIT {
                    return Err(CliError::Message("选择数量超过 4096 项上限".into()));
                }
                if directory_only {
                    return Ok(vec![path]);
                }
                if !selected.is_empty() {
                    return Ok(selected.into_iter().collect());
                }
            }
            Action::Open(index) => {
                let (p, dir) = &entries[index];
                if *dir {
                    path = p.canonicalize()?;
                } else if !selected.remove(p) {
                    selected.insert(p.clone());
                }
            }
        }
    }
}
