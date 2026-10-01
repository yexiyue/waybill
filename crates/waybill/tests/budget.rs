//! 外部消费者通过上传引擎验证共享预算，而不访问其计数器。
use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll, Waker},
};
use waybill::{
    BoxFuture,
    checkpoint::{Checkpoint, CheckpointLease, CheckpointStore, DriverState},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceId, ServiceIdentity},
    source::{Source, SourceIdentity},
    upload::{
        ConflictPolicy, ResourceBudget, RunOptions, SessionStatus, StopToken, UploadEngine,
        UploadIntent, UploadPolicy, UploadSink,
    },
};
struct Pending;
impl Source for Pending {
    fn identity(&self) -> BoxFuture<'_, SourceIdentity> {
        Box::pin(std::future::pending())
    }
    fn read_range(&self, _: u64, _: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async { Err(unsupported()) })
    }
}
struct Sink;
impl UploadSink for Sink {
    fn identity(&self) -> ServiceIdentity {
        ServiceIdentity {
            service: ServiceId::parse("example:sink").unwrap(),
            instance: "test".into(),
        }
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            offset_upload: true,
            durable_upload: true,
            ..Capabilities::default()
        }
    }
    fn prepare<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState> {
        Box::pin(async { Err(unsupported()) })
    }
    fn probe<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        _: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async { Err(unsupported()) })
    }
    fn initialize<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        _: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async { Err(unsupported()) })
    }
    fn write_chunk<'a>(
        &'a self,
        _: &'a UploadIntent,
        _: &'a SourceIdentity,
        _: &'a DriverState,
        _: u64,
        _: Vec<u8>,
    ) -> BoxFuture<'a, SessionStatus> {
        Box::pin(async { Err(unsupported()) })
    }
}
struct Store;
impl CheckpointStore for Store {
    fn acquire<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>> {
        Box::pin(async { Ok(Box::new(Store) as Box<dyn CheckpointLease>) })
    }
}
impl CheckpointLease for Store {
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>> {
        Box::pin(async { Ok(None) })
    }
    fn save<'a>(&'a self, _: &'a Checkpoint) -> BoxFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn remove(&self) -> BoxFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}
fn unsupported() -> Error {
    Error::new(ErrorKind::Unsupported, "test endpoint not used")
}
#[test]
fn shared_budget_rejects_overcommit_and_releases_on_future_drop() {
    let budget = Arc::new(ResourceBudget::new(256 * 1024, 1).unwrap());
    assert_eq!(budget.max_data_bytes(), 256 * 1024);
    let engine = UploadEngine::new(budget.clone());
    let other = UploadEngine::new(budget);
    let stop = StopToken::default();
    let progress = |_| {};
    let options = || RunOptions {
        intent: UploadIntent {
            operation: "budget-test".into(),
            target: "file".into(),
            conflict: ConflictPolicy::Reject,
        },
        policy: UploadPolicy::default(),
        stop: &stop,
        progress: &progress,
    };
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut first = Box::pin(engine.run(&Pending, &Sink, &Store, options()));
    assert!(first.as_mut().poll(&mut context).is_pending());
    let mut second = Box::pin(other.run(&Pending, &Sink, &Store, options()));
    assert!(
        matches!(second.as_mut().poll(&mut context),Poll::Ready(Err(e)) if e.kind==ErrorKind::ResourceBusy)
    );
    drop(first);
    let mut third = Box::pin(other.run(&Pending, &Sink, &Store, options()));
    assert!(third.as_mut().poll(&mut context).is_pending());
}
#[test]
fn oversized_or_unaligned_budgets_are_rejected() {
    assert!(ResourceBudget::new(9 * 1024 * 1024, 2).is_err());
    assert!(ResourceBudget::new(256 * 1024, 3).is_err());
    assert!(ResourceBudget::new(1, 1).is_err());
}
#[test]
fn namespace_identifiers_are_open_and_validated() -> Result<()> {
    assert_eq!(
        ServiceId::parse("third-party:cloud-storage")?.as_str(),
        "third-party:cloud-storage"
    );
    assert!(ServiceId::parse("no-namespace").is_err());
    Ok(())
}
