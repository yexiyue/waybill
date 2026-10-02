//! `wb login`：浏览器授权并把应用身份与 token 存入本机私有目录。
use crate::{cli::Provider, credentials::Credentials, error::CliError, oauth, paths::Layout, uri};
use serde_json::json;
use std::path::PathBuf;

pub async fn run(
    provider: Provider,
    client: Option<PathBuf>,
    no_browser: bool,
    account_override: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    if let Provider::Object { config } = &provider {
        if client.is_some() || no_browser {
            return Err(CliError::Message(
                "对象存储配置不使用 Google 客户端或浏览器参数".into(),
            ));
        }
        return crate::object_host::login(config, account_override, json).await;
    }
    if let Provider::Webdav {
        endpoint,
        username,
        auth,
        password_stdin,
    } = provider
    {
        if client.is_some() || no_browser {
            return Err(CliError::Message(
                "WebDAV 登录不使用 Google 客户端或浏览器参数".into(),
            ));
        }
        return crate::webdav_host::login(
            endpoint,
            username,
            auth,
            password_stdin,
            account_override,
            json,
        )
        .await;
    }
    if let Some(account) = &account_override
        && !uri::safe_account(account)
    {
        return Err(CliError::Message("账户名不能用作目录名".into()));
    }
    let client_path = client
        .or_else(|| std::env::var_os("WAYBILL_GDRIVE_CLIENT").map(PathBuf::from))
        .ok_or_else(|| {
            CliError::Message(
                "缺少 Google 应用凭证：--client <desktop.json> 或环境变量 WAYBILL_GDRIVE_CLIENT"
                    .into(),
            )
        })?;
    let app = oauth::load_desktop(&client_path)?;
    let client = oauth::google_client(&app)?;
    let http = oauth::http_client()?;
    let (token, email) = oauth::authorize(client, &http, no_browser).await?;
    let account = account_override
        .or(email)
        .ok_or_else(|| CliError::Message("无法确定账户邮箱；用 --account 指定目录别名".into()))?;
    if !uri::safe_account(&account) {
        return Err(CliError::Message(format!(
            "账户名不能用作目录名：{account}"
        )));
    }
    let dir = Layout::discover()?.gdrive_account(&account);
    Credentials { app, token }.save(&dir)?;
    if json {
        println!("{}", json!({ "provider": "gdrive", "account": account }));
    } else {
        println!("已登录 gdrive：{account}");
        println!("凭证保存在本机私有目录：{}", dir.display());
        println!("开始上传：wb put <文件>；浏览云盘：wb list（终端内交互选择）");
    }
    Ok(())
}
