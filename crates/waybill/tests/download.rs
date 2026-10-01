//! 下载引擎的契约级测试：区间账本、并行调度、先落盘后记账、
//! 发布失败只重发布与 v1 信封兼容。源与目标为行为可注入的替身。
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use waybill::{
    BoxFuture,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore, Flow},
    checkpoint::{DownloadFlow, DriverState},
    download::{
        Digest, DigestAlgorithm, DownloadEngine, DownloadIntent, DownloadOptions, DownloadProgress,
        DownloadSource, DownloadStatus, DownloadTarget, Interval, RemoteIdentity, Verification,
    },
    error::{Error, ErrorKind},
    service::{Capabilities, ServiceId, ServiceIdentity},
    upload::{ConflictPolicy, Receipt, StopToken},
};

/// 全局事件序列：write / verify / publish / save 按发生顺序记录，
/// 用于钉死「数据写入并同步后才记账」的顺序约束。
type Events = Arc<Mutex<Vec<String>>>;

fn events() -> Events {
    Arc::new(Mutex::new(Vec::new()))
}

fn log(events: &Events, entry: String) {
    events.lock().unwrap().push(entry);
}

/// 可注入延迟、失败与版本变化的云端源替身。
struct FakeSource {
    instance: String,
    data: Vec<u8>,
    revision: Mutex<String>,
    digest: Option<Digest>,
    delays: HashMap<u64, Duration>,
    failures: Mutex<HashSet<u64>>,
    reads: Mutex<Vec<u64>>,
    gauge: AtomicUsize,
    max_gauge: AtomicUsize,
}
impl FakeSource {
    fn new(size: usize) -> Self {
        Self {
            instance: "source-account".into(),
            data: (0..size).map(|byte| (byte % 251) as u8).collect(),
            revision: Mutex::new("rev-1".into()),
            digest: Some(Digest {
                algorithm: DigestAlgorithm::Md5,
                value: "0".repeat(32),
            }),
            delays: HashMap::new(),
            failures: Mutex::new(HashSet::new()),
            reads: Mutex::new(Vec::new()),
            gauge: AtomicUsize::new(0),
            max_gauge: AtomicUsize::new(0),
        }
    }
    fn identity_value(&self) -> RemoteIdentity {
        RemoteIdentity {
            service: ServiceIdentity {
                service: ServiceId::parse("test:remote").unwrap(),
                instance: self.instance.clone(),
            },
            reference: "fake-object".into(),
            revision: self.revision.lock().unwrap().clone(),
            size: self.data.len() as u64,
            digest: self.digest.clone(),
        }
    }
}
impl DownloadSource for FakeSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_download: true,
            ..Default::default()
        }
    }
    fn identity(&self) -> BoxFuture<'_, RemoteIdentity> {
        let identity = self.identity_value();
        Box::pin(async move { Ok(identity) })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        let gauge = &self.gauge;
        let max_gauge = &self.max_gauge;
        let data = self
            .data
            .get(offset as usize..)
            .map(|slice| slice[..length].to_vec());
        let failed = self.failures.lock().unwrap().contains(&offset);
        self.reads.lock().unwrap().push(offset);
        let delay = self.delays.get(&offset).copied();
        Box::pin(async move {
            let current = gauge.fetch_add(1, Ordering::AcqRel) + 1;
            max_gauge.fetch_max(current, Ordering::AcqRel);
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            } else {
                tokio::task::yield_now().await;
            }
            gauge.fetch_sub(1, Ordering::AcqRel);
            if failed {
                return Err(Error::new(ErrorKind::Retryable, "injected read failure"));
            }
            data.ok_or_else(|| Error::new(ErrorKind::InvalidInput, "range out of bounds"))
        })
    }
}

/// 假目标驱动状态：只记录初始化与校验两个事实。
#[derive(serde::Serialize, serde::Deserialize)]
struct FakeTargetState {
    initialized: bool,
    verified: bool,
}
fn encode_state(state: &FakeTargetState) -> DriverState {
    DriverState {
        version: 1,
        payload: serde_json::to_vec(state).unwrap(),
    }
}
fn decode_state(state: &DriverState) -> FakeTargetState {
    serde_json::from_slice(&state.payload).unwrap()
}

