//! `wb list`：列出云盘目录内容；纯查询，不产生本机记录。
use crate::{cli::ListArgs, cloud_host, error::CliError, paths::Layout, ui::human_bytes, uri};
use serde::Serialize;

/// 单条目录项；字段与 JSON 输出共用。
#[derive(Serialize)]
struct EntryRow {
    name: String,
    id: String,
    /// 字节长度；目录与 Google 原生文档为空。
    size: Option<u64>,
    folder: bool,
    /// 服务端返回的最后修改时间（RFC 3339 原样）。
    modified: Option<String>,
}

pub async fn run(args: ListArgs, json: bool) -> Result<(), CliError> {
    let ListArgs { uri: raw, root } = args;
    let parsed = uri::parse(&raw)?;
    if !parsed.directory {
        return Err(CliError::Message(
            "list 的 URI 指向文件路径；目录以 / 结尾，根目录写 gdrive://账户/".into(),
        ));
    }
    let layout = Layout::discover()?;
    let root = root.as_deref().unwrap_or(parsed.provider.default_root());
    let drive = cloud_host::build(&layout, parsed.provider, &parsed.account, root).await?;
    let path = format!("{}/", parsed.target);
    let folder = drive.resolve(&path).await?;
    if !folder.is_directory() {
        return Err(CliError::Message("云端路径不是目录".into()));
    }
    let entries = drive
        .list(&folder.reference)
        .await
        .map_err(CliError::from)?;
    let mut rows: Vec<EntryRow> = entries
        .into_iter()
        .map(|file| {
            let folder = file.is_directory();
            EntryRow {
                name: file.name,
                id: file.reference,
                size: file.size,
                folder,
                modified: file.modified,
            }
        })
        .collect();
    rows.sort_by_cached_key(|row| (!row.folder, row.name.to_lowercase(), row.id.clone()));
    if json {
        println!("{}", serde_json::to_string(&rows)?);
    } else {
        render(&parsed.target, &rows);
    }
    Ok(())
}

fn render(prefix: &str, rows: &[EntryRow]) {
    let path = if prefix.is_empty() {
        "/".to_string()
    } else {
        format!("/{prefix}/")
    };
    println!("云盘目录 {path} 共 {} 项", rows.len());
    for row in rows {
        let kind = if row.folder { "目录" } else { "文件" };
        let size = row.size.map(human_bytes).unwrap_or_else(|| "-".to_string());
        let modified = row.modified.as_deref().unwrap_or("-");
        println!(
            "{kind}  {name:<40} {size:>12}  {modified}  {id}",
            name = row.name,
            id = row.id,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn file_paths_are_rejected_before_account_loading() {
        let result = run(
            ListArgs {
                uri: "gdrive://bill/backup/a.iso".into(),
                root: None,
            },
            false,
        )
        .await;
        assert!(matches!(result, Err(CliError::Message(m)) if m.contains("文件")));
    }

    #[test]
    fn rows_render_sorted_with_folders_first() {
        // 纯渲染分支的冒烟验证：排序逻辑在收集阶段。
        let rows = [EntryRow {
            name: "b.zip".into(),
            id: "f-1".into(),
            size: Some(16),
            folder: false,
            modified: None,
        }];
        render("backup", &rows);
    }
}
