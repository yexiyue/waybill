//! WebDAV 登录和私有连接设置；不把密码放在盘配置或命令行参数中。
use crate::{cli::WebdavAuth, credentials::save_private, error::CliError, paths::Layout, uri};
use serde::{Deserialize, Serialize};
use std::{
    io::{IsTerminal, Read},
    sync::Arc,
};
use waybill::service::Service;
use waybill_service_webdav::{
    Webdav, WebdavConfig,
    credential::{Authentication, Credentials},
};

#[derive(Serialize, Deserialize)]
struct Account {
    endpoint: String,
    username: String,
    password: String,
    auth: WebdavAuth,
}
impl Account {
    fn service(&self) -> Result<Webdav, CliError> {
        let authentication = match self.auth {
            WebdavAuth::Basic => Authentication::Basic,
            WebdavAuth::Digest => Authentication::Digest,
            WebdavAuth::Anonymous => Authentication::Anonymous,
        };
        Ok(Webdav::new(
            WebdavConfig::new(
                self.endpoint.clone(),
                if self.username.is_empty() {
                    "anonymous".into()
                } else {
                    self.username.clone()
                },
            ),
            Arc::new(Credentials {
                authentication,
                username: self.username.clone(),
                password: self.password.clone(),
            }),
        )?)
    }
}
pub(crate) fn build(layout: &Layout, account: &str, root: &str) -> Result<Webdav, CliError> {
    let path = layout.webdav_account(account).join("credentials.json");
    let mut bytes = Vec::new();
    let file = std::fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CliError::Message(format!(
                "WebDAV 账户 {account} 尚未登录；运行 wb login --account {account} webdav --endpoint <URL> --username <USER>"
            ))
        } else {
            error.into()
        }
    })?;
    file.take(65537).read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err(CliError::Message("WebDAV 账户设置超过大小上限".into()));
    }
    let account: Account = serde_json::from_slice(&bytes)
        .map_err(|_| CliError::Message("WebDAV 账户设置无效；请重新登录".into()))?;
    let root = if root == "/" {
        "/".into()
    } else {
        format!("{}/", root.trim_matches('/'))
    };
    Ok(account.service()?.at_root(&root)?)
}
pub(crate) async fn login(
    endpoint: String,
    username: Option<String>,
    auth: WebdavAuth,
    password_stdin: bool,
    alias: Option<String>,
    json: bool,
) -> Result<(), CliError> {
    let anonymous = matches!(auth, WebdavAuth::Anonymous);
    if anonymous && (username.is_some() || password_stdin) {
        return Err(CliError::Message("匿名认证不接受用户名或密码输入".into()));
    }
    let username = if anonymous {
        String::new()
    } else {
        username.ok_or_else(|| CliError::Message("WebDAV Basic / Digest 需要 --username".into()))?
    };
    let alias = alias.unwrap_or_else(|| {
        if anonymous {
            "anonymous".into()
        } else {
            username.clone()
        }
    });
    if !uri::safe_account(&alias) {
        return Err(CliError::Message(
            "账户别名不能用作目录名；用 --account 指定别名".into(),
        ));
    }
    let password = if anonymous {
        String::new()
    } else if password_stdin {
        let mut bytes = Vec::new();
        std::io::stdin().take(16385).read_to_end(&mut bytes)?;
        if bytes.len() > 16384 {
            return Err(CliError::Message("密码输入超过大小上限".into()));
        }
        let value =
            String::from_utf8(bytes).map_err(|_| CliError::Message("密码必须是 UTF-8".into()))?;
        let value = value.strip_suffix('\n').unwrap_or(&value);
        value.strip_suffix('\r').unwrap_or(value).to_string()
    } else {
        if !std::io::stdin().is_terminal() {
            return Err(CliError::Message("非终端登录需要 --password-stdin".into()));
        }
        rpassword::prompt_password("WebDAV 密码：")?
    };
    if password.len() > 16384 {
        return Err(CliError::Message("密码输入超过大小上限".into()));
    }
    let account = Account {
        endpoint,
        username,
        password,
        auth,
    };
    let service = account.service()?;
    if !service.resolve("/").await?.is_directory() {
        return Err(CliError::Message("WebDAV endpoint 必须指向目录".into()));
    }
    save_private(
        &Layout::discover()?
            .webdav_account(&alias)
            .join("credentials.json"),
        &account,
    )?;
    if json {
        println!(
            "{}",
            serde_json::json!({"provider":"webdav","account":alias})
        );
    } else {
        println!(
            "已登录 webdav：{alias}\n配置盘：wb drive add <NAME> --provider webdav --account {alias}"
        );
    }
    Ok(())
}