/// 行为可注入的本地目标替身：内存暂存、可配置冲突与校验失败。
struct FakeTarget {
    identity: ServiceIdentity,
    events: Events,
    inner: Mutex<TargetInner>,
    support_random_write: bool,
}
struct TargetInner {
    verified: bool,
    published: Option<Receipt>,
    staging: Vec<u8>,
    /// 目标位置自始被占；用于下载前的早期冲突拒绝。
    dest_occupied: bool,
    /// 数据收齐后的首次发布冲突；用于发布失败保留暂存窗口。
    fail_publish_once: bool,
    fail_verify: bool,
}
impl FakeTarget {
    fn new(events: &Events) -> Self {
        Self {
            identity: ServiceIdentity {
                service: ServiceId::parse("test:local").unwrap(),
                instance: "fake-target".into(),
            },
            events: Arc::clone(events),
            inner: Mutex::new(TargetInner {
                verified: false,
                published: None,
                staging: Vec::new(),
                dest_occupied: false,
                fail_publish_once: false,
                fail_verify: false,
            }),
            support_random_write: true,
        }
    }
    fn build_receipt(&self, intent: &DownloadIntent, source: &RemoteIdentity) -> Receipt {
        let verified = match &source.digest {
            Some(digest) => Verification::Digest {
                algorithm: digest.algorithm,
                value: digest.value.clone(),
            },
            None => Verification::Length,
        };
        Receipt {
            operation: intent.operation.clone(),
            service: self.identity.clone(),
            target: intent.target.clone(),
            object: format!("file://{}", intent.target),
            size: source.size,
            verified,
        }
    }
}
impl DownloadTarget for FakeTarget {
    fn identity(&self) -> ServiceIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            random_write: self.support_random_write,
            durable_publish: true,
            ..Default::default()
        }
    }
    fn prepare<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        _source: &'a RemoteIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async move {
            Ok(encode_state(&FakeTargetState {
                initialized: false,
                verified: false,
            }))
        })
    }
    fn probe<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        _source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        let inner = self.inner.lock().unwrap();
        let status = if let Some(receipt) = inner.published.clone() {
            DownloadStatus::Complete {
                state: encode_state(&FakeTargetState {
                    initialized: true,
                    verified: true,
                }),
                receipt,
            }
        } else {
            let decoded = decode_state(state);
            if decoded.initialized {
                DownloadStatus::Ready {
                    state: encode_state(&decoded),
                    verified: decoded.verified,
                }
            } else {
                DownloadStatus::NeedsReset(encode_state(&decoded))
            }
        };
        Box::pin(async move { Ok(status) })
    }
    fn initialize<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        _state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        if self.inner.lock().unwrap().dest_occupied {
            return Box::pin(async move {
                Err(Error::new(ErrorKind::Conflict, "destination occupied"))
            });
        }
        let mut inner = self.inner.lock().unwrap();
        inner.verified = false;
        inner.staging = vec![0u8; source.size as usize];
        Box::pin(async move {
            Ok(DownloadStatus::Ready {
                state: encode_state(&FakeTargetState {
                    initialized: true,
                    verified: false,
                }),
                verified: false,
            })
        })
    }
    fn write_chunk<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        _state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, DriverState> {
        let mut inner = self.inner.lock().unwrap();
        let end = offset + data.len() as u64;
        if offset >= source.size || end > source.size {
            return Box::pin(async move {
                Err(Error::new(ErrorKind::InvalidInput, "write out of bounds"))
            });
        }
        // 替身以重开暂存模拟 `.part`；真实目标在 probe/initialize 对账。
        if inner.staging.len() != source.size as usize {
            inner.staging.resize(source.size as usize, 0);
        }
        inner.staging[offset as usize..end as usize].copy_from_slice(&data);
        inner.verified = false;
        log(&self.events, format!("write:{}:{}", offset, data.len()));
        Box::pin(async move {
            Ok(encode_state(&FakeTargetState {
                initialized: true,
                verified: false,
            }))
        })
    }
    fn verify<'a>(
        &'a self,
        _intent: &'a DownloadIntent,
        _source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DriverState> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_verify {
            return Box::pin(async move {
                Err(Error::new(ErrorKind::Checkpoint, "staging digest mismatch"))
            });
        }
        inner.verified = true;
        log(&self.events, "verify".into());
        let decoded = decode_state(state);
        Box::pin(async move {
            Ok(encode_state(&FakeTargetState {
                initialized: decoded.initialized,
                verified: true,
            }))
        })
    }
    fn publish<'a>(
        &'a self,
        intent: &'a DownloadIntent,
        source: &'a RemoteIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DownloadStatus> {
        let mut inner = self.inner.lock().unwrap();
        if inner.dest_occupied || inner.fail_publish_once {
            inner.fail_publish_once = false;
            return Box::pin(async move {
                Err(Error::new(ErrorKind::Conflict, "destination occupied"))
            });
        }
        let receipt = self.build_receipt(intent, source);
        inner.published = Some(receipt.clone());
        log(&self.events, "publish".into());
        let decoded = decode_state(state);
        Box::pin(async move {
            Ok(DownloadStatus::Complete {
                state: encode_state(&FakeTargetState {
                    initialized: decoded.initialized,
                    verified: true,
                }),
                receipt,
            })
        })
    }
}

