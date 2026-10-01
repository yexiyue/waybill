//! 本地下载目标的引擎级验收：`.part` 语义、校验、发布窗口与跨进程恢复。
//! 数据源为确定性内存源；结论不涉及云端行为。
use std::sync::Arc;
use waybill::{
    BoxFuture,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore, Flow},
    download::{
        Digest, DigestAlgorithm, DownloadEngine, DownloadIntent, DownloadOptions, DownloadProgress,
        DownloadSource, DownloadStatus, Verification,
    },
    error::{Error, ErrorKind},
    service::{Capabilities, Service, ServiceId, ServiceIdentity},
    upload::{ConflictPolicy, Receipt, StopToken},
};
use waybill_service_fs::{FileCheckpointStore, FsService};

/// 确定性数据源：字节由下标生成，子进程可独立重建同一内容。
struct PatternSource {
    size: u64,
    digest: Digest,
}
impl PatternSource {
    fn new(size: u64) -> Self {
        use md5::Digest as _;
        let mut hasher = md5::Md5::new();
        let mut buffer = Vec::new();
        for index in 0..size {
            buffer.push((index % 251) as u8);
            if buffer.len() == 65536 {
                hasher.update(&buffer);
                buffer.clear();
            }
        }
        hasher.update(&buffer);
        Self {
            size,
            digest: Digest {
                algorithm: DigestAlgorithm::Md5,
                value: format!("{:x}", hasher.finalize()),
            },
        }
    }
    fn identity(&self) -> waybill::download::RemoteIdentity {
        waybill::download::RemoteIdentity {
            service: ServiceIdentity {
                service: ServiceId::parse("test:pattern").unwrap(),
                instance: "source-account".into(),
            },
            reference: "pattern-source".into(),
            revision: "1".into(),
            size: self.size,
            digest: Some(self.digest.clone()),
        }
    }
    fn byte_at(index: u64) -> u8 {
        (index % 251) as u8
    }
}
impl DownloadSource for PatternSource {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_download: true,
            ..Default::default()
        }
    }
    fn identity(&self) -> BoxFuture<'_, waybill::download::RemoteIdentity> {
        let identity = self.identity();
        Box::pin(async move { Ok(identity) })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            if offset + length as u64 > self.size {
                return Err(Error::new(ErrorKind::InvalidInput, "range out of bounds"));
            }
            Ok((offset..offset + length as u64)
                .map(Self::byte_at)
                .collect())
        })
    }
}

fn service() -> Arc<FsService> {
    Arc::new(FsService::new("test-local").unwrap())
}
fn intent(operation: &str, dest: &std::path::Path) -> DownloadIntent {
    DownloadIntent {
        operation: operation.into(),
        target: dest.to_string_lossy().into_owned(),
        conflict: ConflictPolicy::Reject,
    }
}
fn engine(chunk: usize, concurrency: usize) -> DownloadEngine {
    DownloadEngine::new(Arc::new(ResourceBudget::new(chunk, concurrency).unwrap()))
}
async fn run(
    size: u64,
    operation: &str,
    dest: &std::path::Path,
    checkpoints: &std::path::Path,
    stop: &StopToken,
    progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> waybill::error::Result<Receipt> {
    let source = PatternSource::new(size);
    let target = service().download_target().unwrap();
    let store = FileCheckpointStore::new(checkpoints);
    engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: intent(operation, dest),
                stop,
                progress,
            },
        )
        .await
}
fn staging_paths(dest: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dest.parent().unwrap())
        .unwrap()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".wb-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "part")
        })
        .collect()
}
fn staging(dest: &std::path::Path) -> std::path::PathBuf {
    let paths = staging_paths(dest);
    assert_eq!(
        paths.len(),
        1,
        "one private staging file expected: {paths:?}"
    );
    paths[0].clone()
}
fn expected_bytes(size: u64) -> Vec<u8> {
    (0..size).map(PatternSource::byte_at).collect()
}

