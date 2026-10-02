//! 本机脚本化 HTTP 替身覆盖关键崩溃窗口，不代表 Google 真机验收。
use super::*;
use crate::credential::AccessToken;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use waybill::{
    BoxFuture,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore, Flow},
    error::{Error, ErrorKind},
    source::Source,
    transfer::{ConflictPolicy, StopToken, TransferEngine},
    upload::{UploadIntent, UploadOptions, UploadPolicy},
};
use waybill_service_fs::{FileCheckpointStore, FileSource};
struct Tokens {
    refreshes: AtomicUsize,
    reconnects: AtomicUsize,
}
impl Tokens {
    fn new() -> Self {
        Self {
            refreshes: AtomicUsize::new(0),
            reconnects: AtomicUsize::new(0),
        }
    }
}
impl TokenProvider for Tokens {
    fn access_token(&self, _: Duration) -> BoxFuture<'_, AccessToken> {
        Box::pin(async { Ok(AccessToken::new("secret-token", "1")) })
    }
    fn after_rejection<'a>(&'a self, _: &'a AccessToken) -> BoxFuture<'a, AccessToken> {
        Box::pin(async {
            self.refreshes.fetch_add(1, Ordering::SeqCst);
            Ok(AccessToken::new("secret-new-token", "2"))
        })
    }
    fn reconnect_required<'a>(&'a self, _: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async {
            self.reconnects.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Reply {
    method: &'static str,
    path: &'static str,
    status: u16,
    body: serde_json::Value,
    headers: Vec<(&'static str, String)>,
    range: Option<String>,
    disconnect: bool,
    raw: Option<Vec<u8>>,
    expect_headers: Vec<(&'static str, String)>,
    forbid_headers: Vec<&'static str>,
}
impl Reply {
    fn new(method: &'static str, path: &'static str, status: u16, body: serde_json::Value) -> Self {
        Self {
            method,
            path,
            status,
            body,
            headers: Vec::new(),
            range: None,
            disconnect: false,
            raw: None,
            expect_headers: Vec::new(),
            forbid_headers: Vec::new(),
        }
    }
    /// 二进制响应体（媒体内容）；优先于 JSON body。
    fn raw(method: &'static str, path: &'static str, status: u16, body: Vec<u8>) -> Self {
        Self {
            raw: Some(body),
            ..Self::new(method, path, status, serde_json::Value::Null)
        }
    }
    /// 断言请求携带指定头。
    fn expect_header(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.expect_headers.push((key, value.into()));
        self
    }
    /// 断言请求不携带指定头（如跨主机重定向后的 Authorization）。
    fn forbid_header(mut self, key: &'static str) -> Self {
        self.forbid_headers.push(key);
        self
    }
    fn header(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((key, value.into()));
        self
    }
    fn range(mut self, range: impl Into<String>) -> Self {
        self.range = Some(range.into());
        self
    }
    fn disconnect(mut self) -> Self {
        self.disconnect = true;
        self
    }
}
struct Server {
    origin: String,
    script: Arc<Mutex<VecDeque<Reply>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Server {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let script = Arc::new(Mutex::new(VecDeque::<Reply>::new()));
        let queue = script.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let mut buffer = [0u8; 16384];
                let headers_end = loop {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0, "missing request headers");
                    data.extend_from_slice(&buffer[..n]);
                    if let Some(index) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                        break index + 4;
                    }
                    assert!(data.len() < 65536);
                };
                let headers = String::from_utf8(data[..headers_end].to_vec()).unwrap();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while data.len() < headers_end + content_length {
                    let n = socket.read(&mut buffer).await.unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buffer[..n]);
                }
                let reply = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request");
                let first = headers.lines().next().unwrap();
                assert!(
                    first.starts_with(&format!("{} {}", reply.method, reply.path)),
                    "expected {} {}, received {first}",
                    reply.method,
                    reply.path
                );
                if let Some(range) = reply.range {
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains(&format!("content-range: {}", range.to_ascii_lowercase())),
                        "missing expected range"
                    );
                }
                for (key, value) in &reply.expect_headers {
                    assert!(
                        headers.to_ascii_lowercase().contains(&format!(
                            "{}: {}",
                            key,
                            value.to_ascii_lowercase()
                        )),
                        "missing expected request header {key}"
                    );
                }
                for key in &reply.forbid_headers {
                    assert!(
                        !headers.to_ascii_lowercase().contains(&format!("{key}:")),
                        "unexpected request header {key}"
                    );
                }
                if reply.disconnect {
                    drop(socket);
                    continue;
                }
                let body = if let Some(raw) = reply.raw {
                    raw
                } else if reply.body.is_null() {
                    Vec::new()
                } else {
                    serde_json::to_vec(&reply.body).unwrap()
                };
                let mut response = format!(
                    "HTTP/1.1 {} Test\r\nContent-Length: {}\r\nConnection: close\r\n",
                    reply.status,
                    body.len()
                );
                for (key, value) in reply.headers {
                    response.push_str(&format!("{key}: {value}\r\n"));
                }
                response.push_str("\r\n");
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
        });
        Self {
            origin,
            script,
            task,
        }
    }
    fn add(&self, replies: Vec<Reply>) {
        self.script.lock().unwrap().extend(replies);
    }
    fn finished(&self) {
        assert!(
            self.script.lock().unwrap().is_empty(),
            "unused HTTP expectations"
        );
        assert!(!self.task.is_finished(), "HTTP server failed");
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
fn drive(server: &Server, tokens: Arc<Tokens>) -> Gdrive {
    let mut drive = Gdrive::new(
        GdriveConfig {
            account: "account-a".into(),
            oauth_application: "oauth-client-a".into(),
            root: "root".into(),
        },
        tokens,
    )
    .unwrap();
    let api = Arc::get_mut(&mut drive.api).unwrap();
    api.api_root = format!("{}/drive/v3", server.origin);
    api.about_url = format!("{}/drive/v2/about", server.origin);
    api.upload_root = format!("{}/upload/drive/v3/files", server.origin);
    api.test_origin = Some(url::Url::parse(&server.origin).unwrap());
    drive
}
fn intent() -> UploadIntent {
    UploadIntent {
        operation: "operation-1".into(),
        target: "payload.bin".into(),
        conflict: ConflictPolicy::Reject,
    }
}
fn missing() -> Reply {
    Reply::new(
        "GET",
        "/drive/v3/files/object-1",
        404,
        serde_json::Value::Null,
    )
}
fn listing() -> Reply {
    Reply::new("GET", "/drive/v3/files?", 200, json!({"files":[]}))
}
fn prepare() -> Vec<Reply> {
    vec![
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        ),
        listing(),
        Reply::new(
            "GET",
            "/drive/v3/files/generateIds",
            200,
            json!({"ids":["object-1"]}),
        ),
        missing(),
    ]
}
fn initialize(server: &Server) -> Vec<Reply> {
    vec![
        missing(),
        listing(),
        Reply::new(
            "POST",
            "/upload/drive/v3/files",
            200,
            serde_json::Value::Null,
        )
        .header(
            "Location",
            format!(
                "{}/upload/drive/v3/files?upload_id=secret-session",
                server.origin
            ),
        ),
    ]
}
fn object(drive: &Gdrive, identity: &waybill::source::SourceIdentity) -> serde_json::Value {
    json!({"id":"object-1","name":"payload.bin","size":identity.size.to_string(),"parents":["folder-1"],"appProperties":drive.properties(&intent(),identity)})
}
fn engine(store: impl CheckpointStore + 'static) -> TransferEngine {
    TransferEngine::new(store).with_budget(Arc::new(ResourceBudget::new(256 * 1024, 2).unwrap()))
}
async fn source(dir: &tempfile::TempDir, size: usize) -> FileSource {
    let path = dir.path().join("source.bin");
    tokio::fs::write(&path, vec![0x5a; size]).await.unwrap();
    FileSource::open(path).await.unwrap()
}

