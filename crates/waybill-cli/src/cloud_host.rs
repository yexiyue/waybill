//! 宿主唯一的 service 工厂；浏览和队列只依赖公开端口。
use crate::{
    drives::{Drive, ProviderKind},
    error::CliError,
    paths::Layout,
    uri,
};
use std::sync::Arc;
use waybill::service::Service;

pub(crate) async fn build(
    layout: &Layout,
    provider: ProviderKind,
    account: &str,
    root: &str,
) -> Result<Arc<dyn Service>, CliError> {
    match provider {
        ProviderKind::Gdrive => Ok(Arc::new(
            crate::gdrive_host::build(layout, account, root).await?,
        )),
        ProviderKind::Webdav => Ok(Arc::new(crate::webdav_host::build(layout, account, root)?)),
    }
}
/// 浏览结果的引用相对当前根；GDrive 的目录 ID 则是绝对引用。
pub(crate) fn selected_root(provider: ProviderKind, current: &str, selected: &str) -> String {
    if provider == ProviderKind::Gdrive || current == "/" {
        return selected.into();
    }
    if selected == "/" {
        return current.into();
    }
    format!("{}/{selected}", current.trim_end_matches('/'))
}

/// 尚未配置盘时，交互入口也能选择各后端的已登录账户。
pub(crate) fn accounts(layout: &Layout) -> Result<Vec<(String, Drive)>, CliError> {
    let mut accounts = Vec::new();
    for (provider, sample) in [
        (ProviderKind::Gdrive, layout.gdrive_account("placeholder")),
        (ProviderKind::Webdav, layout.webdav_account("placeholder")),
    ] {
        let directory = sample
            .parent()
            .ok_or_else(|| CliError::Message("账户目录无效".into()))?;
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        for entry in entries {
            let entry = entry?;
            let Some(account) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if entry.file_type()?.is_dir() && uri::safe_account(&account) {
                accounts.push((
                    format!("{}/{account}", provider.name()),
                    Drive {
                        provider,
                        account,
                        root: provider.default_root().into(),
                    },
                ));
            }
        }
    }
    accounts.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selected_root_preserves_backend_reference_semantics() {
        assert_eq!(
            selected_root(ProviderKind::Webdav, "parent", "child/"),
            "parent/child/"
        );
        assert_eq!(
            selected_root(ProviderKind::Webdav, "parent/", "child/"),
            "parent/child/"
        );
        assert_eq!(
            selected_root(ProviderKind::Webdav, "parent/", "/"),
            "parent/"
        );
        assert_eq!(selected_root(ProviderKind::Webdav, "/", "child/"), "child/");
        assert_eq!(
            selected_root(ProviderKind::Gdrive, "parent-id", "child-id"),
            "child-id"
        );
    }
}