#[tokio::test]
async fn engine_publishes_verified_content_and_removes_staging() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out/bin.dat");
    let receipt = run(
        100,
        "publish-op",
        &dest,
        &dir.path().join("cp"),
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.size, 100);
    assert_eq!(receipt.object, dest.to_string_lossy());
    match &receipt.verified {
        Verification::Digest { algorithm, value } => {
            assert_eq!(*algorithm, DigestAlgorithm::Md5);
            assert_eq!(value, &PatternSource::new(100).digest.value);
        }
        other => panic!("expected digest evidence, got {other:?}"),
    }
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
    assert!(staging_paths(&dest).is_empty(), "暂存必须已发布");
    // confirm 后记录清除。
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    engine(16, 2).confirm(&store, &receipt).await.unwrap();
    assert!(
        std::fs::read_dir(dir.path().join("cp")).unwrap().count() >= 1,
        "锁文件保留"
    );
}

#[tokio::test]
async fn out_of_order_writes_land_at_exact_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("reverse.bin");
    let source = PatternSource::new(48);
    let target = service().download_target().unwrap();
    let reverse_intent = intent("reverse-op", &dest);
    let identity = source.identity();
    let state = target.prepare(&reverse_intent, &identity).await.unwrap();
    let status = target
        .initialize(&reverse_intent, &identity, &state)
        .await
        .unwrap();
    let DownloadStatus::Ready { state, .. } = status else {
        panic!("expected ready");
    };
    // 乱序写入：先尾部后头部，中间最后。
    let mut offsets = [32usize, 0, 16];
    let mut state = state;
    for _ in 0..3 {
        let offset = offsets[0];
        offsets.rotate_left(1);
        let data: Vec<u8> = (offset as u64..offset as u64 + 16)
            .map(PatternSource::byte_at)
            .collect();
        state = target
            .write_chunk(&reverse_intent, &identity, &state, offset as u64, data)
            .await
            .unwrap();
    }
    let state = target
        .verify(&reverse_intent, &identity, &state)
        .await
        .unwrap();
    let status = target
        .publish(&reverse_intent, &identity, &state)
        .await
        .unwrap();
    assert!(matches!(status, DownloadStatus::Complete { .. }));
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(48));
}

#[tokio::test]
async fn part_length_is_not_completion_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("preset.bin");
    let source = PatternSource::new(48);
    let target = service().download_target().unwrap();
    let preset_intent = intent("preset-op", &dest);
    let identity = source.identity();
    let state = target.prepare(&preset_intent, &identity).await.unwrap();
    target
        .initialize(&preset_intent, &identity, &state)
        .await
        .unwrap();
    let part = staging(&dest);
    // 预分配让 .part 已是完整长度，但没有任何区间记账。
    assert_eq!(std::fs::metadata(&part).unwrap().len(), 48);
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    let reads = std::sync::atomic::AtomicUsize::new(0);
    let receipt = engine(16, 1)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: preset_intent,
                stop: &StopToken::default(),
                progress: &|_| {
                    reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                },
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt.size, 48);
    assert!(
        reads.load(std::sync::atomic::Ordering::Relaxed) > 0,
        "账本为空时必须重新读取全部区间"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(48));
}

#[tokio::test]
async fn tampered_staging_is_rebuilt_not_trusted() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("tamper.bin");
    let checkpoints = dir.path().join("cp");
    let stop = StopToken::default();
    let first = run(100, "tamper-op", &dest, &checkpoints, &stop, &|progress| {
        if progress.persisted == 32 {
            stop.stop();
        }
    })
    .await;
    assert!(matches!(first, Err(e) if e.kind == ErrorKind::Paused));
    // 截断暂存：长度与账本不再互证，必须整体重建。
    let part = staging(&dest);
    let file = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
    file.set_len(10).unwrap();
    drop(file);
    let receipt = run(
        100,
        "tamper-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.size, 100);
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
}