#[tokio::test]
async fn pause_restart_reconciles_server_ahead_and_retains_receipt_until_confirm() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 300 * 1024).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("checkpoints"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143")
        .range("bytes 0-262143/307200"),
    ]);
    let stop = StopToken::default();
    let result = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: stop.clone(),
                progress: Some(&|p| {
                    if p.persisted == 262144 {
                        stop.stop();
                    }
                }),
            },
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Paused));
    // 模拟本地确认记录落后：服务器已接收比 checkpoint 更多的字节。
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    let Flow::Upload(flow) = &mut saved.flow else {
        panic!("upload flow");
    };
    flow.acknowledged = 0;
    lease.save(&saved).await.unwrap();
    drop(lease);
    server.add(vec![
        missing(),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143")
        .range("bytes */307200"),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            200,
            object(&drive, &identity),
        )
        .range("bytes 262144-307199/307200"),
    ]);
    let fresh_source = FileSource::open(dir.path().join("source.bin"))
        .await
        .unwrap();
    let receipt = engine(store.clone())
        .upload(
            &fresh_source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: StopToken::default(),
                progress: Some(&|_| {}),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.verified, waybill::content::Verification::Length);
    let lease = store.acquire("operation-1").await.unwrap();
    assert_eq!(
        lease
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .receipt,
        Some(receipt.clone())
    );
    drop(lease);
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/object-1",
        200,
        object(&drive, &identity),
    )]);
    assert_eq!(
        engine(store.clone())
            .upload(
                &fresh_source,
                &drive,
                UploadOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: StopToken::default(),
                    progress: Some(&|_| {})
                }
            )
            .await
            .unwrap(),
        receipt
    );
    engine(store.clone()).confirm(&receipt).await.unwrap();
    assert!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .is_none()
    );
    server.finished();
}

#[tokio::test]
async fn lost_completion_response_is_reconciled_without_duplicate_creation() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 42).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            200,
            serde_json::Value::Null,
        )
        .disconnect(),
        Reply::new(
            "GET",
            "/drive/v3/files/object-1",
            200,
            object(&drive, &identity),
        ),
    ]);
    let receipt = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: StopToken::default(),
                progress: Some(&|_| {}),
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.object, "object-1");
    server.finished();
}