/// 跨 lease 共享的内存 checkpoint 存储；可注入「带回执保存失败」。
#[derive(Clone)]
struct SharedStore {
    records: Arc<Mutex<HashMap<String, Checkpoint>>>,
    fail_save_with_receipt: Arc<AtomicBool>,
    events: Events,
}
impl SharedStore {
    fn new(events: &Events) -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            fail_save_with_receipt: Arc::new(AtomicBool::new(false)),
            events: Arc::clone(events),
        }
    }
    fn seed(&self, checkpoint: Checkpoint) {
        let operation = match &checkpoint.flow {
            Flow::Upload(flow) => flow.intent.operation.clone(),
            Flow::Download(flow) => flow.intent.operation.clone(),
        };
        self.records.lock().unwrap().insert(operation, checkpoint);
    }
    fn contains(&self, operation: &str) -> bool {
        self.records.lock().unwrap().contains_key(operation)
    }
}
impl CheckpointStore for SharedStore {
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        let lease = SharedLease {
            records: Arc::clone(&self.records),
            fail_save_with_receipt: Arc::clone(&self.fail_save_with_receipt),
            events: Arc::clone(&self.events),
            operation: operation.to_string(),
        };
        Box::pin(async move { Ok(Box::new(lease) as Box<dyn CheckpointLease>) })
    }
}
struct SharedLease {
    records: Arc<Mutex<HashMap<String, Checkpoint>>>,
    fail_save_with_receipt: Arc<AtomicBool>,
    events: Events,
    operation: String,
}
impl CheckpointLease for SharedLease {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        let record = self.records.lock().unwrap().get(&self.operation).cloned();
        Box::pin(async move { Ok(record) })
    }
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()> {
        let with_receipt = match &checkpoint.flow {
            Flow::Upload(flow) => flow.receipt.is_some(),
            Flow::Download(flow) => flow.receipt.is_some(),
        };
        if with_receipt && self.fail_save_with_receipt.load(Ordering::Acquire) {
            return Box::pin(async move {
                Err(Error::new(
                    ErrorKind::Checkpoint,
                    "injected receipt save failure",
                ))
            });
        }
        let covered = match &checkpoint.flow {
            Flow::Download(flow) => flow
                .persisted
                .iter()
                .map(|interval| interval.end - interval.start)
                .sum::<u64>(),
            Flow::Upload(flow) => flow.acknowledged,
        };
        log(&self.events, format!("save:{covered}"));
        self.records
            .lock()
            .unwrap()
            .insert(self.operation.clone(), checkpoint.clone());
        Box::pin(async move { Ok(()) })
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        self.records.lock().unwrap().remove(&self.operation);
        Box::pin(async move { Ok(()) })
    }
}

