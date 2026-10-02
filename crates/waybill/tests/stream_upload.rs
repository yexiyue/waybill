//! 整文件上传的持久化顺序、显式重传和发布窗口，使用外部消费者端口验证。
use futures_util::TryStreamExt;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use waybill::{
    BoxFuture, TransferEngine, UploadOptions,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore, DriverState},
    content::Verification,
    error::{Error, ErrorKind},
    service::{Capabilities, ServiceId, ServiceIdentity},
    source::{Source, SourceIdentity},
    transfer::{Receipt, StopToken},
    upload::{StreamStatus, StreamUploadSink, UploadBody, UploadIntent, UploadMode},
};
#[derive(Clone, Default)]
struct Store(Arc<Mutex<Option<Checkpoint>>>);
impl CheckpointStore for Store {
    fn acquire<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        Box::pin(async { Ok(Box::new(self.clone()) as Box<dyn CheckpointLease>) })
    }
}
impl CheckpointLease for Store {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        Box::pin(async { Ok(self.0.lock().unwrap().clone()) })
    }
    fn save<'a>(&'a self, saved: &'a Checkpoint) -> BoxFuture<'a, ()> {
        Box::pin(async {
            *self.0.lock().unwrap() = Some(saved.clone());
            Ok(())
        })
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        Box::pin(async {
            *self.0.lock().unwrap() = None;
            Ok(())
        })
    }
}
struct Input {
    reads: AtomicUsize,
    changed: AtomicBool,
}
impl Input {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            reads: AtomicUsize::new(0),
            changed: AtomicBool::new(false),
        })
    }
}
impl Source for Input {
    fn max_read_size(&self) -> usize {
        8
    }
    fn identity(&self) -> BoxFuture<'_, SourceIdentity> {
        Box::pin(async {
            Ok(SourceIdentity {
                reference: "input".into(),
                revision: if self.changed.load(Ordering::Relaxed) {
                    "new"
                } else {
                    "old"
                }
                .into(),
                size: 11,
                blake3: "0".repeat(64),
            })
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            assert!(length <= 3);
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(b"hello world"[offset as usize..offset as usize + length].to_vec())
        })
    }
}
struct Sink {
    store: Store,
    staged: AtomicBool,
    published: AtomicBool,
    writes: AtomicUsize,
    publishes: AtomicUsize,
    fail_write: AtomicBool,
    lose_responses: bool,
    invalid_receipt: bool,
    regress_after_publish: bool,
    background_body: bool,
    stop_after_write: Option<StopToken>,
    change_after_write: Option<Arc<Input>>,
}
impl Sink {
    fn new(store: Store) -> Self {
        Self {
            store,
            staged: AtomicBool::new(false),
            published: AtomicBool::new(false),
            writes: AtomicUsize::new(0),
            publishes: AtomicUsize::new(0),
            fail_write: AtomicBool::new(false),
            lose_responses: false,
            invalid_receipt: false,
            regress_after_publish: false,
            background_body: false,
            stop_after_write: None,
            change_after_write: None,
        }
    }
    fn assert_saved(&self, state: &DriverState) {
        let saved = self.store.0.lock().unwrap();
        let flow = saved.as_ref().unwrap().upload().unwrap();
        assert_eq!(flow.mode, UploadMode::Stream);
        assert_eq!(&flow.driver, state);
        assert!(flow.receipt.is_none());
    }
}
fn state(phase: u8) -> DriverState {
    DriverState {
        version: 1,
        payload: vec![phase],
    }
}
impl StreamUploadSink for Sink {
    fn identity(&self) -> ServiceIdentity {
        ServiceIdentity {
            service: ServiceId::parse("test:stream").unwrap(),
            instance: "account/root".into(),
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            stream_upload: true,
            durable_upload: true,
            ..Default::default()
        }
    }
    fn prepare<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async { Ok(state(0)) })
    }
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        saved: &'a DriverState,
        read_limit: usize,
    ) -> BoxFuture<'a, StreamStatus> {
        Box::pin(async move {
            assert_eq!(read_limit, 3);
            if self.published.load(Ordering::Relaxed) {
                if self.regress_after_publish {
                    return Ok(StreamStatus::Ready {
                        state: state(1),
                        restart_required: true,
                    });
                }
                return Ok(StreamStatus::Complete {
                    state: state(3),
                    receipt: Receipt {
                        operation: intent.operation.clone(),
                        service: self.identity(),
                        target: intent.target.clone(),
                        object: "final".into(),
                        size: source.size + u64::from(self.invalid_receipt),
                        verified: Verification::Length,
                    },
                });
            }
            if self.staged.load(Ordering::Relaxed) {
                return Ok(StreamStatus::Staged(state(2)));
            }
            Ok(StreamStatus::Ready {
                state: saved.clone(),
                restart_required: saved.payload[0] != 0,
            })
        })
    }
    fn begin<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        _: &'a DriverState,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async { Ok(state(1)) })
    }
    fn write<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        saved: &'a DriverState,
        mut body: UploadBody,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.assert_saved(saved);
            self.writes.fetch_add(1, Ordering::Relaxed);
            let data = if self.background_body {
                let mut tasks = tokio::task::JoinSet::new();
                tasks.spawn(async move {
                    let mut data = Vec::new();
                    while let Some(chunk) = body.try_next().await? {
                        data.extend(chunk);
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    Ok::<_, Error>(data)
                });
                tasks.join_next().await.unwrap().unwrap()?
            } else {
                let mut data = Vec::new();
                while let Some(chunk) = body.try_next().await? {
                    data.extend(chunk);
                    if self.fail_write.swap(false, Ordering::Relaxed) {
                        return Err(network());
                    }
                }
                data
            };
            assert_eq!(data, b"hello world");
            self.staged.store(true, Ordering::Relaxed);
            if let Some(stop) = &self.stop_after_write {
                stop.stop();
            }
            if let Some(input) = &self.change_after_write {
                input.changed.store(true, Ordering::Relaxed);
            }
            if self.lose_responses {
                Err(network())
            } else {
                Ok(())
            }
        })
    }
    fn publish<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        saved: &'a DriverState,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.assert_saved(saved);
            self.publishes.fetch_add(1, Ordering::Relaxed);
            self.published.store(true, Ordering::Relaxed);
            if self.lose_responses {
                Err(network())
            } else {
                Ok(())
            }
        })
    }
}
fn network() -> Error {
    Error::new(ErrorKind::Retryable, "response lost")
}
fn setup() -> (Store, TransferEngine, Arc<Input>) {
    let store = Store::default();
    let engine = TransferEngine::new(store.clone())
        .with_budget(Arc::new(ResourceBudget::new(3, 1).unwrap()));
    (store, engine, Input::new())
}
fn options() -> UploadOptions<'static> {
    UploadOptions::new("stream-op", "file.bin")
}
#[tokio::test]
async fn lost_write_and_publish_responses_reconcile_without_duplicate_upload() {
    let (store, engine, input) = setup();
    let mut sink = Sink::new(store.clone());
    sink.lose_responses = true;
    let receipt = engine
        .upload_stream(input.clone(), &sink, options())
        .await
        .unwrap();
    assert_eq!(input.reads.load(Ordering::Relaxed), 4);
    assert_eq!(
        engine
            .upload_stream(input.clone(), &sink, options())
            .await
            .unwrap(),
        receipt
    );
    assert_eq!(sink.writes.load(Ordering::Relaxed), 1);
    assert_eq!(sink.publishes.load(Ordering::Relaxed), 1);
    engine.confirm(&receipt).await.unwrap();
    assert!(store.0.lock().unwrap().is_none());
}
#[tokio::test]
async fn partial_request_requires_explicit_restart_and_persists_epoch() {
    let (store, engine, input) = setup();
    let sink = Sink::new(store.clone());
    sink.fail_write.store(true, Ordering::Relaxed);
    assert_eq!(
        engine
            .upload_stream(input.clone(), &sink, options())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Retryable
    );
    assert_eq!(
        engine
            .upload_stream(input.clone(), &sink, options())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::SessionExpired
    );
    assert_eq!(sink.writes.load(Ordering::Relaxed), 1);
    let mut opts = options();
    opts.policy.allow_restart = true;
    engine.upload_stream(input, &sink, opts).await.unwrap();
    assert_eq!(
        store
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upload()
            .unwrap()
            .restarts,
        1
    );
}
#[tokio::test]
async fn paused_staged_transfer_only_retries_publish() {
    let (store, engine, input) = setup();
    let stop = StopToken::default();
    let mut sink = Sink::new(store.clone());
    sink.stop_after_write = Some(stop.clone());
    let mut opts = options();
    opts.stop = stop;
    assert_eq!(
        engine
            .upload_stream(input.clone(), &sink, opts)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Paused
    );
    let flow = store
        .0
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .upload()
        .unwrap()
        .clone();
    assert_eq!(flow.acknowledged, 11);
    assert!(flow.receipt.is_none());
    assert_eq!(sink.publishes.load(Ordering::Relaxed), 0);
    engine.upload_stream(input, &sink, options()).await.unwrap();
    assert_eq!(sink.writes.load(Ordering::Relaxed), 1);
}
#[tokio::test]
async fn changed_source_before_publish_retains_staged_data() {
    let (store, engine, input) = setup();
    let mut sink = Sink::new(store.clone());
    sink.change_after_write = Some(input.clone());
    assert_eq!(
        engine
            .upload_stream(input, &sink, options())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::SourceChanged
    );
    assert_eq!(sink.publishes.load(Ordering::Relaxed), 0);
    assert!(
        store
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upload()
            .unwrap()
            .receipt
            .is_none()
    );
}
#[tokio::test]
async fn invalid_receipt_cannot_be_persisted_as_success() {
    let (store, engine, input) = setup();
    let mut sink = Sink::new(store.clone());
    sink.invalid_receipt = true;
    assert_eq!(
        engine
            .upload_stream(input, &sink, options())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::Protocol
    );
    assert!(
        store
            .0
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upload()
            .unwrap()
            .receipt
            .is_none()
    );
}
#[tokio::test]
async fn upload_mode_mismatch_is_rejected_before_another_write() {
    let (store, engine, input) = setup();
    let sink = Sink::new(store.clone());
    sink.fail_write.store(true, Ordering::Relaxed);
    engine
        .upload_stream(input.clone(), &sink, options())
        .await
        .unwrap_err();
    {
        let mut saved = store.0.lock().unwrap();
        let waybill::checkpoint::Flow::Upload(flow) = &mut saved.as_mut().unwrap().flow else {
            panic!()
        };
        flow.mode = UploadMode::Offset;
    }
    assert_eq!(
        engine
            .upload_stream(input, &sink, options())
            .await
            .unwrap_err()
            .kind,
        ErrorKind::IdentityMismatch
    );
    assert_eq!(sink.writes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn uncertain_publish_cannot_trigger_another_put_even_when_restart_is_allowed() {
    let (store, engine, input) = setup();
    let mut sink = Sink::new(store.clone());
    sink.regress_after_publish = true;
    let mut opts = options();
    opts.policy.allow_restart = true;
    assert_eq!(
        engine
            .upload_stream(input, &sink, opts)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::ResultUnknown
    );
    assert_eq!(sink.writes.load(Ordering::Relaxed), 1);
    assert_eq!(sink.publishes.load(Ordering::Relaxed), 1);
    let saved = store.0.lock().unwrap();
    let flow = saved.as_ref().unwrap().upload().unwrap();
    assert_eq!(flow.acknowledged, 11);
    assert_eq!(flow.driver, state(2));
    assert!(flow.receipt.is_none());
}

#[tokio::test]
async fn background_body_consumption_reports_progress_before_request_completion() {
    let (store, engine, input) = setup();
    let mut sink = Sink::new(store);
    sink.background_body = true;
    let observed = Mutex::new(Vec::new());
    let progress = |p: waybill::upload::UploadProgress| {
        observed
            .lock()
            .unwrap()
            .push((p.sent, p.persisted, p.complete));
    };
    let mut opts = options();
    opts.progress = Some(&progress);
    engine.upload_stream(input, &sink, opts).await.unwrap();
    assert!(
        observed
            .lock()
            .unwrap()
            .iter()
            .any(|&(sent, persisted, complete)| sent > 0
                && sent < 11
                && persisted == 0
                && !complete)
    );
}
