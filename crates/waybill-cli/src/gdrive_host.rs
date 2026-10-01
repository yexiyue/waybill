//! 从本机凭证目录构造 Drive service；凭证细节不跨越模块边界。
use crate::{
    credentials::{Credentials, Host},
    error::CliError,
    oauth,
    paths::Layout,
};
use std::{sync::Arc, time::Duration};
use waybill_service_gdrive::credential::TokenProvider;
use waybill_service_gdrive::{Gdrive, GdriveConfig};

/// 组装 Drive service 与刷新宿主；未登录时给出下一步指引。
pub(crate) async fn build(layout: &Layout, account: &str, root: &str) -> Result<Gdrive, CliError> {
    let dir = layout.gdrive_account(account);
    let credentials = Credentials::load(&dir).map_err(|error| match error {
        CliError::Io(error) if error.kind() == std::io::ErrorKind::NotFound => CliError::Message(
            format!("账户 {account} 尚未登录；先运行 wb login --account '{account}' gdrive"),
        ),
        error => error,
    })?;
    let application = credentials.app.client_id.clone();
    let host = Arc::new(Host::new(credentials, &dir)?);
    let token = host.access_token(Duration::from_secs(90)).await?;
    let principal = oauth::account_info(&oauth::http_client()?, token.secret()).await?;
    let drive = Gdrive::new(
        GdriveConfig {
            account: principal.permission_id,
            oauth_application: application,
            root: root.to_string(),
        },
        host,
    )?;
    Ok(drive)
}
