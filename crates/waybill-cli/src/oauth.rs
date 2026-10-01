//! OAuth 宿主：授权流程、凭证私有存储与刷新。
//!
//! 全部属于 CLI 边界（从验收工具 google_probe 提升）：
//! 错误信息保持静态描述，不携带授权码、token 或回调细节。
use crate::error::CliError;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
    basic::BasicClient,
};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};
use waybill::{
    BoxFuture,
    error::{Error, ErrorKind},
};
use waybill_service_gdrive::credential::{AccessToken, TokenProvider};

type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

/// Google desktop.json 中的应用身份；与 token 一起构成本机登录态。
#[derive(Serialize, Deserialize)]
pub(crate) struct App {
    pub client_id: String,
    pub client_secret: String,
}

/// 本机持久化的 token；refresh 单次轮换后立即落盘。
#[derive(Serialize, Deserialize)]
pub(crate) struct Token {
    access: String,
    refresh: Option<String>,
    expires: u64,
    generation: u64,
}

/// 读取 desktop.json 形式的应用凭证。
pub(crate) fn load_desktop(path: &Path) -> Result<App, CliError> {
    #[derive(Deserialize)]
    struct Desktop {
        installed: App,
    }
    let bytes = std::fs::read(path)?;
    let desktop: Desktop = serde_json::from_slice(&bytes)?;
    Ok(desktop.installed)
}

/// 构造 Google 授权客户端；端点固定为 Google 生产地址。
pub(crate) fn google_client(app: &App) -> Result<GoogleClient, CliError> {
    Ok(BasicClient::new(ClientId::new(app.client_id.clone()))
        .set_client_secret(ClientSecret::new(app.client_secret.clone()))
        .set_auth_type(AuthType::RequestBody)
        .set_auth_uri(AuthUrl::new(
            "https://accounts.google.com/o/oauth2/v2/auth".into(),
        )?)
        .set_token_uri(TokenUrl::new("https://oauth2.googleapis.com/token".into())?))
}

/// 无重定向、带超时的共享 HTTP 客户端；授权与业务请求复用。
pub(crate) fn http_client() -> Result<reqwest::Client, CliError> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// 账户管理器：唯一刷新所有者，驱动只请求有效租约。
pub(crate) struct Host {
    client: GoogleClient,
    http: reqwest::Client,
    token: Mutex<Token>,
    path: std::path::PathBuf,
}

impl Host {
    /// 从本机 token 文件恢复；文件损坏属于需要重新登录的状态。
    pub(crate) fn load(
        client: GoogleClient,
        http: reqwest::Client,
        path: &Path,
    ) -> Result<Self, CliError> {
        let token: Token = serde_json::from_slice(&std::fs::read(path)?)?;
        Ok(Self {
            client,
            http,
            token: Mutex::new(token),
            path: path.to_path_buf(),
        })
    }

    async fn renew(
        &self,
        rejected: Option<&AccessToken>,
        min_validity: Duration,
    ) -> waybill::error::Result<AccessToken> {
        let mut token = self.token.lock().await;
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
            token.generation += 1;
            save_private(&self.path, &*token).map_err(|_| auth_error())?;
        }
        Ok(AccessToken::new(
            &token.access,
            token.generation.to_string(),
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

/// 走一次浏览器授权，返回首版 token 与账户邮箱。
///
/// 邮箱来自 Drive about 端点，用作凭证目录名；拿不到时由调用方要求 --account。
pub(crate) async fn authorize(
    client: GoogleClient,
    http: &reqwest::Client,
    no_browser: bool,
) -> Result<(Token, Option<String>), CliError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let client = client.set_redirect_uri(RedirectUrl::new(format!(
        "http://127.0.0.1:{port}/oauth/callback"
    ))?);
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/drive.file".into(),
        ))
        .set_pkce_challenge(challenge)
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .url();
    if no_browser {
        println!("请在浏览器打开：\n{url}");
    } else {
        // 打开失败不致命：回退为打印 URL，SSH 场景仍可手动复制。
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if std::process::Command::new(opener)
            .arg(url.as_str())
            .status()
            .is_err()
        {
            println!("无法打开浏览器，请手动访问：\n{url}");
        }
    }
    println!("等待本机回调；不会输出授权码或 token。");
    let (code, mut socket) = tokio::time::timeout(Duration::from_secs(300), async {
        loop {
            let (mut socket, _) = listener.accept().await?;
            let mut data = Vec::new();
            let mut buffer = [0u8; 2048];
            let read = tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    let n = socket.read(&mut buffer).await?;
                    if n == 0 {
                        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                    }
                    data.extend_from_slice(&buffer[..n]);
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        return Ok(());
                    }
                    if data.len() > 16384 {
                        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                    }
                }
            })
            .await;
            if !matches!(read, Ok(Ok(()))) {
                continue;
            }
            let request = String::from_utf8_lossy(&data);
            let target = request
                .lines()
                .next()
                .and_then(|l| l.strip_prefix("GET "))
                .and_then(|l| l.split_whitespace().next());
            if let Some(code) = target
                .and_then(|target| {
                    url::Url::parse(&format!("http://127.0.0.1:{port}{target}")).ok()
                })
                .filter(|callback| callback.path() == "/oauth/callback")
                .and_then(|callback| {
                    let pairs: std::collections::HashMap<_, _> =
                        callback.query_pairs().into_owned().collect();
                    let state_ok = pairs.get("state").is_some_and(|s| s == state.secret());
                    state_ok.then(|| pairs.get("code").cloned()).flatten()
                })
            {
                return Ok::<_, std::io::Error>((code, socket));
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    })
    .await
    .map_err(|_| CliError::Message("授权回调等待超时（300 秒）".into()))??;
    let result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(verifier)
        .request_async(http)
        .await
        .map_err(|_| CliError::Message("Google token exchange failed".into()))?;
    let token = Token {
        access: result.access_token().secret().clone(),
        refresh: result.refresh_token().map(|v| v.secret().clone()),
        expires: now()
            + result
                .expires_in()
                .unwrap_or(Duration::from_secs(3600))
                .as_secs(),
        generation: 1,
    };
    let account = account_email(http, result.access_token().secret()).await?;
    let body = b"Authorization complete. You may close this page.";
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    socket.write_all(body).await?;
    Ok((token, account))
}

/// 用刚换到的 access token 查询账户邮箱；失败不阻断授权本身。
async fn account_email(http: &reqwest::Client, secret: &str) -> Result<Option<String>, CliError> {
    #[derive(Deserialize)]
    struct About {
        user: Option<User>,
    }
    #[derive(Deserialize)]
    struct User {
        email_address: Option<String>,
    }
    let response = http
        .get("https://www.googleapis.com/drive/v3/about")
        .query(&[("fields", "user(emailAddress)")])
        .bearer_auth(secret)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            let about: Option<About> = response.json().await.ok();
            Ok(about
                .and_then(|about| about.user)
                .and_then(|user| user.email_address))
        }
        _ => Ok(None),
    }
}

fn now() -> u64 {
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
