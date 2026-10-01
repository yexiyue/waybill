//! Google 桌面 OAuth：PKCE、回环回调与账户识别。
//!
//! 全部属于 CLI 边界（从验收工具 google_probe 提升）：
//! 错误信息保持静态描述，不携带授权码、token 或回调细节。
use crate::credentials::{App, Token, now};
use crate::error::CliError;
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, RedirectUrl, Scope, TokenResponse, TokenUrl,
    basic::BasicClient,
};
use serde::Deserialize;
use std::{path::Path, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
pub(crate) type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;

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
        .add_scope(Scope::new(
            "https://www.googleapis.com/auth/drive.readonly".into(),
        ))
        .set_pkce_challenge(challenge)
        .add_extra_param("access_type", "offline")
        .add_extra_param("prompt", "consent")
        .url();
    if no_browser {
        eprintln!("请在浏览器打开：\n{url}");
    } else {
        // 打开失败不致命：回退为打印 URL，SSH 场景仍可手动复制。
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        if !std::process::Command::new(opener)
            .arg(url.as_str())
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("无法打开浏览器，请手动访问：\n{url}");
        }
    }
    eprintln!("等待本机回调；不会输出授权码或 token。");
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
                    if data.len() > 16384 {
                        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                    }
                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                        return Ok(());
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
            match target.map(|target| parse_callback(target, state.secret())) {
                Some(Callback::Code(code)) => return Ok::<_, CliError>((code, socket)),
                Some(Callback::Denied) => {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 21\r\nConnection: close\r\n\r\nAuthorization denied.").await;
                    return Err(CliError::Message("Google 授权被拒绝；重新运行登录命令重试".into()));
                }
                _ => {}
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
    let account = account_info(http, result.access_token().secret())
        .await
        .ok()
        .and_then(|user| user.email_address);
    let body = b"Authorization received. Return to wb to see the login result.";
    let _ = socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await;
    let _ = socket.write_all(body).await;
    Ok((token, account))
}

enum Callback {
    Code(String),
    Denied,
    Invalid,
}

/// 只有匹配路径和随机 state 的回调才能结束授权等待。
fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let Ok(callback) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Invalid;
    };
    if callback.path() != "/oauth/callback" {
        return Callback::Invalid;
    }
    let pairs: std::collections::HashMap<_, _> = callback.query_pairs().into_owned().collect();
    if pairs
        .get("state")
        .is_none_or(|state| state != expected_state)
    {
        return Callback::Invalid;
    }
    if pairs.contains_key("error") {
        return Callback::Denied;
    }
    match pairs.get("code").filter(|code| !code.is_empty()) {
        Some(code) => Callback::Code(code.clone()),
        None => Callback::Invalid,
    }
}

#[derive(Deserialize)]
struct About {
    user: Option<User>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct User {
    pub email_address: Option<String>,
    pub permission_id: String,
}

/// Drive 用户身份：邮箱用于目录名，稳定 permissionId 用于 service 实例隔离。
pub(crate) async fn account_info(http: &reqwest::Client, secret: &str) -> Result<User, CliError> {
    let response = http
        .get("https://www.googleapis.com/drive/v3/about")
        .query(&[("fields", "user(emailAddress,permissionId)")])
        .bearer_auth(secret)
        .send()
        .await
        .map_err(|_| CliError::Message("无法读取 Google 账户身份；检查网络后重试".into()))?;
    if !response.status().is_success() {
        return Err(CliError::Message(
            "无法读取 Google 账户身份；检查 Drive API 和授权后重试".into(),
        ));
    }
    let about: About = response
        .json()
        .await
        .map_err(|_| CliError::Message("Google 账户身份响应无效".into()))?;
    about
        .user
        .filter(|user| !user.permission_id.is_empty())
        .ok_or_else(|| CliError::Message("Google 账户缺少稳定身份".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callbacks_require_state_and_handle_denial() {
        assert!(
            matches!(parse_callback("/oauth/callback?state=right&code=abc", "right"), Callback::Code(code) if code == "abc")
        );
        assert!(matches!(
            parse_callback("/oauth/callback?state=wrong&code=abc", "right"),
            Callback::Invalid
        ));
        assert!(matches!(
            parse_callback("/oauth/callback?state=right&error=access_denied", "right"),
            Callback::Denied
        ));
        assert!(matches!(
            parse_callback("/favicon.ico?state=right&code=abc", "right"),
            Callback::Invalid
        ));
    }

    #[test]
    fn reads_google_camel_case_email() {
        let about: About = serde_json::from_str(
            r#"{"user":{"emailAddress":"bill@example.com","permissionId":"principal-a"}}"#,
        )
        .unwrap();
        assert_eq!(
            about.user.unwrap().email_address.as_deref(),
            Some("bill@example.com")
        );
    }
}