fn intent(operation: &str) -> DownloadIntent {
    DownloadIntent {
        operation: operation.into(),
        target: "/tmp/restore/file.bin".into(),
        conflict: ConflictPolicy::Reject,
    }
}
fn engine(chunk: usize, concurrency: usize) -> DownloadEngine {
    DownloadEngine::new(Arc::new(ResourceBudget::new(chunk, concurrency).unwrap()))
}
fn options<'a>(
    intent: DownloadIntent,
    stop: &'a StopToken,
    progress: &'a (dyn Fn(DownloadProgress) + Send + Sync),
) -> DownloadOptions<'a> {
    DownloadOptions {
        intent,
        stop,
        progress,
    }
}

#[tokio::test]
async fn out_of_order_arrivals_merge_into_one_interval() {
    let events = events();
    let mut source = FakeSource::new(20);
    // 首块延迟制造乱序到达：第二路先完成写入。
    source.delays.insert(0, Duration::from_millis(30));
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let receipt = engine(8, 2)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 20);
    assert_eq!(source.max_gauge.load(Ordering::Acquire), 2);
    let sequence = events.lock().unwrap().clone();
    let writes: Vec<&str> = sequence
        .iter()
        .map(String::as_str)
        .filter(|entry| entry.starts_with("write:"))
        .collect();
    assert_eq!(writes[0], "write:8:8", "第二路必须先于延迟的首路写入");
    let saves: Vec<&str> = sequence
        .iter()
        .map(String::as_str)
        .filter(|entry| entry.starts_with("save:"))
        .collect();
    assert_eq!(saves.last(), Some(&"save:20"));
}

#[tokio::test]
async fn budget_bounds_inflight_reads() {
    let events = events();
    let source = FakeSource::new(24);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(source.max_gauge.load(Ordering::Acquire), 1);
    assert_eq!(source.reads.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn resumed_ledger_skips_persisted_ranges() {
    let events = events();
    let source = FakeSource::new(24);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    // 预置已持久化首块的记录；目标侧驱动状态已初始化。
    store.seed(
        DownloadFlow {
            intent: intent("op-1"),
            service: target.identity(),
            source: source.identity_value(),
            persisted: vec![Interval { start: 0, end: 8 }],
            driver: encode_state(&FakeTargetState {
                initialized: true,
                verified: false,
            }),
            receipt: None,
        }
        .checkpoint(),
    );
    let receipt = engine(8, 2)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 24);
    let reads = source.reads.lock().unwrap().clone();
    assert!(!reads.contains(&0), "已持久化区间不得重取");
    assert!(reads.contains(&8) && reads.contains(&16));
}

#[tokio::test]
async fn writes_are_synced_before_the_ledger_records_them() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    let sequence = events.lock().unwrap().clone();
    // 每次写入后最近的 save 覆盖量必须达到写入区间末尾。
    for (index, entry) in sequence.iter().enumerate() {
        if let Some(rest) = entry.strip_prefix("write:") {
            let mut parts = rest.split(':');
            let offset: u64 = parts.next().unwrap().parse().unwrap();
            let length: u64 = parts.next().unwrap().parse().unwrap();
            let next_save = sequence[index + 1..]
                .iter()
                .find_map(|later| later.strip_prefix("save:"))
                .and_then(|covered| covered.parse::<u64>().ok())
                .unwrap_or(0);
            assert!(
                next_save >= offset + length,
                "write {} 后的 save {} 未覆盖写入区间",
                offset + length,
                next_save
            );
        }
    }
}