#[tokio::test]
async fn expiration_requires_explicit_restart_and_reuses_object_id() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 42).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            404,
            serde_json::Value::Null,
        ),
        missing(),
    ]);
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await, Err(e) if e.kind == ErrorKind::SessionExpired)
    );
    assert!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .is_some()
    );
    server.add(vec![
        missing(),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            404,
            serde_json::Value::Null,
        ),
        missing(),
    ]);
    server.add(initialize(&server));
    server.add(vec![Reply::new(
        "PUT",
        "/upload/drive/v3/files",
        200,
        object(&drive, &identity),
    )]);
    let epochs = Mutex::new(Vec::new());
    engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy {
                    allow_restart: true,
                },
                stop: StopToken::default(),
                progress: Some(&|p| epochs.lock().unwrap().push(p.epoch)),
            },
        )
        .await
        .unwrap();
    assert!(epochs.lock().unwrap().contains(&1));
    server.finished();
}

#[tokio::test]
async fn server_offset_can_move_backwards_without_trusting_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 300 * 1024).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143"),
    ]);
    let stop = StopToken::default();
    let _ = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: stop.clone(),
                progress: Some(&|p| {
                    if p.persisted > 0 {
                        stop.stop()
                    }
                }),
            },
        )
        .await;
    server.add(vec![
        missing(),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        ),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143")
        .range("bytes 0-262143/307200"),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            200,
            object(&drive, &identity),
        )
        .range("bytes 262144-307199/307200"),
    ]);
    engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: StopToken::default(),
                progress: Some(&|_| {}),
            },
        )
        .await
        .unwrap();
    server.finished();
}

#[tokio::test]
async fn empty_file_and_authorization_refresh_are_supported() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let tokens = Arc::new(Tokens::new());
    let drive = drive(&server, tokens.clone());
    let source = source(&dir, 0).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(vec![Reply::new(
        "GET",
        "/drive/v2/about",
        401,
        serde_json::Value::Null,
    )]);
    server.add(prepare());
    server.add(vec![
        missing(),
        listing(),
        Reply::new("POST", "/drive/v3/files", 200, object(&drive, &identity)),
    ]);
    assert_eq!(
        engine(store.clone())
            .upload(
                &source,
                &drive,
                UploadOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: StopToken::default(),
                    progress: Some(&|_| {})
                }
            )
            .await
            .unwrap()
            .size,
        0
    );
    assert_eq!(tokens.refreshes.load(Ordering::SeqCst), 1);
    server.finished();
}

#[tokio::test]
async fn invalid_range_returns_protocol_error_and_keeps_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 42).await;
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-999"),
        missing(),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-999"),
    ]);
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::ResultUnknown)
    );
    assert!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .is_some()
    );
    server.finished();
}