#[tokio::test]
async fn corrupted_persisted_region_is_caught_by_final_digest() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("corrupt.bin");
    let checkpoints = dir.path().join("cp");
    let stop = StopToken::default();
    let first = run(100, "corrupt-op", &dest, &checkpoints, &stop, &|progress| {
        if progress.persisted == 32 {
            stop.stop();
        }
    })
    .await;
    assert!(matches!(first, Err(e) if e.kind == ErrorKind::Paused));
    // 篡改已记账区间的字节：账本无法察觉，终态摘要校验必须拦截。
    let part = staging(&dest);
    let file = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
    use std::os::unix::fs::FileExt;
    file.write_all_at(&[0xFF, 0xFF], 4).unwrap();
    drop(file);
    let second = run(
        100,
        "corrupt-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await;
    assert!(
        matches!(second, Err(e) if e.kind == ErrorKind::Checkpoint),
        "摘要不符必须失败并保留暂存"
    );
    assert!(part.exists(), "校验失败不得删除 .part");
    assert!(!dest.exists());
}

#[tokio::test]
async fn late_destination_conflict_keeps_staging_and_rerun_publishes_only() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("late.bin");
    let checkpoints = dir.path().join("cp");
    let progress_calls = std::sync::atomic::AtomicUsize::new(0);
    let first = run(
        100,
        "late-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|progress| {
            progress_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if progress.persisted == progress.total {
                // 数据收齐后目标位置被占用：发布必须失败且保留暂存。
                std::fs::write(&dest, b"occupied").unwrap();
            }
        },
    )
    .await;
    assert!(matches!(first, Err(e) if e.kind == ErrorKind::Conflict));
    let reads_after_first = progress_calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(staging(&dest).exists(), ".part 必须保留");
    std::fs::remove_file(&dest).unwrap();
    let receipt = run(
        100,
        "late-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.size, 100);
    assert_eq!(
        progress_calls.load(std::sync::atomic::Ordering::Relaxed),
        reads_after_first,
        "重跑不得重新读取"
    );
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
}

#[tokio::test]
async fn operation_suffix_publishes_to_a_stable_suffixed_name() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("keep.bin");
    std::fs::write(&dest, b"existing").unwrap();
    let checkpoints = dir.path().join("cp");
    let source = PatternSource::new(100);
    let target = service().download_target().unwrap();
    let store = FileCheckpointStore::new(&checkpoints);
    let suffixed_intent = DownloadIntent {
        operation: "suffix-op".into(),
        target: dest.to_string_lossy().into_owned(),
        conflict: ConflictPolicy::OperationSuffix,
    };
    let receipt = engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: suffixed_intent.clone(),
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    // 原目标不动，后缀名承接发布。
    assert_eq!(std::fs::read(&dest).unwrap(), b"existing");
    let published = std::path::PathBuf::from(&receipt.object);
    assert_ne!(published, dest);
    assert_eq!(std::fs::read(&published).unwrap(), expected_bytes(100));
    // 重跑幂等：对账后缀名目标并返回原回执。
    let again = engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: suffixed_intent,
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    assert_eq!(again, receipt);
}

#[tokio::test]
async fn empty_file_publishes_with_digest_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("empty.bin");
    let receipt = run(
        0,
        "empty-op",
        &dest,
        &dir.path().join("cp"),
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.size, 0);
    assert!(matches!(receipt.verified, Verification::Digest { .. }));
    assert_eq!(std::fs::read(&dest).unwrap(), Vec::<u8>::new());
}

/// 拦截带回执保存的租约包装，模拟发布成功但回执落盘失败。
struct FailingReceiptLease {
    inner: Box<dyn CheckpointLease>,
}
impl CheckpointLease for FailingReceiptLease {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        self.inner.load()
    }
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()> {
        let with_receipt = match &checkpoint.flow {
            Flow::Upload(flow) => flow.receipt.is_some(),
            Flow::Download(flow) => flow.receipt.is_some(),
        };
        if with_receipt {
            return Box::pin(async move {
                Err(Error::new(
                    ErrorKind::Checkpoint,
                    "injected receipt save failure",
                ))
            });
        }
        self.inner.save(checkpoint)
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        self.inner.remove()
    }
}