#[tokio::test]
async fn single_stream_failure_leaves_a_hole_for_the_rerun() {
    let events = events();
    let source = FakeSource::new(16);
    source.failures.lock().unwrap().insert(8);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Retryable));
    assert!(store.contains("op-1"), "失败后记录保留");
    // 清除故障重跑：只补缺失区间。
    source.failures.lock().unwrap().clear();
    let reads_before = source.reads.lock().unwrap().len();
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 16);
    let reads = source.reads.lock().unwrap().clone();
    assert_eq!(
        reads.iter().filter(|offset| **offset == 0).count(),
        1,
        "首块不得重复读取"
    );
    assert_eq!(reads.len(), reads_before + 1);
}

#[tokio::test]
async fn stop_pauses_between_chunks_and_rerun_resumes() {
    let events = events();
    let source = FakeSource::new(24);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let stop = StopToken::default();
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &stop, &|progress| {
                if progress.persisted == 8 {
                    stop.stop();
                }
            }),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Paused));
    assert_eq!(source.reads.lock().unwrap().len(), 1);
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 24);
    assert_eq!(source.reads.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn stop_with_a_complete_ledger_defers_publish() {
    let events = events();
    let source = FakeSource::new(8);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let stop = StopToken::default();
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &stop, &|_| {
                stop.stop();
            }),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Paused));
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry == "publish"),
        "停止后不得发布"
    );
    let reads_before = source.reads.lock().unwrap().len();
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 8);
    assert_eq!(
        source.reads.lock().unwrap().len(),
        reads_before,
        "重跑不得重新读取"
    );
}

#[tokio::test]
async fn early_destination_conflict_rejects_before_any_read() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    target.inner.lock().unwrap().dest_occupied = true;
    let store = SharedStore::new(&events);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Conflict));
    assert!(
        source.reads.lock().unwrap().is_empty(),
        "目标冲突必须在下载前拒绝"
    );
    assert!(store.contains("op-1"));
}

#[tokio::test]
async fn publish_failure_keeps_staging_and_rerun_only_publishes() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    target.inner.lock().unwrap().fail_publish_once = true;
    let store = SharedStore::new(&events);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Conflict));
    let fetched = source.reads.lock().unwrap().len();
    assert_eq!(fetched, 2, "数据已收齐，仅发布失败");
    assert!(events.lock().unwrap().contains(&"verify".to_string()));
    let verifies = events
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.as_str() == "verify")
        .count();
    // 重跑：不再读取、不重复校验，直接发布。
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 16);
    assert_eq!(source.reads.lock().unwrap().len(), fetched);
    let verifies_after = events
        .lock()
        .unwrap()
        .iter()
        .filter(|entry| entry.as_str() == "verify")
        .count();
    assert_eq!(verifies, verifies_after, "已校验状态不得重复校验");
}

#[tokio::test]
async fn publish_window_reconciles_the_destination() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let progress = Mutex::new(Vec::new());
    store.fail_save_with_receipt.store(true, Ordering::Release);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|update| {
                progress.lock().unwrap().push(update);
            }),
        )
        .await;
    assert!(
        matches!(result, Err(e) if e.kind == ErrorKind::Checkpoint),
        "发布成功但回执落盘失败"
    );
    assert!(
        progress
            .lock()
            .unwrap()
            .iter()
            .all(|update| !update.complete),
        "回执持久化失败不得报告完成"
    );
    // 完成对账分支同样必须先持久化回执，再发送完成进度。
    let failed_reconciliation = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|update| {
                progress.lock().unwrap().push(update);
            }),
        )
        .await;
    assert!(matches!(failed_reconciliation, Err(error) if error.kind == ErrorKind::Checkpoint));
    assert!(
        progress
            .lock()
            .unwrap()
            .iter()
            .all(|update| !update.complete)
    );
    // 重跑对账目标位置：零读取返回原回执。
    store.fail_save_with_receipt.store(false, Ordering::Release);
    let reads_before = source.reads.lock().unwrap().len();
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|update| {
                progress.lock().unwrap().push(update);
            }),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 16);
    assert_eq!(
        progress
            .lock()
            .unwrap()
            .iter()
            .filter(|update| update.complete)
            .count(),
        1
    );
    assert_eq!(source.reads.lock().unwrap().len(), reads_before);
    engine(8, 1).confirm(&store, &receipt).await.unwrap();
    assert!(!store.contains("op-1"));
}