#[derive(Clone)]
struct FailingStore {
    inner: FileCheckpointStore,
    fail_receipt: Arc<AtomicBool>,
}
struct FailingLease {
    inner: Box<dyn CheckpointLease>,
    fail_receipt: Arc<AtomicBool>,
}
impl CheckpointStore for FailingStore {
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        Box::pin(async move {
            Ok(Box::new(FailingLease {
                inner: self.inner.acquire(operation).await?,
                fail_receipt: self.fail_receipt.clone(),
            }) as Box<dyn CheckpointLease>)
        })
    }
}
impl CheckpointLease for FailingLease {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        self.inner.load()
    }
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if checkpoint
                .upload()
                .is_some_and(|flow| flow.receipt.is_some())
                && self.fail_receipt.swap(false, Ordering::SeqCst)
            {
                return Err(Error::new(
                    ErrorKind::Checkpoint,
                    "injected receipt disk failure",
                ));
            }
            self.inner.save(checkpoint).await
        })
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        self.inner.remove()
    }
}
#[tokio::test]
async fn remote_completion_survives_receipt_save_failure_and_instance_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 42).await;
    let identity = source.identity().await.unwrap();
    let store = FailingStore {
        inner: FileCheckpointStore::new(dir.path().join("cp")),
        fail_receipt: Arc::new(AtomicBool::new(true)),
    };
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![Reply::new(
        "PUT",
        "/upload/drive/v3/files",
        200,
        object(&drive, &identity),
    )]);
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::Checkpoint)
    );
    let mut another = drive.clone();
    another.identity.instance = "another-account".into();
    assert!(
        matches!(engine(store.clone()).upload(&source, &another, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::IdentityMismatch)
    );
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/object-1",
        200,
        object(&drive, &identity),
    )]);
    let receipt = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: StopToken::default(),
                progress: Some(&|_| {}),
            },
        )
        .await
        .unwrap();
    let mut wrong = receipt.clone();
    wrong.object = "not-the-object".into();
    assert!(
        matches!(engine(store.clone()).confirm(&wrong).await,Err(e) if e.kind==ErrorKind::IdentityMismatch)
    );
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    saved.version = 999;
    lease.save(&saved).await.unwrap();
    drop(lease);
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::IncompatibleVersion)
    );
    server.finished();
}
#[tokio::test]
async fn changed_source_and_driver_version_preserve_recovery_record() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 300 * 1024).await;
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143"),
    ]);
    let stop = StopToken::default();
    let _ = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: stop.clone(),
                progress: Some(&|p| {
                    if p.persisted > 0 {
                        stop.stop()
                    }
                }),
            },
        )
        .await;
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    let Flow::Upload(flow) = &mut saved.flow else {
        panic!("upload flow");
    };
    flow.driver.version = 999;
    lease.save(&saved).await.unwrap();
    drop(lease);
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::IncompatibleVersion)
    );
    tokio::fs::write(dir.path().join("source.bin"), b"changed")
        .await
        .unwrap();
    let replaced = FileSource::open(dir.path().join("source.bin"))
        .await
        .unwrap();
    assert!(
        matches!(engine(store.clone()).upload(&replaced, &drive, UploadOptions { intent: intent(), policy: UploadPolicy::default(), stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::SourceChanged)
    );
    assert!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .is_some()
    );
    server.finished();
}
#[tokio::test]
async fn transient_get_retries_are_bounded_and_new_token_rejection_notifies_host() {
    let server = Server::new().await;
    let tokens = Arc::new(Tokens::new());
    let drive = drive(&server, tokens.clone());
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v3/files/object-1",
            429,
            serde_json::Value::Null,
        )
        .header("Retry-After", "999999"),
        Reply::new(
            "GET",
            "/drive/v3/files/object-1",
            503,
            serde_json::Value::Null,
        ),
        missing(),
    ]);
    assert!(drive.api.get_file("object-1").await.unwrap().is_none());
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v3/files/object-1",
            401,
            serde_json::Value::Null,
        ),
        Reply::new(
            "GET",
            "/drive/v3/files/object-1",
            401,
            serde_json::Value::Null,
        ),
    ]);
    let error = drive.api.get_file("object-1").await.err().unwrap();
    assert_eq!(error.kind, ErrorKind::Authentication);
    assert!(!format!("{error:?}").contains("secret-token"));
    assert_eq!(tokens.reconnects.load(Ordering::SeqCst), 1);
    server.add(
        (0..4)
            .map(|_| {
                Reply::new(
                    "GET",
                    "/drive/v3/files/object-1",
                    503,
                    serde_json::Value::Null,
                )
            })
            .collect(),
    );
    assert!(matches!(drive.api.get_file("object-1").await,Err(e) if e.kind==ErrorKind::Retryable));
    server.finished();
}
#[tokio::test]
async fn expired_session_restart_count_is_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 42).await;
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    for attempt in 0..3 {
        server.add(vec![
            Reply::new(
                "PUT",
                "/upload/drive/v3/files",
                404,
                serde_json::Value::Null,
            ),
            missing(),
        ]);
        if attempt < 2 {
            server.add(initialize(&server));
        }
    }
    assert!(
        matches!(engine(store.clone()).upload(&source, &drive, UploadOptions { intent: intent(), policy: UploadPolicy{allow_restart:true}, stop: StopToken::default(), progress: Some(&|_|{})}).await,Err(e) if e.kind==ErrorKind::SessionExpired)
    );
    assert_eq!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .restarts,
        2
    );
    server.finished();
}
#[test]
fn production_session_urls_and_debug_output_are_strict() {
    let drive = Gdrive::new(
        GdriveConfig {
            account: "a".into(),
            oauth_application: "client".into(),
            root: "root".into(),
        },
        Arc::new(Tokens::new()),
    )
    .unwrap();
    for url in [
        "http://www.googleapis.com/upload/drive/v3/files",
        "https://evil.example/upload/drive/v3/files",
        "https://www.googleapis.com/upload/drive/v3/files-evil",
        "https://user:pass@www.googleapis.com/upload/drive/v3/files",
        "https://www.googleapis.com:444/upload/drive/v3/files",
    ] {
        assert!(drive.api.validate_session(url).is_err());
    }
    assert!(
        drive
            .api
            .validate_session("https://www.googleapis.com/upload/drive/v3/files?upload_id=secret")
            .is_ok()
    );
    assert!(
        !format!(
            "{:?}",
            AccessToken::new("credential-secret", "generation-secret")
        )
        .contains("credential-secret")
    );
}

