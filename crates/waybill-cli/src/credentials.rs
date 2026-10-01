//! Google 账户凭证：原子私有存储与唯一刷新所有者。
use crate::{
    error::CliError,
    oauth::{self, GoogleClient},
};
use oauth2::{RefreshToken, TokenResponse};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use waybill::{
    BoxFuture,
    error::{Error, ErrorKind},
};
use waybill_service_gdrive::credential::{AccessToken, TokenProvider};

/// Google desktop.json 中的应用身份；与 token 一起构成本机登录态。
#[derive(Serialize, Deserialize)]
pub(crate) struct App {
    pub client_id: String,
    pub client_secret: String,
}

/// 本机持久化的 token；refresh 单次轮换后立即落盘。
#[derive(Serialize, Deserialize)]
pub(crate) struct Token {
    pub(crate) access: String,
    pub(crate) refresh: Option<String>,
    pub(crate) expires: u64,
    pub(crate) generation: u64,
}

/// 应用与 token 一起替换，重新登录不会产生跨应用的半份凭证。
#[derive(Serialize, Deserialize)]
pub(crate) struct Credentials {
    pub app: App,
    pub token: Token,
}

impl Credentials {
    pub fn load(dir: &Path) -> Result<Self, CliError> {
        match std::fs::read(dir.join("credentials.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // 读取早期原型保存的文件；下一次刷新会原子写入统一记录。
                Ok(Self {
                    app: serde_json::from_slice(&std::fs::read(dir.join("app.json"))?)?,
                    token: serde_json::from_slice(&std::fs::read(dir.join("token.json"))?)?,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<(), CliError> {
        save_private(&dir.join("credentials.json"), self)
    }
}

/// 账户管理器：唯一刷新所有者，驱动只请求有效租约。
pub(crate) struct Host {
    client: GoogleClient,
    http: reqwest::Client,
    credentials: Mutex<Credentials>,
    path: PathBuf,
}

impl Host {
    /// 组装刷新宿主；应用身份与 token 来自同一私有记录。
    pub(crate) fn new(credentials: Credentials, dir: &Path) -> Result<Self, CliError> {
        Ok(Self {
            client: oauth::google_client(&credentials.app)?,
            http: oauth::http_client()?,
            credentials: Mutex::new(credentials),
            path: dir.join("credentials.json"),
        })
    }

    async fn renew(
        &self,
        rejected: Option<&AccessToken>,
        min_validity: Duration,
    ) -> waybill::error::Result<AccessToken> {
        let mut credentials = self.credentials.lock().await;
        let token = &mut credentials.token;
        let stale = rejected.is_some_and(|v| v.generation() == token.generation.to_string());
        if stale || token.expires < now() + min_validity.as_secs() {
            let refresh = token.refresh.clone().ok_or_else(auth_error)?;
            let next = self
                .client
                .exchange_refresh_token(&RefreshToken::new(refresh))
                .request_async(&self.http)
                .await
                .map_err(|_| auth_error())?;
            token.access = next.access_token().secret().clone();
            if let Some(refresh) = next.refresh_token() {
                token.refresh = Some(refresh.secret().clone());
            }
            token.expires = now()
                + next
                    .expires_in()
                    .unwrap_or(Duration::from_secs(3600))
                    .as_secs();
            token.generation = token.generation.checked_add(1).ok_or_else(auth_error)?;
            if let Err(error) = save_private(&self.path, &*credentials) {
                credentials.token.expires = 0;
                return Err(
                    Error::new(ErrorKind::Io, "cannot persist refreshed credentials")
                        .with_source(error),
                );
            }
        }
        Ok(AccessToken::new(
            &credentials.token.access,
            credentials.token.generation.to_string(),
        ))
    }
}

impl TokenProvider for Host {
    fn access_token(&self, min: Duration) -> BoxFuture<'_, AccessToken> {
        Box::pin(self.renew(None, min))
    }
    fn after_rejection<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, AccessToken> {
        Box::pin(self.renew(Some(rejected), Duration::from_secs(90)))
    }
    fn reconnect_required<'a>(&'a self, _: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async { Err(auth_error()) })
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or_default()
}

fn auth_error() -> Error {
    Error::new(
        ErrorKind::Authentication,
        "run `wb login gdrive` to refresh",
    )
}

/// 0700 目录 + 0600 文件 + 原子替换；崩溃后最多留下旧版本。
pub(crate) fn save_private(
    path: &Path,
    value: &impl Serialize,
) -> std::result::Result<(), CliError> {
    let parent = path
        .parent()
        .ok_or_else(|| CliError::Message("missing private directory".into()))?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|e| CliError::Io(e.error))?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_record_is_private_and_replaced_together() {
        let dir = tempfile::tempdir().unwrap();
        let mut credentials = Credentials {
            app: App {
                client_id: "app-a".into(),
                client_secret: "private".into(),
            },
            token: Token {
                access: "private".into(),
                refresh: Some("private".into()),
                expires: 1,
                generation: 1,
            },
        };
        credentials.save(dir.path()).unwrap();
        credentials.app.client_id = "app-b".into();
        credentials.token.generation = 2;
        credentials.save(dir.path()).unwrap();
        let stored = Credentials::load(dir.path()).unwrap();
        assert_eq!(
            (stored.app.client_id.as_str(), stored.token.generation),
            ("app-b", 2)
        );
        assert_eq!(
            std::fs::metadata(dir.path().join("credentials.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}