#[tokio::test]
async fn revision_change_is_rejected_and_the_record_survives() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    // 消费者尚未 confirm，远端更新了对象：同操作重跑必须拒绝。
    *source.revision.lock().unwrap() = "rev-2".into();
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::SourceChanged));
    assert!(store.contains("op-1"));
}

#[tokio::test]
async fn digest_mismatch_keeps_staging_for_the_rerun() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    target.inner.lock().unwrap().fail_verify = true;
    let store = SharedStore::new(&events);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert!(
        !events
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry == "publish"),
        "校验失败不得发布"
    );
    let fetched = source.reads.lock().unwrap().len();
    target.inner.lock().unwrap().fail_verify = false;
    let receipt = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 16);
    assert_eq!(
        source.reads.lock().unwrap().len(),
        fetched,
        "校验失败不触发重下"
    );
}

#[tokio::test]
async fn empty_file_never_reads_and_publishes() {
    let events = events();
    let source = FakeSource::new(0);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let receipt = engine(8, 2)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 0);
    assert!(source.reads.lock().unwrap().is_empty());
    assert!(events.lock().unwrap().contains(&"publish".to_string()));
}

#[tokio::test]
async fn capability_shortfalls_are_rejected_before_any_io() {
    let events = events();
    let source = FakeSource::new(16);
    let weak_target = FakeTarget {
        support_random_write: false,
        ..FakeTarget::new(&events)
    };
    let store = SharedStore::new(&events);
    let result = engine(8, 1)
        .run(
            &source,
            &weak_target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Unsupported));
    assert!(source.reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn upload_records_cannot_drive_a_download_operation() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    let upload = waybill::checkpoint::UploadFlow {
        intent: waybill::upload::UploadIntent {
            operation: "op-1".into(),
            target: "a.bin".into(),
            conflict: ConflictPolicy::Reject,
        },
        service: ServiceIdentity {
            service: ServiceId::parse("waybill:gdrive").unwrap(),
            instance: "inst".into(),
        },
        source: waybill::source::SourceIdentity {
            reference: "/tmp/a".into(),
            revision: "r".into(),
            size: 16,
            blake3: "0".repeat(64),
        },
        acknowledged: 4,
        restarts: 0,
        driver: DriverState {
            version: 1,
            payload: vec![],
        },
        receipt: None,
    }
    .checkpoint();
    store.seed(upload);
    let result = engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("op-1"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::IdentityMismatch));
    assert!(store.contains("op-1"), "拒绝时不破坏原记录");
}

#[test]
fn v2_records_roundtrip_and_download_records_stay_distinct() {
    let flow = waybill::checkpoint::UploadFlow {
        intent: waybill::upload::UploadIntent {
            operation: "legacy".into(),
            target: "a.bin".into(),
            conflict: ConflictPolicy::Reject,
        },
        service: ServiceIdentity {
            service: ServiceId::parse("waybill:gdrive").unwrap(),
            instance: "inst".into(),
        },
        source: waybill::source::SourceIdentity {
            reference: "/tmp/a".into(),
            revision: "r".into(),
            size: 10,
            blake3: "0".repeat(64),
        },
        acknowledged: 4,
        restarts: 1,
        driver: DriverState {
            version: 1,
            payload: vec![],
        },
        receipt: None,
    };
    // v2 结构序列化携带方向标签，并可再次解码。
    let upgraded = flow.checkpoint();
    let json = serde_json::to_string(&upgraded).unwrap();
    assert!(json.contains("\"flow\":{\"kind\":\"upload\""));
    let again: Checkpoint = serde_json::from_str(&json).unwrap();
    assert_eq!(again.upload().unwrap(), &flow);
    // 下载记录解码后保持方向。
    let download = DownloadFlow {
        intent: intent("dl"),
        service: ServiceIdentity {
            service: ServiceId::parse("waybill:fs").unwrap(),
            instance: "local".into(),
        },
        source: RemoteIdentity {
            service: ServiceIdentity {
                service: ServiceId::parse("test:remote").unwrap(),
                instance: "source-account".into(),
            },
            reference: "file-1".into(),
            revision: "1:abc:4".into(),
            size: 4,
            digest: Some(Digest {
                algorithm: DigestAlgorithm::Md5,
                value: "0".repeat(32),
            }),
        },
        persisted: vec![],
        driver: DriverState {
            version: 1,
            payload: vec![],
        },
        receipt: None,
    }
    .checkpoint();
    let download_json = serde_json::to_string(&download).unwrap();
    let parsed: Checkpoint = serde_json::from_str(&download_json).unwrap();
    assert!(parsed.download().is_some());
}