#[tokio::test]
async fn a_new_process_resumes_the_persisted_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 300 * 1024).await;
    let identity = source.identity().await.unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143"),
    ]);
    let stop = StopToken::default();
    assert!(
        matches!(engine(store.clone()).upload(&source,&drive,UploadOptions{intent:intent(),policy:UploadPolicy::default(),stop:stop.clone(),progress: Some(&|p|if p.persisted>0{stop.stop()})}).await,Err(e) if e.kind==ErrorKind::Paused)
    );
    server.add(vec![
        missing(),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-262143"),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            200,
            object(&drive, &identity),
        )
        .range("bytes 262144-307199/307200"),
    ]);
    let path = dir.path().to_owned();
    let origin = server.origin.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::resume_child", "--ignored", "--nocapture"])
            .env("WAYBILL_TEST_RESUME_ROOT", path)
            .env("WAYBILL_TEST_ORIGIN", origin)
            .status()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(status.success());
    assert!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .receipt
            .is_some()
    );
    server.finished();
}
#[tokio::test]
#[ignore = "helper invoked by the cross-process resume test"]
async fn resume_child() {
    let root = std::path::PathBuf::from(std::env::var("WAYBILL_TEST_RESUME_ROOT").unwrap());
    let origin = std::env::var("WAYBILL_TEST_ORIGIN").unwrap();
    let mut drive = Gdrive::new(
        GdriveConfig {
            account: "account-a".into(),
            oauth_application: "oauth-client-a".into(),
            root: "root".into(),
        },
        Arc::new(Tokens::new()),
    )
    .unwrap();
    let api = Arc::get_mut(&mut drive.api).unwrap();
    api.api_root = format!("{origin}/drive/v3");
    api.about_url = format!("{origin}/drive/v2/about");
    api.upload_root = format!("{origin}/upload/drive/v3/files");
    api.test_origin = Some(url::Url::parse(&origin).unwrap());
    let source = FileSource::open(root.join("source.bin")).await.unwrap();
    let store = FileCheckpointStore::new(root.join("cp"));
    assert_eq!(
        engine(store.clone())
            .upload(
                &source,
                &drive,
                UploadOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: StopToken::default(),
                    progress: Some(&|_| {})
                }
            )
            .await
            .unwrap()
            .size,
        300 * 1024
    );
}

#[tokio::test]
async fn target_conflicts_require_explicit_suffix_and_directories_require_ownership() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let root = || {
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        )
    };
    let occupied = || {
        Reply::new(
            "GET",
            "/drive/v3/files?",
            200,
            json!({"files":[{
                "id":"foreign-object", "name":"payload.bin", "parents":["folder-1"]
            }]}),
        )
    };
    server.add(vec![root(), occupied()]);
    assert_eq!(
        drive.plan_target(&intent()).await.err().unwrap().kind,
        ErrorKind::Conflict
    );
    let mut renamed = intent();
    renamed.conflict = ConflictPolicy::OperationSuffix;
    server.add(vec![
        root(),
        occupied(),
        Reply::new(
            "GET",
            "/drive/v3/files/generateIds",
            200,
            json!({"ids":["object-1"]}),
        ),
    ]);
    let state = drive.plan_target(&renamed).await.unwrap();
    assert!(state.remote_name.starts_with("payload ("));
    assert!(state.remote_name.ends_with(").bin"));
    server.add(vec![occupied()]);
    assert_eq!(
        drive.check_collision(&state).await.unwrap_err().kind,
        ErrorKind::Conflict
    );
    let mut nested = intent();
    nested.target = "unowned/payload.bin".into();
    server.add(vec![
        root(),
        Reply::new(
            "GET",
            "/drive/v3/files?",
            200,
            json!({"files":[{
                "id":"foreign-dir", "name":"unowned", "parents":["folder-1"],
                "mimeType":"application/vnd.google-apps.folder"
            }]}),
        ),
    ]);
    assert_eq!(
        drive.plan_target(&nested).await.err().unwrap().kind,
        ErrorKind::Conflict
    );
    server.finished();
}

#[tokio::test]
async fn planned_directory_id_survives_a_lost_create_response() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let mut nested = intent();
    nested.target = "owned/payload.bin".into();
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        ),
        listing(),
        Reply::new(
            "GET",
            "/drive/v3/files/generateIds",
            200,
            json!({"ids":["directory-1"]}),
        ),
        Reply::new(
            "GET",
            "/drive/v3/files/generateIds",
            200,
            json!({"ids":["object-1"]}),
        ),
    ]);
    let state = drive.plan_target(&nested).await.unwrap();
    let checkpoint = state.encode().unwrap();
    // 状态编解码模拟持久边界；prepare 没有创建目录或文件对象。
    let restored = crate::object::State::decode(&checkpoint).unwrap();
    assert_eq!(restored.parent_id, "directory-1");
    let directory = &restored.directories[0];
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v3/files/directory-1",
            404,
            serde_json::Value::Null,
        ),
        Reply::new("POST", "/drive/v3/files", 200, serde_json::Value::Null).disconnect(),
        Reply::new("POST", "/drive/v3/files", 409, serde_json::Value::Null),
        Reply::new(
            "GET",
            "/drive/v3/files/directory-1",
            200,
            json!({
                "id":directory.id, "name":directory.name, "parents":[directory.parent],
                "mimeType":"application/vnd.google-apps.folder",
                "appProperties":{"waybill_directory":directory.key}
            }),
        ),
    ]);
    drive.ensure_directories(&restored).await.unwrap();
    server.finished();
}

