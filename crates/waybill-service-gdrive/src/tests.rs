//! 本机脚本化 HTTP 替身覆盖关键崩溃窗口，不代表 Google 真机验收。
use super::*;
use crate::credential::AccessToken;
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use waybill::{
    BoxFuture,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore},
    error::{Error, ErrorKind},
    source::Source,
    upload::{
        ConflictPolicy, ResourceBudget, RunOptions, StopToken, UploadEngine, UploadIntent,
        UploadPolicy,
    },
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
        }
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
                if reply.disconnect {
                    drop(socket);
                    continue;
                }
                let body = if reply.body.is_null() {
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
            "/drive/v3/files/root",
            200,
            json!({"id":"folder-1","name":"root","mimeType":"application/vnd.google-apps.folder"}),
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
fn engine() -> UploadEngine {
    UploadEngine::new(Arc::new(ResourceBudget::new(256 * 1024, 2).unwrap()))
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
    let result = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &stop,
                progress: &|p| {
                    if p.persisted == 262144 {
                        stop.stop();
                    }
                },
            },
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Paused));
    // 模拟本地确认记录落后：服务器已接收比 checkpoint 更多的字节。
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    saved.acknowledged = 0;
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
    let receipt = engine()
        .run(
            &fresh_source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    let lease = store.acquire("operation-1").await.unwrap();
    assert_eq!(
        lease.load().await.unwrap().unwrap().receipt,
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
        engine()
            .run(
                &fresh_source,
                &drive,
                &store,
                RunOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: &StopToken::default(),
                    progress: &|_| {}
                }
            )
            .await
            .unwrap(),
        receipt
    );
    engine().confirm(&store, &receipt).await.unwrap();
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
    let receipt = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &StopToken::default(),
                progress: &|_| {},
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
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await, Err(e) if e.kind == ErrorKind::SessionExpired)
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
    engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy {
                    allow_restart: true,
                },
                stop: &StopToken::default(),
                progress: &|p| epochs.lock().unwrap().push(p.epoch),
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
    let _ = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &stop,
                progress: &|p| {
                    if p.persisted > 0 {
                        stop.stop()
                    }
                },
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
    engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &StopToken::default(),
                progress: &|_| {},
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
        "/drive/v3/files/root",
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
        engine()
            .run(
                &source,
                &drive,
                &store,
                RunOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: &StopToken::default(),
                    progress: &|_| {}
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
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::ResultUnknown)
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

struct FailingStore {
    inner: FileCheckpointStore,
    fail_receipt: Arc<AtomicUsize>,
}
struct FailingLease {
    inner: Box<dyn CheckpointLease>,
    fail_receipt: Arc<AtomicUsize>,
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
            if checkpoint.receipt.is_some()
                && self
                    .fail_receipt
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok()
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
        fail_receipt: Arc::new(AtomicUsize::new(1)),
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
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::Checkpoint)
    );
    let mut another = drive.clone();
    another.identity.instance = "another-account".into();
    assert!(
        matches!(engine().run(&source, &another, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::IdentityMismatch)
    );
    server.add(vec![Reply::new(
        "GET",
        "/drive/v3/files/object-1",
        200,
        object(&drive, &identity),
    )]);
    let receipt = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    let mut wrong = receipt.clone();
    wrong.object = "not-the-object".into();
    assert!(
        matches!(engine().confirm(&store,&wrong).await,Err(e) if e.kind==ErrorKind::IdentityMismatch)
    );
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    saved.version = 999;
    lease.save(&saved).await.unwrap();
    drop(lease);
    assert!(
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::IncompatibleVersion)
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
    let _ = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &stop,
                progress: &|p| {
                    if p.persisted > 0 {
                        stop.stop()
                    }
                },
            },
        )
        .await;
    let lease = store.acquire("operation-1").await.unwrap();
    let mut saved = lease.load().await.unwrap().unwrap();
    saved.driver.version = 999;
    lease.save(&saved).await.unwrap();
    drop(lease);
    assert!(
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::IncompatibleVersion)
    );
    tokio::fs::write(dir.path().join("source.bin"), b"changed")
        .await
        .unwrap();
    let replaced = FileSource::open(dir.path().join("source.bin"))
        .await
        .unwrap();
    assert!(
        matches!(engine().run(&replaced, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy::default(), stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::SourceChanged)
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
        matches!(engine().run(&source, &drive, &store, RunOptions { intent: intent(), policy: UploadPolicy{allow_restart:true}, stop: &StopToken::default(), progress: &|_|{} }).await,Err(e) if e.kind==ErrorKind::SessionExpired)
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
        matches!(engine().run(&source,&drive,&store,RunOptions{intent:intent(),policy:UploadPolicy::default(),stop:&stop,progress:&|p|if p.persisted>0{stop.stop()}}).await,Err(e) if e.kind==ErrorKind::Paused)
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
    api.upload_root = format!("{origin}/upload/drive/v3/files");
    api.test_origin = Some(url::Url::parse(&origin).unwrap());
    let source = FileSource::open(root.join("source.bin")).await.unwrap();
    let store = FileCheckpointStore::new(root.join("cp"));
    assert_eq!(
        engine()
            .run(
                &source,
                &drive,
                &store,
                RunOptions {
                    intent: intent(),
                    policy: UploadPolicy::default(),
                    stop: &StopToken::default(),
                    progress: &|_| {}
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
            "/drive/v3/files/root",
            200,
            json!({
                "id":"folder-1", "name":"root", "mimeType":"application/vnd.google-apps.folder"
            }),
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
            "/drive/v3/files/root",
            200,
            json!({"id":"folder-1","name":"root","mimeType":"application/vnd.google-apps.folder"}),
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
    let result = engine()
        .run(
            &source,
            &drive,
            &store,
            RunOptions {
                intent: intent(),
                policy: UploadPolicy::default(),
                stop: &stop,
                progress: &|_| {},
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
            .acknowledged,
        0
    );
    server.finished();
}
