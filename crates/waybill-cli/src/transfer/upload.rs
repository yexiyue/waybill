//! 上传队列编排；不感知终端形态。
use super::{
    Event, Failure, Outcome, complete_queue, emit_progress, failure, operation_id, output_closed,
};
use std::path::PathBuf;
use tokio::sync::mpsc;
use waybill::{
    error::ErrorKind,
    source::Source,
    upload::{ConflictPolicy, RunOptions, StopToken, UploadEngine, UploadIntent},
};
use waybill_service_fs::{FileCheckpointStore, FileSource};
/// 一个待投递文件：本地路径与解析后的远端目标。
pub(crate) struct UploadJob {
    pub path: PathBuf,
    pub name: String,
    pub target: String,
}

/// 编排资源由一个队列拥有，输出通过有界通道交付。
pub(crate) struct UploadRunner {
    pub sink: std::sync::Arc<dyn waybill::upload::UploadSink>,
    pub store: FileCheckpointStore,
    pub engine: UploadEngine,
    pub stop: StopToken,
    pub conflict: ConflictPolicy,
    pub events: mpsc::Sender<Event>,
}

impl UploadRunner {
    /// 顺序投递全部任务；事件发完（含 Done）后返回。
    pub(crate) async fn run(
        self,
        jobs: Vec<UploadJob>,
        operation_override: Option<String>,
    ) -> Outcome {
        let mut receipts = 0;
        let mut failures = 0;
        let mut stopped = false;
        for (index, job) in jobs.into_iter().enumerate() {
            if self.stop.is_stopped() || self.events.is_closed() {
                stopped = true;
                break;
            }
            match self
                .deliver(index, &job, operation_override.as_deref())
                .await
            {
                Ok(()) => receipts += 1,
                Err(error) => {
                    let kind = error.kind;
                    error.emit(&self.events, index, job.name.clone()).await;
                    failures += 1;
                    if kind == ErrorKind::Paused {
                        stopped = true;
                        break;
                    }
                    if kind == ErrorKind::Authentication {
                        // 凭证失败不是用户暂停；保留失败退出语义。
                        break;
                    }
                }
            }
        }
        complete_queue(&self.events, receipts, failures, stopped).await
    }

    async fn deliver(
        &self,
        index: usize,
        job: &UploadJob,
        operation_override: Option<&str>,
    ) -> Result<(), Failure> {
        let source = FileSource::open(&job.path).await.map_err(|e| failure(&e))?;
        let identity = source.identity().await.map_err(|e| failure(&e))?;
        let operation = operation_override.map(str::to_string).unwrap_or_else(|| {
            derive_operation(&self.sink.identity(), &job.target, &identity.blake3)
        });
        self.events
            .send(Event::Started {
                index,
                name: job.name.clone(),
                target: job.target.clone(),
                size: identity.size,
                operation: operation.clone(),
            })
            .await
            .map_err(|_| output_closed())?;
        let receipt = self
            .engine
            .run(
                &source,
                self.sink.as_ref(),
                &self.store,
                RunOptions {
                    intent: UploadIntent {
                        operation,
                        target: job.target.clone(),
                        conflict: self.conflict,
                    },
                    policy: Default::default(),
                    stop: &self.stop,
                    progress: &|p| {
                        emit_progress(
                            &self.events,
                            &self.stop,
                            Event::Progress {
                                index,
                                persisted: p.persisted,
                                sent: p.sent,
                                total: p.total,
                                epoch: p.epoch,
                                complete: p.complete,
                            },
                        );
                    },
                },
            )
            .await
            .map_err(|e| failure(&e))?;
        self.events
            .send(Event::Completed { index, receipt })
            .await
            .map_err(|_| output_closed())?;
        Ok(())
    }
}

/// 默认操作 ID：实例、目标与内容摘要联合派生。
///
/// 同一命令重跑得到同一操作（续传或回执幂等）；文件改动即新运单。
fn derive_operation(
    service: &waybill::service::ServiceIdentity,
    target: &str,
    digest: &str,
) -> String {
    operation_id(&[
        "upload",
        service.service.as_str(),
        &service.instance,
        target,
        digest,
    ])
}

#[cfg(test)]
mod tests {
    use super::derive_operation;
    fn service(instance: &str) -> waybill::service::ServiceIdentity {
        waybill::service::ServiceIdentity {
            service: waybill::service::ServiceId::parse("test:drive").unwrap(),
            instance: instance.into(),
        }
    }

    #[test]
    fn operation_is_stable_and_bound_to_identity() {
        let instance = service("instance");
        let other = service("other");
        let a = derive_operation(&instance, "backup/a.iso", &"0".repeat(64));
        assert_eq!(
            a,
            derive_operation(&instance, "backup/a.iso", &"0".repeat(64))
        );
        assert_ne!(
            a,
            derive_operation(&instance, "backup/b.iso", &"0".repeat(64))
        );
        assert_ne!(a, derive_operation(&other, "backup/a.iso", &"0".repeat(64)));
        assert!(a.starts_with("wb-") && a.len() == 19);
    }