#[tokio::test]
async fn a_stop_during_source_read_prevents_scheduling_another_request() {
    struct StopsRead<'a> {
        source: FileSource,
        stop: &'a StopToken,
    }
    impl Source for StopsRead<'_> {
        fn max_read_size(&self) -> usize {
            self.source.max_read_size()
        }
        fn identity(&self) -> BoxFuture<'_, waybill::source::SourceIdentity> {
            self.source.identity()
        }
        fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
            Box::pin(async move {
                let data = self.source.read_range(offset, length).await?;
                self.stop.stop();
                Ok(data)
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let stop = StopToken::default();
    let source = StopsRead {
        source: source(&dir, 300 * 1024).await,
        stop: &stop,
    };
    let store = FileCheckpointStore::new(dir.path().join("checkpoints"));
    server.add(prepare());
    server.add(initialize(&server));
    let result = engine(store.clone())
        .upload(
            &source,
            &drive,
            UploadOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: stop.clone(),
                progress: Some(&|_| {}),
            },
        )
        .await;
    assert_eq!(result.unwrap_err().kind, ErrorKind::Paused);
    assert_eq!(
        store
            .acquire("operation-1")
            .await
            .unwrap()
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .acknowledged,
        0
    );
    server.finished();
}

#[tokio::test]
async fn default_root_uses_about_when_root_file_is_not_visible() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/root",
        404,
        serde_json::Value::Null,
    )]);
    assert!(drive.api.get_file("root").await.unwrap().is_none());
    server.add(vec![
        Reply::new("GET", "/drive/v2/about?fields=rootFolderId", 200, json!({"rootFolderId":"folder-1"})),
        Reply::new("GET", "/drive/v3/files?q=trashed+%3D+false+and+%27folder-1%27+in+parents+and+name+%3D+%27payload.bin%27", 200, json!({"files":[]})),
        Reply::new("GET", "/drive/v3/files/generateIds", 200, json!({"ids":["object-1"]})),
    ]);
    let state = drive.plan_target(&intent()).await.unwrap();
    assert_eq!(state.parent_id, "folder-1");
    let source = waybill::source::SourceIdentity {
        reference: "source".into(),
        revision: "v1".into(),
        size: 1,
        blake3: "0".repeat(64),
    };
    let mut remote = object(&drive, &source);
    remote["parents"] = json!(["wrong-parent"]);
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/object-1",
        200,
        remote,
    )]);
    assert_eq!(
        drive
            .probe(&intent(), &source, &state.encode().unwrap())
            .await
            .err()
            .unwrap()
            .kind,
        ErrorKind::ResultUnknown
    );
    server.finished();
}

#[tokio::test]
async fn explicit_root_still_requires_a_matching_accessible_folder() {
    let server = Server::new().await;
    let mut drive = drive(&server, Arc::new(Tokens::new()));
    drive.config.root = "explicit-root".into();
    for (status, body, expected) in [
        (404, serde_json::Value::Null, ErrorKind::InvalidInput),
        (
            200,
            json!({"id":"explicit-root","name":"file","mimeType":"application/octet-stream"}),
            ErrorKind::InvalidInput,
        ),
        (
            200,
            json!({"id":"other-root","name":"folder","mimeType":"application/vnd.google-apps.folder"}),
            ErrorKind::IdentityMismatch,
        ),
    ] {
        server.add(vec![Reply::new(
            "GET",
            "/drive/v3/files/explicit-root",
            status,
            body,
        )]);
        assert_eq!(
            drive.plan_target(&intent()).await.err().unwrap().kind,
            expected
        );
    }
    server.finished();
}

#[tokio::test]
async fn default_root_rejects_invalid_about_identity() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    for id in ["root", "", "../folder"] {
        server.add(vec![Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":id}),
        )]);
        assert!(drive.plan_target(&intent()).await.is_err());
    }
    server.finished();
}
// ---------- 下载侧：范围读取、重定向白名单、元数据与路径解析 ----------

/// 元数据响应；md5 / version 供下载身份复核。
fn file_metadata(
    id: &'static str,
    name: &'static str,
    size: &'static str,
    md5: Option<&'static str>,
) -> Reply {
    let mut body = json!({
        "id": id, "name": name, "size": size, "mimeType": "application/zip",
        "trashed": false, "version": "3",
    });
    if let Some(md5) = md5 {
        body["md5Checksum"] = json!(md5);
    }
    let path: &'static str = Box::leak(format!("/drive/v3/files/{id}").into_boxed_str());
    Reply::new("GET", path, 200, body)
}
const MD5_16: &str = "0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn media_range_reads_validate_content_range_exactly() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![
        Reply::raw("GET", "/drive/v3/files/file-1", 206, b"01234567".to_vec())
            .header("Content-Range", "bytes 0-7/16")
            .expect_header("range", "bytes=0-7"),
    ]);
    let data = drive
        .media("file-1")
        .unwrap()
        .read_range(0, 8)
        .await
        .unwrap();
    assert_eq!(data, b"01234567");
    server.finished();
}

#[tokio::test]
async fn media_range_rejects_partial_prefix_full_body() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    // 服务器忽略 Range 且响应超出请求长度：必须拒绝，不能静默截断。
    server.add(vec![
        Reply::raw("GET", "/drive/v3/files/file-1", 200, vec![0u8; 16])
            .expect_header("range", "bytes=0-7"),
    ]);
    assert!(
        matches!(drive.media("file-1").unwrap().read_range(0, 8).await, Err(e) if e.kind == ErrorKind::Protocol)
    );
    server.finished();
}

