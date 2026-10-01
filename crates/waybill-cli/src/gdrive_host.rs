//! 从本机凭证目录构造 Drive service；凭证细节不跨越模块边界。
use crate::{error::CliError, oauth, paths::Layout};
use std::sync::Arc;
use waybill_service_gdrive::{Gdrive, GdriveConfig};

/// 组装 Drive service 与刷新宿主；未登录时给出下一步指引。
pub(crate) fn build(
    layout: &Layout,
    account: &str,
    root: &str,
) -> Result<(Gdrive, Arc<oauth::Host>), CliError> {
    let dir = layout.gdrive_account(account);
    let app_path = dir.join("app.json");
    if !app_path.exists() {
        return Err(CliError::Message(format!(
            "账户 {account} 尚未登录；先运行 wb login gdrive"
        )));
    }
    let app: oauth::App = serde_json::from_slice(&std::fs::read(&app_path)?)?;
    let client = oauth::google_client(&app)?;
    let http = oauth::http_client()?;
    let host = Arc::new(oauth::Host::load(client, http, &dir.join("token.json"))?);
    let drive = Gdrive::new(
        GdriveConfig {
            account: account.to_string(),
            oauth_application: app.client_id,
            root: root.to_string(),
        },
        host.clone(),
    )?;
    Ok((drive, host))
}