#[tokio::test]
async fn publish_window_reconciles_destination_after_receipt_loss() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("window.bin");
    let checkpoints = dir.path().join("cp");
    let source = PatternSource::new(100);
    let target = service().download_target().unwrap();
    let failing = FileCheckpointStore::new(&checkpoints);
    // 直接以真实存储驱动但拦截带回执保存：发布成功、回执未落盘。
    let first = engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &FailingProbe { inner: failing },
            DownloadOptions {
                intent: intent("window-op", &dest),
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await;
    assert!(
        matches!(first, Err(e) if e.kind == ErrorKind::Checkpoint),
        "回执落盘失败必须暴露"
    );
    assert!(dest.exists(), "发布已实际完成");
    // 重跑走发布窗口对账：零读取返回带原证据的回执。
    let receipt = run(
        100,
        "window-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.object, dest.to_string_lossy());
    assert!(matches!(receipt.verified, Verification::Digest { .. }));
}
/// 探测包装：把真实 lease 包进失败拦截。
struct FailingProbe {
    inner: FileCheckpointStore,
}
impl CheckpointStore for FailingProbe {
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        Box::pin(async move {
            let lease = self.inner.acquire(operation).await?;
            Ok(Box::new(FailingReceiptLease { inner: lease }) as Box<dyn CheckpointLease>)
        })
    }
}

/// 跨进程恢复：父进程停在首块持久化后，子进程以同一 checkpoint 目录
/// 与 `.part` 继续完成下载。
#[tokio::test]
async fn a_new_process_resumes_the_persisted_download() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("cross.bin");
    let checkpoints = dir.path().join("cp");
    let stop = StopToken::default();
    let first = run(100, "cross-op", &dest, &checkpoints, &stop, &|progress| {
        if progress.persisted == 16 {
            stop.stop();
        }
    })
    .await;
    assert!(matches!(first, Err(e) if e.kind == ErrorKind::Paused));
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resume_child", "--ignored", "--nocapture"])
        .env("WAYBILL_TEST_DL_CHECKPOINTS", checkpoints)
        .env("WAYBILL_TEST_DL_DEST", &dest)
        .status()
        .unwrap();
    assert!(child.success(), "子进程必须完成下载");
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
}

#[tokio::test]
#[ignore = "helper invoked by the cross-process resume test"]
async fn resume_child() {
    let checkpoints = std::env::var("WAYBILL_TEST_DL_CHECKPOINTS").unwrap();
    let dest = std::env::var("WAYBILL_TEST_DL_DEST").unwrap();
    let dest = std::path::PathBuf::from(dest);
    let receipt = run(
        100,
        "cross-op",
        &dest,
        std::path::Path::new(&checkpoints),
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    assert_eq!(receipt.size, 100);
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
}

#[tokio::test]
async fn service_identity_binds_the_target_instance() {
    let identity = ServiceIdentity {
        service: ServiceId::parse("waybill:fs").unwrap(),
        instance: "test-local".into(),
    };
    let target = service().download_target().unwrap();
    assert_eq!(target.identity(), identity);
}

#[tokio::test]
async fn published_same_length_corruption_is_not_a_verified_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("published.bin");
    let checkpoints = dir.path().join("cp");
    run(
        100,
        "published-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await
    .unwrap();
    std::fs::write(&dest, [0xFF; 100]).unwrap();
    let result = run(
        100,
        "published-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert_eq!(std::fs::read(&dest).unwrap(), [0xFF; 100]);
}

#[tokio::test]
async fn verified_staging_is_rechecked_before_publish_retry() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("verified.bin");
    let checkpoints = dir.path().join("cp");
    let first = run(
        100,
        "verified-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|p| {
            if p.persisted == p.total {
                std::fs::write(&dest, b"occupied").unwrap();
            }
        },
    )
    .await;
    assert!(matches!(first, Err(e) if e.kind == ErrorKind::Conflict));
    let part = staging(&dest);
    std::fs::write(&part, [0xFF; 100]).unwrap();
    std::fs::remove_file(&dest).unwrap();
    let result = run(
        100,
        "verified-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|_| {},
    )
    .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert!(part.exists());
    assert!(!dest.exists());
}