#[tokio::test]
async fn matching_object_from_a_different_source_instance_cannot_resume() {
    let events = events();
    let source = FakeSource::new(16);
    let target = FakeTarget::new(&events);
    let store = SharedStore::new(&events);
    engine(8, 1)
        .run(
            &source,
            &target,
            &store,
            options(intent("namespace-bound"), &StopToken::default(), &|_| {}),
        )
        .await
        .unwrap();
    let mut other = FakeSource::new(16);
    other.instance = "different-account".into();
    // 除实例外，对象引用、版本、长度与摘要均完全一致。
    let mut original_identity = source.identity_value();
    original_identity.service.instance = other.instance.clone();
    assert_eq!(original_identity, other.identity_value());
    let result = engine(8, 1)
        .run(
            &other,
            &target,
            &store,
            options(intent("namespace-bound"), &StopToken::default(), &|_| {}),
        )
        .await;
    assert!(matches!(result, Err(error) if error.kind == ErrorKind::IdentityMismatch));
    assert!(other.reads.lock().unwrap().is_empty());
    assert!(store.contains("namespace-bound"));
}

#[tokio::test]
async fn malformed_saved_ledgers_are_rejected_before_reads_or_writes() {
    for (label, persisted) in [
        (
            "overlap",
            vec![
                Interval { start: 0, end: 8 },
                Interval { start: 4, end: 12 },
            ],
        ),
        (
            "unsorted",
            vec![
                Interval { start: 8, end: 12 },
                Interval { start: 0, end: 4 },
            ],
        ),
        ("beyond-source", vec![Interval { start: 8, end: 17 }]),
        (
            "overflow",
            vec![
                Interval {
                    start: 0,
                    end: u64::MAX,
                },
                Interval {
                    start: 1,
                    end: u64::MAX,
                },
            ],
        ),
        (
            "reversed",
            vec![Interval {
                start: u64::MAX,
                end: 0,
            }],
        ),
        ("empty", vec![Interval { start: 8, end: 8 }]),
    ] {
        let events = events();
        let source = FakeSource::new(16);
        let target = FakeTarget::new(&events);
        let store = SharedStore::new(&events);
        store.seed(
            DownloadFlow {
                intent: intent(label),
                service: target.identity(),
                source: source.identity_value(),
                persisted,
                driver: encode_state(&FakeTargetState {
                    initialized: true,
                    verified: false,
                }),
                receipt: None,
            }
            .checkpoint(),
        );
        let saved = store.records.lock().unwrap().get(label).unwrap().clone();
        let result = engine(8, 2)
            .run(
                &source,
                &target,
                &store,
                options(intent(label), &StopToken::default(), &|_| {}),
            )
            .await;
        assert!(
            matches!(result, Err(error) if error.kind == ErrorKind::Checkpoint),
            "{label}"
        );
        assert!(source.reads.lock().unwrap().is_empty(), "{label}");
        assert!(events.lock().unwrap().is_empty(), "{label}");
        assert_eq!(
            store.records.lock().unwrap().get(label).unwrap().download(),
            saved.download(),
            "{label}"
        );
    }
}