    use super::{Event, UploadJob, UploadRunner};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use waybill::{
        BoxFuture,
        budget::ResourceBudget,
        checkpoint::DriverState,
        error::{Error, ErrorKind},
        service::{Capabilities, ServiceId, ServiceIdentity},
        source::SourceIdentity,
        upload::{
            ConflictPolicy, Receipt, SessionStatus, StopToken, UploadEngine, UploadIntent,
            UploadSink,
        },
    };
    use waybill_service_fs::FileCheckpointStore;

    struct CompletedSink {
        prepares: AtomicUsize,
        authentication_failure: bool,
    }

    impl UploadSink for CompletedSink {
        fn identity(&self) -> ServiceIdentity {
            ServiceIdentity {
                service: ServiceId::parse("test:drive").unwrap(),
                instance: "account".into(),
            }
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities {
                offset_upload: true,
                durable_upload: true,
                ..Default::default()
            }
        }
        fn prepare<'a>(
            &'a self,
            _: &'a UploadIntent,
            _: &'a SourceIdentity,
        ) -> BoxFuture<'a, DriverState> {
            Box::pin(async move {
                self.prepares.fetch_add(1, Ordering::Relaxed);
                if self.authentication_failure {
                    Err(Error::new(ErrorKind::Authentication, "reconnect"))
                } else {
                    Ok(DriverState {
                        version: 1,
                        payload: vec![1],
                    })
                }
            })
        }
        fn probe<'a>(
            &'a self,
            intent: &'a UploadIntent,
            source: &'a SourceIdentity,
            state: &'a DriverState,
        ) -> BoxFuture<'a, SessionStatus> {
            Box::pin(async move {
                Ok(SessionStatus::Complete {
                    state: state.clone(),
                    receipt: Receipt {
                        operation: intent.operation.clone(),
                        service: self.identity(),
                        target: intent.target.clone(),
                        object: "original-object".into(),
                        size: source.size,
                        verified: Default::default(),
                    },
                })
            })
        }
        fn initialize<'a>(
            &'a self,
            _: &'a UploadIntent,
            _: &'a SourceIdentity,
            _: &'a DriverState,
        ) -> BoxFuture<'a, SessionStatus> {
            Box::pin(async { panic!("completed upload must not initialize") })
        }
        fn write_chunk<'a>(
            &'a self,
            _: &'a UploadIntent,
            _: &'a SourceIdentity,
            _: &'a DriverState,
            _: u64,
            _: Vec<u8>,
        ) -> BoxFuture<'a, SessionStatus> {
            Box::pin(async { panic!("completed upload must not retransmit") })
        }
    }

    async fn queue(
        sink: Arc<CompletedSink>,
        dir: &std::path::Path,
        stop: StopToken,
    ) -> (super::Outcome, Vec<Event>) {
        let (events, mut receiver) = tokio::sync::mpsc::channel(64);
        let runner = UploadRunner {
            sink,
            store: FileCheckpointStore::new(dir.join("checkpoints")),
            engine: UploadEngine::new(Arc::new(ResourceBudget::default())),
            stop,
            conflict: ConflictPolicy::Reject,
            events,
        };
        let outcome = runner
            .run(
                vec![UploadJob {
                    path: dir.join("source"),
                    name: "source".into(),
                    target: "target".into(),
                }],
                None,
            )
            .await;
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        (outcome, events)
    }

    #[tokio::test]
    async fn repeated_put_keeps_completed_identity() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source"), b"content").unwrap();
        let sink = Arc::new(CompletedSink {
            prepares: AtomicUsize::new(0),
            authentication_failure: false,
        });
        for _ in 0..2 {
            let (outcome, events) = queue(sink.clone(), dir.path(), StopToken::default()).await;
            assert_eq!(outcome.failures, 0);
            assert!(events.iter().any(|event| matches!(event, Event::Completed { receipt, .. } if receipt.object == "original-object")));
        }
        assert_eq!(sink.prepares.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn stop_before_first_file_is_not_successful_completion() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(CompletedSink {
            prepares: AtomicUsize::new(0),
            authentication_failure: false,
        });
        let stop = StopToken::default();
        stop.stop();
        let (outcome, events) = queue(sink.clone(), dir.path(), stop).await;
        assert!(outcome.stopped);
        assert!(matches!(
            events.as_slice(),
            [Event::Done {
                receipts: 0,
                failures: 0,
                stopped: true
            }]
        ));
        assert_eq!(sink.prepares.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn authentication_failure_is_not_user_pause() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source"), b"content").unwrap();
        let sink = Arc::new(CompletedSink {
            prepares: AtomicUsize::new(0),
            authentication_failure: true,
        });
        let (outcome, events) = queue(sink, dir.path(), StopToken::default()).await;
        assert_eq!(outcome.failures, 1);
        assert!(!outcome.stopped);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Failed { paused: false, .. }))
        );
    }
}