#[tokio::test]
async fn media_full_file_ok_without_range_support() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![
        Reply::raw("GET", "/drive/v3/files/file-1", 200, vec![7u8; 16])
            .expect_header("range", "bytes=0-15"),
    ]);
    let data = drive
        .media("file-1")
        .unwrap()
        .read_range(0, 16)
        .await
        .unwrap();
    assert_eq!(data.len(), 16);
    server.finished();
}

#[tokio::test]
async fn media_range_errors_map_to_stable_kinds() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![Reply::raw(
        "GET",
        "/drive/v3/files/file-1",
        416,
        Vec::new(),
    )]);
    assert!(
        matches!(drive.media("file-1").unwrap().read_range(0, 8).await, Err(e) if e.kind == ErrorKind::SourceChanged)
    );
    server.add(vec![Reply::raw(
        "GET",
        "/drive/v3/files/file-1",
        404,
        Vec::new(),
    )]);
    assert!(
        matches!(drive.media("file-1").unwrap().read_range(0, 8).await, Err(e) if e.kind == ErrorKind::NotFound)
    );
    // 区间不匹配的 206 按协议错误处理。
    server.add(vec![
        Reply::raw("GET", "/drive/v3/files/file-1", 206, vec![0u8; 8])
            .header("Content-Range", "bytes 4-11/16"),
    ]);
    assert!(
        matches!(drive.media("file-1").unwrap().read_range(0, 8).await, Err(e) if e.kind == ErrorKind::Protocol)
    );
    server.finished();
}

#[tokio::test]
async fn media_redirects_drop_credentials_cross_origin() {
    let api = Server::new().await;
    let cdn = Server::new().await;
    let drive = drive(&api, Arc::new(Tokens::new()));
    // 首跳 302 到内容服务器（测试豁免下仍要求同 scheme；跨源必须免鉴权）。
    api.add(vec![
        Reply::raw("GET", "/drive/v3/files/file-1", 302, Vec::new())
            .header("Location", format!("{}/download/file-1", cdn.origin)),
    ]);
    cdn.add(vec![
        Reply::raw("GET", "/download/file-1", 206, b"abcdefgh".to_vec())
            .header("Content-Range", "bytes 0-7/16")
            .forbid_header("authorization"),
    ]);
    let data = drive
        .media("file-1")
        .unwrap()
        .read_range(0, 8)
        .await
        .unwrap();
    assert_eq!(data, b"abcdefgh");
    api.finished();
    cdn.finished();
}

#[tokio::test]
async fn media_redirect_targets_are_allowlisted() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    for location in [
        "http://evil.example/f",
        "https://user@evil.googleusercontent.com.evil.example/f",
    ] {
        server.add(vec![
            Reply::raw("GET", "/drive/v3/files/file-1", 302, Vec::new())
                .header("Location", location),
        ]);
        assert!(
            matches!(drive.media("file-1").unwrap().read_range(0, 8).await, Err(e) if e.kind == ErrorKind::Protocol),
            "重定向目标 {location} 必须被拒绝"
        );
    }
    server.finished();
}

#[tokio::test]
async fn download_identity_uses_version_md5_and_size() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![file_metadata("file-1", "a.zip", "16", Some(MD5_16))]);
    let identity = drive.media("file-1").unwrap().identity().await.unwrap();
    assert_eq!(identity.reference, "file-1");
    assert_eq!(identity.size, 16);
    assert_eq!(
        identity.revision,
        format!("3:{MD5_16}:16"),
        "revision 复合 version / md5 / size"
    );
    assert_eq!(
        identity
            .digest
            .as_ref()
            .map(|d| (d.algorithm, d.value.as_str())),
        Some((waybill::content::DigestAlgorithm::Md5, MD5_16))
    );
    server.finished();
}

#[tokio::test]
async fn download_identity_rejects_native_documents_and_missing_files() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    // Google 原生文档没有二进制内容：明确拒绝而不是退化为导出。
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/doc-1",
        200,
        json!({"id":"doc-1","name":"Doc","mimeType":"application/vnd.google-apps.document","trashed":false}),
    )]);
    assert!(
        matches!(drive.media("doc-1").unwrap().identity().await, Err(e) if e.kind == ErrorKind::InvalidInput)
    );
    server.add(vec![missing()]);
    assert!(
        matches!(drive.media("object-1").unwrap().identity().await, Err(e) if e.kind == ErrorKind::NotFound)
    );
    // 回收站对象按不可用处理。
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/file-2",
        200,
        json!({"id":"file-2","name":"a","size":"4","mimeType":"application/zip","trashed":true}),
    )]);
    assert!(
        matches!(drive.media("file-2").unwrap().identity().await, Err(e) if e.kind == ErrorKind::NotFound)
    );
    server.finished();
}