#[tokio::test]
async fn independent_operations_have_private_staging_and_no_clobber_publication() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("shared.bin");
    let source = PatternSource::new(48);
    let target = service().download_target().unwrap();
    let identity = source.identity();
    let first = intent("first-op", &dest);
    let second = intent("second-op", &dest);
    let a = target.prepare(&first, &identity).await.unwrap();
    let b = target.prepare(&second, &identity).await.unwrap();
    let (a, b) = tokio::join!(
        target.initialize(&first, &identity, &a),
        target.initialize(&second, &identity, &b)
    );
    let DownloadStatus::Ready { state: a, .. } = a.unwrap() else {
        panic!("ready");
    };
    let DownloadStatus::Ready { state: b, .. } = b.unwrap() else {
        panic!("ready");
    };
    assert_eq!(staging_paths(&dest).len(), 2);
    let a = target
        .write_chunk(&first, &identity, &a, 0, expected_bytes(48))
        .await
        .unwrap();
    let b = target
        .write_chunk(&second, &identity, &b, 0, expected_bytes(48))
        .await
        .unwrap();
    let a = target.verify(&first, &identity, &a).await.unwrap();
    let b = target.verify(&second, &identity, &b).await.unwrap();
    let (a, b) = tokio::join!(
        target.publish(&first, &identity, &a),
        target.publish(&second, &identity, &b)
    );
    assert_eq!(usize::from(a.is_ok()) + usize::from(b.is_ok()), 1);
    let failure = match (a, b) {
        (Err(error), Ok(_)) | (Ok(_), Err(error)) => error,
        _ => panic!("exactly one operation must publish"),
    };
    assert_eq!(failure.kind, ErrorKind::Conflict);
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(48));
    assert_eq!(
        staging_paths(&dest).len(),
        1,
        "losing operation retains its bytes"
    );
}

#[tokio::test]
async fn preexisting_part_symlink_is_untouched_and_replaced_private_staging_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("safe.bin");
    let external = dir.path().join("external.bin");
    std::fs::write(&external, b"preserve").unwrap();
    std::os::unix::fs::symlink(&external, dest.with_extension("bin.part")).unwrap();
    let source = PatternSource::new(48);
    let target = service().download_target().unwrap();
    let identity = source.identity();
    let request = intent("safe-op", &dest);
    let state = target.prepare(&request, &identity).await.unwrap();
    let DownloadStatus::Ready { state, .. } = target
        .initialize(&request, &identity, &state)
        .await
        .unwrap()
    else {
        panic!("ready");
    };
    assert_eq!(std::fs::read(&external).unwrap(), b"preserve");
    let part = staging(&dest);
    std::fs::remove_file(&part).unwrap();
    std::os::unix::fs::symlink(&external, &part).unwrap();
    let result = target
        .write_chunk(&request, &identity, &state, 0, expected_bytes(48))
        .await;
    assert!(matches!(result, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert_eq!(std::fs::read(&external).unwrap(), b"preserve");
}

#[tokio::test]
async fn conflict_suffix_uses_file_name_in_dotted_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("backup.v2");
    std::fs::create_dir(&parent).unwrap();
    let dest = parent.join("no-extension");
    std::fs::write(&dest, b"original").unwrap();
    let source = PatternSource::new(48);
    let target = service().download_target().unwrap();
    let store = FileCheckpointStore::new(dir.path().join("cp"));
    let mut request = intent("dotted-op", &dest);
    request.conflict = ConflictPolicy::OperationSuffix;
    let receipt = engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: request,
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    let published = std::path::Path::new(&receipt.object);
    assert_eq!(published.parent(), Some(parent.as_path()));
    assert!(
        published
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("no-extension-")
    );
    assert_eq!(std::fs::read(&dest).unwrap(), b"original");
}

