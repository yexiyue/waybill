//! 验证存储保证与源变化，而不是镜像实现细节。
use waybill::{
    checkpoint::{Checkpoint, CheckpointStore, DriverState, Flow, UploadFlow},
    error::ErrorKind,
    service::{ServiceId, ServiceIdentity},
    source::{Source, SourceIdentity},
    upload::{ConflictPolicy, UploadIntent},
};
use waybill_service_fs::{FileCheckpointStore, FileSource, decode_checkpoint};
fn record() -> Checkpoint {
    UploadFlow {
        intent: UploadIntent {
            operation: "lock-test".into(),
            target: "file.bin".into(),
            conflict: ConflictPolicy::Reject,
        },
        service: ServiceIdentity {
            service: ServiceId::parse("test:sink").unwrap(),
            instance: "account".into(),
        },
        source: SourceIdentity {
            reference: "src".into(),
            revision: "v1".into(),
            size: 42,
            blake3: "0".repeat(64),
        },
        acknowledged: 20,
        restarts: 0,
        driver: DriverState {
            version: 1,
            payload: b"sensitive-session-url".to_vec(),
        },
        receipt: None,
    }
    .checkpoint()
}
#[tokio::test]
async fn persists_permissions_and_excludes_other_processes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path());
    let lease = store.acquire("lock-test").await.unwrap();
    lease.save(&record()).await.unwrap();
    assert_eq!(
        lease
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .acknowledged,
        20
    );
    let other = FileCheckpointStore::new(dir.path());
    assert!(
        matches!(other.acquire("lock-test").await, Err(e) if e.kind == ErrorKind::OperationBusy)
    );
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lock_child", "--ignored", "--nocapture"])
        .env("WAYBILL_TEST_LOCK_ROOT", dir.path())
        .status()
        .unwrap();
    assert!(child.success());
    let path = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "json"))
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let mut oversized = record();
    let Flow::Upload(flow) = &mut oversized.flow else {
        panic!("upload flow");
    };
    flow.driver.payload = vec![1; 1024 * 1024 + 1];
    assert!(matches!(lease.save(&oversized).await, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert_eq!(
        lease
            .load()
            .await
            .unwrap()
            .unwrap()
            .upload()
            .unwrap()
            .acknowledged,
        20
    );
    drop(lease);
    let lease = other.acquire("lock-test").await.unwrap();
    std::fs::write(&path, b"{broken").unwrap();
    assert!(matches!(lease.load().await, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert!(path.exists());
    std::fs::write(&path, vec![0; 1024 * 1024 + 1]).unwrap();
    assert!(matches!(lease.load().await, Err(e) if e.kind == ErrorKind::Checkpoint));
}
#[tokio::test]
#[ignore = "helper invoked by the cross-process lock test"]
async fn lock_child() {
    let root = std::env::var("WAYBILL_TEST_LOCK_ROOT").unwrap();
    assert!(
        matches!(FileCheckpointStore::new(root).acquire("lock-test").await, Err(e) if e.kind == ErrorKind::OperationBusy)
    );
}
#[tokio::test]
async fn rejects_changed_source_and_bounded_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, b"original").unwrap();
    let source = FileSource::open(&path).await.unwrap();
    assert_eq!(source.read_range(2, 3).await.unwrap(), b"igi");
    assert!(
        matches!(source.read_range(0, 9 * 1024 * 1024).await, Err(e) if e.kind == ErrorKind::InvalidInput)
    );
    std::fs::write(&path, b"replaced").unwrap();
    assert!(matches!(source.identity().await, Err(e) if e.kind == ErrorKind::SourceChanged));
    assert!(matches!(source.read_range(0, 1).await, Err(e) if e.kind == ErrorKind::SourceChanged));
}
#[test]
fn debug_redacts_checkpoint_payload() {
    let record = record();
    let driver = record.upload().unwrap().driver.clone();
    assert!(!format!("{record:?} {driver:?}").contains("sensitive-session"));
}
/// v1 平铺上传记录（M1 保存格式）经解码包壳加载，保存后升级为 v2。
#[tokio::test]
async fn v1_flat_records_load_and_upgrade_on_save() {
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path());
    let v1 = format!(
        "{{\"version\":1,\"intent\":{{\"operation\":\"legacy\",\"target\":\"a.bin\",\
         \"conflict\":\"Reject\"}},\"service\":{{\"service\":\"waybill:gdrive\",\
         \"instance\":\"inst\"}},\"source\":{{\"reference\":\"/tmp/a\",\
         \"revision\":\"r\",\"size\":10,\"blake3\":\"{}\"}},\"acknowledged\":4,\
         \"restarts\":1,\"driver\":{{\"version\":1,\"payload\":[]}},\"receipt\":null}}",
        "0".repeat(64)
    );
    assert!(decode_checkpoint(v1.as_bytes()).is_ok());
    assert!(matches!(decode_checkpoint(b"{"), Err(e) if e.kind == ErrorKind::Checkpoint));
    // 写入 v1 原始字节：租约加载按上传流解析，保存后落盘为 v2。
    let key = blake3::hash(b"legacy").to_hex().to_string();
    std::fs::write(dir.path().join(format!("{key}.json")), v1).unwrap();
    let lease = store.acquire("legacy").await.unwrap();
    let loaded = lease.load().await.unwrap().unwrap();
    assert_eq!(loaded.version, 1);
    let flow = loaded.upload().unwrap();
    assert_eq!(flow.acknowledged, 4);
    assert_eq!(flow.restarts, 1);
    // 引擎保存经 checkpoint() 打包：v1 记录此时升级为 v2；裸 save 忠实保留原版本。
    lease.save(&flow.checkpoint()).await.unwrap();
    drop(lease);
    let raw = std::fs::read(dir.path().join(format!("{key}.json"))).unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("\"kind\":\"upload\""));
    let upgraded = decode_checkpoint(&raw).unwrap();
    assert_eq!(upgraded.version, waybill::checkpoint::FORMAT_VERSION);
    assert_eq!(upgraded.upload().unwrap(), flow);
}