/// 根目录解析 + 单层路径命中的标准前置响应。
fn resolved_path() -> Vec<Reply> {
    vec![
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        ),
        Reply::new(
            "GET",
            "/drive/v3/files?",
            200,
            json!({"files":[{"id":"dir-1","name":"backup","mimeType":"application/vnd.google-apps.folder","trashed":false}]}),
        ),
        Reply::new(
            "GET",
            "/drive/v3/files?",
            200,
            json!({"files":[{"id":"file-1","name":"a.zip","size":"16","mimeType":"application/zip","trashed":false,"version":"3","md5Checksum":MD5_16}]}),
        ),
    ]
}

#[tokio::test]
async fn resolve_walks_segments_from_configured_root() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(resolved_path());
    let resolved = drive.resolve("backup/a.zip").await.unwrap();
    let Resolved::File(file) = resolved else {
        panic!("expected file");
    };
    assert_eq!(file.id, "file-1");
    assert_eq!(file.size, Some(16));
    assert!(!file.folder);
    // 根路径解析为根文件夹本身。
    server.add(vec![Reply::new(
        "GET",
        "/drive/v2/about",
        200,
        json!({"rootFolderId":"folder-1"}),
    )]);
    let Resolved::Folder { id } = drive.resolve("/").await.unwrap() else {
        panic!("expected folder");
    };
    assert_eq!(id, "folder-1");
    server.finished();
}

#[tokio::test]
async fn resolve_rejects_ambiguity_and_reports_missing_paths() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        ),
        Reply::new(
            "GET",
            "/drive/v3/files?",
            200,
            json!({"files":[
                {"id":"a-1","name":"backup","mimeType":"application/vnd.google-apps.folder","trashed":false},
                {"id":"a-2","name":"backup","mimeType":"application/vnd.google-apps.folder","trashed":false}
            ]}),
        ),
    ]);
    assert!(
        matches!(drive.resolve("backup/a.zip").await, Err(e) if e.kind == ErrorKind::InvalidInput),
        "同名目录必须报多义错误"
    );
    server.add(vec![
        Reply::new(
            "GET",
            "/drive/v2/about",
            200,
            json!({"rootFolderId":"folder-1"}),
        ),
        listing(),
    ]);
    assert!(matches!(drive.resolve("absent.bin").await, Err(e) if e.kind == ErrorKind::NotFound));
    server.finished();
}

#[tokio::test]
async fn list_folder_maps_children_with_folder_flags() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files?",
        200,
        json!({"files":[
            {"id":"d-1","name":"sub","mimeType":"application/vnd.google-apps.folder","trashed":false},
            {"id":"f-1","name":"a.zip","size":"16","mimeType":"application/zip","trashed":false,"modifiedTime":"2026-10-01T00:00:00Z"},
            {"id":"f-2","name":"doc","mimeType":"application/vnd.google-apps.document","trashed":false}
        ]}),
    )]);
    let children = drive.list("folder-1").await.unwrap();
    assert_eq!(children.len(), 3);
    assert!(children[0].folder);
    assert!(children[1].size.is_some());
    assert_eq!(
        children[1].modified_time.as_deref(),
        Some("2026-10-01T00:00:00Z")
    );
    assert!(children[2].size.is_none(), "原生文档无字节长度");
    server.finished();
}

#[tokio::test]
async fn upload_alignment_is_checked_before_preparing_a_remote_session() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 300 * 1024).await;
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    let result = TransferEngine::new(store.clone())
        .with_budget(Arc::new(ResourceBudget::new(128 * 1024, 1).unwrap()))
        .upload(&source, &drive, UploadOptions::new("limits", "file.bin"))
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::InvalidInput));
    assert!(!dir.path().join("cp").exists());
    server.finished();
}

#[tokio::test]
async fn drive_path_constraints_are_checked_before_network_io() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let result = drive
        .plan_target(&UploadOptions::new("path", "../file.bin").intent)
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::InvalidInput));
    server.finished();
}

#[tokio::test]
async fn a_large_budget_is_clamped_to_the_backend_chunk_limit() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    let source = source(&dir, 9 * 1024 * 1024).await;
    let identity = source.identity().await.unwrap();
    server.add(prepare());
    server.add(initialize(&server));
    server.add(vec![
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            308,
            serde_json::Value::Null,
        )
        .header("Range", "bytes=0-8388607")
        .range("bytes 0-8388607/9437184"),
        Reply::new(
            "PUT",
            "/upload/drive/v3/files",
            200,
            object(&drive, &identity),
        )
        .range("bytes 8388608-9437183/9437184"),
    ]);
    let engine = TransferEngine::new(FileCheckpointStore::new(dir.path().join("cp")))
        .with_budget(Arc::new(ResourceBudget::new(32 * 1024 * 1024, 1).unwrap()));
    let receipt = engine
        .upload(
            &source,
            &drive,
            UploadOptions::new("operation-1", "payload.bin"),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 9 * 1024 * 1024);
    server.finished();
}

#[tokio::test]
async fn invalid_server_digest_is_rejected_instead_of_downgrading_verification() {
    let server = Server::new().await;
    let drive = drive(&server, Arc::new(Tokens::new()));
    server.add(vec![file_metadata(
        "file-1",
        "a.zip",
        "16",
        Some("invalid-md5"),
    )]);
    let result = drive.media("file-1").unwrap().identity().await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Protocol));
    server.finished();
}