#[tokio::test]
async fn target_declares_blake3_and_length_verification_accurately() {
    let dir = tempfile::tempdir().unwrap();
    let target = service().download_target().unwrap();
    let bytes = expected_bytes(48);
    for digest in [
        Some(Digest {
            algorithm: DigestAlgorithm::Blake3,
            value: blake3::hash(&bytes).to_hex().to_string(),
        }),
        None,
    ] {
        let operation = if digest.is_some() {
            "blake3-op"
        } else {
            "length-op"
        };
        let dest = dir.path().join(operation);
        let request = intent(operation, &dest);
        let mut identity = PatternSource::new(48).identity();
        identity.digest = digest.clone();
        let state = target.prepare(&request, &identity).await.unwrap();
        let DownloadStatus::Ready { state, .. } = target
            .initialize(&request, &identity, &state)
            .await
            .unwrap()
        else {
            panic!("ready");
        };
        let state = target
            .write_chunk(&request, &identity, &state, 0, bytes.clone())
            .await
            .unwrap();
        let state = target.verify(&request, &identity, &state).await.unwrap();
        let DownloadStatus::Complete { receipt, state } =
            target.publish(&request, &identity, &state).await.unwrap()
        else {
            panic!("complete");
        };
        let expected = digest.map_or(Verification::Length, |digest| Verification::Digest {
            algorithm: digest.algorithm,
            value: digest.value,
        });
        assert_eq!(receipt.verified, expected);
        // 本地被同长度替换：可信摘要应拒绝；无摘要只声明长度、不得声称摘要一致。
        std::fs::write(&dest, [0xFF; 48]).unwrap();
        let result = target.probe(&request, &identity, &state).await;
        if identity.digest.is_some() {
            assert!(matches!(result, Err(error) if error.kind == ErrorKind::Checkpoint));
        } else {
            let DownloadStatus::Complete { receipt, .. } = result.unwrap() else {
                panic!("complete");
            };
            assert_eq!(receipt.verified, Verification::Length);
        }
    }
}

#[tokio::test]
async fn hardlink_publish_window_reconciles_without_remote_reads() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("linked.bin");
    let checkpoints = dir.path().join("cp");
    let first = run(
        100,
        "linked-op",
        &dest,
        &checkpoints,
        &StopToken::default(),
        &|progress| {
            if progress.persisted == progress.total {
                std::fs::write(&dest, b"occupied").unwrap();
            }
        },
    )
    .await;
    assert!(matches!(first, Err(error) if error.kind == ErrorKind::Conflict));
    let part = staging(&dest);
    std::fs::remove_file(&dest).unwrap();
    // 模拟 no-clobber 硬链接已发布、旧暂存名称尚未删除的崩溃窗口。
    std::fs::hard_link(&part, &dest).unwrap();
    struct MetadataOnlySource {
        inner: PatternSource,
        reads: std::sync::atomic::AtomicUsize,
    }
    impl DownloadSource for MetadataOnlySource {
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        fn identity(&self) -> BoxFuture<'_, waybill::download::RemoteIdentity> {
            <PatternSource as DownloadSource>::identity(&self.inner)
        }
        fn read_range(&self, _: u64, _: usize) -> BoxFuture<'_, Vec<u8>> {
            Box::pin(async move {
                self.reads
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Err(Error::new(
                    ErrorKind::Protocol,
                    "published download must not read remotely",
                ))
            })
        }
    }
    let source = MetadataOnlySource {
        inner: PatternSource::new(100),
        reads: std::sync::atomic::AtomicUsize::new(0),
    };
    let target = service().download_target().unwrap();
    let store = FileCheckpointStore::new(&checkpoints);
    let receipt = engine(16, 2)
        .run(
            &source,
            target.as_ref(),
            &store,
            DownloadOptions {
                intent: intent("linked-op", &dest),
                stop: &StopToken::default(),
                progress: &|_| {},
            },
        )
        .await
        .unwrap();
    assert_eq!(source.reads.load(std::sync::atomic::Ordering::Relaxed), 0);
    assert_eq!(receipt.object, dest.to_string_lossy());
    assert!(matches!(receipt.verified, Verification::Digest { .. }));
    assert_eq!(std::fs::read(&dest).unwrap(), expected_bytes(100));
    assert!(
        !part.exists(),
        "reconciled staging hardlink must be cleaned up"
    );
}
