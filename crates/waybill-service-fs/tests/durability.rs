//! 验证存储保证与源变化，而不是镜像实现细节。
use waybill::{
    checkpoint::{Checkpoint, CheckpointStore, DriverState, FORMAT_VERSION},
    error::ErrorKind,
    service::{ServiceId, ServiceIdentity},
    source::{Source, SourceIdentity},
    upload::{ConflictPolicy, UploadIntent},
};
use waybill_service_fs::{FileCheckpointStore, FileSource};
fn record() -> Checkpoint {
    Checkpoint {
        version: FORMAT_VERSION,
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
}
#[tokio::test]
async fn persists_permissions_and_excludes_other_processes() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let store = FileCheckpointStore::new(dir.path());
    let lease = store.acquire("lock-test").await.unwrap();
    lease.save(&record()).await.unwrap();
    assert_eq!(lease.load().await.unwrap().unwrap().acknowledged, 20);
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
    oversized.driver.payload = vec![1; 1024 * 1024 + 1];
    assert!(matches!(lease.save(&oversized).await, Err(e) if e.kind == ErrorKind::Checkpoint));
    assert_eq!(lease.load().await.unwrap().unwrap().acknowledged, 20);
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
    assert!(!format!("{record:?} {:?}", record.driver).contains("sensitive-session"));
}
