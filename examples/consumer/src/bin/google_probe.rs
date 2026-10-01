//! 宿主侧真实验收工具；OAuth 依赖仅属于消费者，不进入核心或 service。
use oauth2::{
    AuthType, AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointNotSet,
    EndpointSet, PkceCodeChallenge, RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
    basic::BasicClient,
};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Mutex,
};
use waybill::{
    BoxFuture,
    checkpoint::DriverState,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::{Source, SourceIdentity},
    upload::{
        ConflictPolicy, ResourceBudget, RunOptions, SessionStatus, StopToken, UploadEngine,
        UploadIntent, UploadPolicy, UploadSink,
    },
};
use waybill_service_fs::{FileCheckpointStore, FileSource};
use waybill_service_gdrive::{
    Gdrive, GdriveConfig,
    credential::{AccessToken, TokenProvider},
};
type GoogleClient =
    BasicClient<EndpointSet, EndpointNotSet, EndpointNotSet, EndpointNotSet, EndpointSet>;
#[derive(Deserialize)]
struct Desktop {
    installed: Installed,
}
#[derive(Deserialize)]
struct Installed {
    client_id: String,
    client_secret: String,
}
#[derive(Serialize, Deserialize)]
struct Token {
    access: String,
    refresh: Option<String>,
    expires: u64,
    generation: u64,
}
#[derive(Serialize, Deserialize)]
struct Probe {
    root: String,
    operation: String,
}
struct Host {
    client: GoogleClient,
    http: reqwest::Client,
    token: Mutex<Token>,
    path: PathBuf,
}
impl Host {
    async fn renew(
        &self,
        rejected: Option<&AccessToken>,
        min_validity: Duration,
    ) -> Result<AccessToken> {
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
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs())
        .unwrap_or_default()
}
fn auth_error() -> Error {
    Error::new(
        ErrorKind::Authentication,
        "probe host authorization required",
    )
}
fn save_private(
    path: &Path,
    value: &impl Serialize,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let parent = path.parent().ok_or("missing private directory")?;
    std::fs::create_dir_all(parent)?;
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
async fn authorize(
    client: GoogleClient,
    http: &reqwest::Client,
    path: &Path,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
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
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(opener)
        .arg(url.as_str())
        .status()?;
    println!("浏览器授权已打开，等待本机回调；不会输出授权码或 token。");
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
            if let Some(target) = target {
                if let Ok(callback) = url::Url::parse(&format!("http://127.0.0.1:{port}{target}")) {
                    let pairs: std::collections::HashMap<_, _> =
                        callback.query_pairs().into_owned().collect();
                    if callback.path() == "/oauth/callback"
                        && pairs.get("state").is_some_and(|s| s == state.secret())
                    {
                        if let Some(code) = pairs.get("code") {
                            return Ok::<_, std::io::Error>((code.clone(), socket));
                        }
                    }
                }
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        }
    })
    .await??;
    let result = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(verifier)
        .request_async(http)
        .await
        .map_err(|_| "Google token exchange failed")?;
    save_private(
        path,
        &Token {
            access: result.access_token().secret().clone(),
            refresh: result.refresh_token().map(|v| v.secret().clone()),
            expires: now()
                + result
                    .expires_in()
                    .unwrap_or(Duration::from_secs(3600))
                    .as_secs(),
            generation: 1,
        },
    )?;
    let body = b"Authorization complete. You may close this page.";
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\n",body.len()).as_bytes()).await?;
    socket.write_all(body).await?;
    println!("授权完成，凭证已保存到本机私有状态目录。");
    Ok(())
}
#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: google_probe <desktop.json> <private-state-directory> <auth|pause|resume|retry|confirm|crash|resume-crash|lost-response|retry-lost-response|large>".into());
    }
    let desktop: Desktop = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let root = PathBuf::from(&args[2]);
    let token_path = root.join("oauth.json");
    let client = BasicClient::new(ClientId::new(desktop.installed.client_id.clone()))
        .set_client_secret(ClientSecret::new(desktop.installed.client_secret))
        .set_auth_type(AuthType::RequestBody)
        .set_auth_uri(AuthUrl::new(
            "https://accounts.google.com/o/oauth2/v2/auth".into(),
        )?)
        .set_token_uri(TokenUrl::new("https://oauth2.googleapis.com/token".into())?);
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(60))
        .build()?;
    if args[3] == "auth" {
        return authorize(client, &http, &token_path).await;
    }
    if ![
        "pause",
        "resume",
        "retry",
        "confirm",
        "crash",
        "resume-crash",
        "lost-response",
        "retry-lost-response",
        "large",
    ]
    .contains(&args[3].as_str())
    {
        return Err("unknown probe action".into());
    }
    let token: Token = serde_json::from_slice(&std::fs::read(&token_path)?)?;
    let host = Arc::new(Host {
        client,
        http: http.clone(),
        token: Mutex::new(token),
        path: token_path.clone(),
    });
    let config_path = root.join("probe.json");
    let mut config: Probe = if config_path.exists() {
        serde_json::from_slice(&std::fs::read(&config_path)?)?
    } else {
        let token = host.access_token(Duration::from_secs(90)).await?;
        let response = http
            .post("https://www.googleapis.com/drive/v3/files")
            .bearer_auth(token.secret())
            .query(&[("fields", "id")])
            .json(&serde_json::json!({
                "name": format!("waybill-acceptance-{}", now()),
                "mimeType": "application/vnd.google-apps.folder"
            }))
            .send()
            .await
            .map_err(|_| "test folder creation request failed")?;
        if !response.status().is_success() {
            return Err("test folder creation failed".into());
        }
        let body: serde_json::Value = response.json().await?;
        let config = Probe {
            root: body
                .get("id")
                .and_then(|v| v.as_str())
                .ok_or("missing test folder ID")?
                .into(),
            operation: format!("gdrive-probe-{}", now()),
        };
        save_private(&config_path, &config)?;
        config
    };
    let action = args[3].as_str();
    let target = if action == "crash" || action == "resume-crash" {
        config.operation.push_str("-crash");
        "crash-payload.bin"
    } else if action == "lost-response" || action == "retry-lost-response" {
        config.operation.push_str("-lost-response");
        "lost-response.bin"
    } else if action == "large" {
        config.operation.push_str("-large");
        "large-payload.bin"
    } else {
        "payload.bin"
    };
    let path = root.join(if action == "large" {
        "large-payload.bin"
    } else {
        "payload.bin"
    });
    if !path.exists() {
        tokio::fs::create_dir_all(&root).await?;
        let mut file = tokio::fs::File::create(&path).await?;
        let block = vec![0x5a; 256 * 1024];
        let blocks = if action == "large" { 1024 } else { 68 };
        for _ in 0..blocks {
            file.write_all(&block).await?;
        }
        file.sync_all().await?;
    }
    let source = FileSource::open(path).await?;
    let drive = Gdrive::new(
        GdriveConfig {
            account: "probe-account".into(),
            oauth_application: desktop.installed.client_id,
            root: config.root.clone(),
        },
        host,
    )?;
    let store = FileCheckpointStore::new(root.join("checkpoints"));
    let engine = UploadEngine::new(Arc::new(ResourceBudget::default()));
    let stop = StopToken::default();
    let pause = action == "pause";
    let crash = action == "crash";
    let sink: Box<dyn UploadSink> = if action == "lost-response" {
        Box::new(LostCompletion {
            inner: drive.clone(),
            dropped: std::sync::atomic::AtomicBool::new(false),
        })
    } else {
        Box::new(drive.clone())
    };
    let started = std::time::Instant::now();
    let digest = source.identity().await?.blake3;
    let receipt = engine
        .run(
            &source,
            sink.as_ref(),
            &store,
            RunOptions {
                intent: UploadIntent {
                    operation: config.operation.clone(),
                    target: target.into(),
                    conflict: ConflictPolicy::Reject,
                },
                policy: UploadPolicy::default(),
                stop: &stop,
                progress: &|p| {
                    println!(
                        "persisted={}/{} complete={} epoch={}",
                        p.persisted, p.total, p.complete, p.epoch
                    );
                    if crash && p.persisted >= 8 * 1024 * 1024 && !p.complete {
                        println!("故障注入：checkpoint 已持久化，进程直接退出，不执行析构。");
                        std::process::exit(75);
                    }
                    if pause && p.persisted >= 8 * 1024 * 1024 && !p.complete {
                        stop.stop();
                    }
                },
            },
        )
        .await;
    match receipt {
        Err(error) if pause && error.kind == ErrorKind::Paused => {
            println!("已持久暂停；使用 resume 在新进程继续。");
            Ok(())
        }
        Err(error) => Err(Box::new(error) as Box<dyn std::error::Error>),
        Ok(receipt) => {
            // 真机校验是消费者验收行为，不作为 core 的下载能力。
            verify_remote(&http, &token_path, &receipt.object, &digest).await?;
            save_private(
                &root.join(format!("result-{action}.json")),
                &serde_json::json!({
                    "object": receipt.object,
                    "root": config.root,
                    "size": receipt.size,
                    "remoteHashVerified": true,
                    "seconds": started.elapsed().as_secs_f64(),
                    "action": action
                }),
            )?;
            if args[3] == "confirm" {
                engine.confirm(&store, &receipt).await?;
            }
            println!("真实 Drive 上传/对账及测试文件下载哈希核验完成，结果保存在私有状态目录。");
            Ok(())
        }
    }
}
// 只读验收请求可以重试；每次从头计算哈希，不拼接失败请求的部分数据。
async fn verify_remote(
    http: &reqwest::Client,
    token_path: &Path,
    object: &str,
    expected: &str,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    for attempt in 0..3 {
        let token = drive_token(token_path)?;
        let read = async {
            let mut response = http
                .get(format!(
                    "https://www.googleapis.com/drive/v3/files/{object}"
                ))
                .query(&[("alt", "media")])
                .bearer_auth(token)
                .send()
                .await
                .map_err(|_| "remote test payload request failed")?;
            if !response.status().is_success() {
                return Err("remote test payload read failed");
            }
            let mut hash = blake3::Hasher::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| "remote test payload stream failed")?
            {
                hash.update(&chunk);
            }
            Ok(hash.finalize().to_hex().to_string())
        }
        .await;
        match read {
            Ok(hash) if hash == expected => return Ok(()),
            Ok(_) => return Err("remote test payload mismatch".into()),
            Err(reason) if attempt == 2 => return Err(reason.into()),
            Err(_) => tokio::time::sleep(Duration::from_secs(1 << attempt)).await,
        }
    }
    Err("remote test payload verification exhausted".into())
}
fn drive_token(path: &Path) -> std::result::Result<String, Box<dyn std::error::Error>> {
    let token: Token = serde_json::from_slice(&std::fs::read(path)?)?;
    Ok(token.access)
}

// 真机故障注入位于消费者边界：完成已发生，但结果未交付核心。
// 这验证真实对象对账，不伪称为 Google 网络层丢包。
struct LostCompletion {
    inner: Gdrive,
    dropped: std::sync::atomic::AtomicBool,
}
impl UploadSink for LostCompletion {
    fn identity(&self) -> ServiceIdentity {
        self.inner.identity()
    }
    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }
    fn prepare<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState> {
        self.inner.prepare(intent, source)
    }
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        self.inner.probe(intent, source, state)
    }
    fn initialize<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        self.inner.initialize(intent, source, state)
    }
    fn write_chunk<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async move {
            let result = self
                .inner
                .write_chunk(intent, source, state, offset, data)
                .await?;
            if matches!(result, SessionStatus::Complete { .. })
                && !self.dropped.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                println!("故障注入：真实 Drive 已完成，丢弃驱动完成结果，核心必须重新对账。");
                return Err(Error::new(
                    ErrorKind::Retryable,
                    "injected lost completion result",
                ));
            }
            Ok(result)
        })
    }
}
