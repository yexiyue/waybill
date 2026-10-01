//! 单文件下载编排；源与目标操作身份均由公开契约提供。
use super::{
    Event, Failure, Outcome, complete_queue, emit_progress, failure, operation_id, output_closed,
};
use tokio::sync::mpsc;
use waybill::{
    error::ErrorKind,
    upload::{ConflictPolicy, StopToken},
};
use waybill_service_fs::FileCheckpointStore;
/// 单个待取回文件：解析后的云对象与本地目标。
pub(crate) struct DownloadJob {
    pub source: std::sync::Arc<dyn waybill::download::DownloadSource>,
    /// 远端文件名，用于展示与目录展开。
    pub name: String,
    /// 本地目标文件路径。
    pub target: String,
}

/// 下载编排资源；事件复用上传队列的形态。
pub(crate) struct DownloadRunner {
    pub target: std::sync::Arc<dyn waybill::download::DownloadTarget>,
    pub store: FileCheckpointStore,
    pub engine: waybill::download::DownloadEngine,
    pub stop: StopToken,
    pub conflict: ConflictPolicy,
    pub events: mpsc::Sender<Event>,
}

impl DownloadRunner {
    /// 顺序取回选择集合；进度语义为「已读取 / 已持久化区间 / 总长」。
    pub(crate) async fn run(
        self,
        jobs: Vec<DownloadJob>,
        operation_override: Option<String>,
    ) -> Outcome {
        let mut receipts = 0;
        let mut failures = 0;
        let mut stopped = self.stop.is_stopped() || self.events.is_closed();
        if stopped {
            return complete_queue(&self.events, receipts, failures, stopped).await;
        }
        for (index, job) in jobs.iter().enumerate() {
            if self.stop.is_stopped() || self.events.is_closed() {
                stopped = true;
                break;
            }
            match self
                .deliver(index, job, operation_override.as_deref())
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
                }
            }
        }
        stopped |= self.stop.is_stopped();
        complete_queue(&self.events, receipts, failures, stopped).await
    }

    async fn deliver(
        &self,
        index: usize,
        job: &DownloadJob,
        operation_override: Option<&str>,
    ) -> Result<(), Failure> {
        let identity = job.source.identity().await.map_err(|e| failure(&e))?;
        let operation = operation_override.map(str::to_string).unwrap_or_else(|| {
            derive_download_operation(
                &identity.service,
                &self.target.identity(),
                &identity.reference,
                &identity.revision,
                &job.target,
            )
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
                job.source.as_ref(),
                self.target.as_ref(),
                &self.store,
                waybill::download::DownloadOptions {
                    intent: waybill::download::DownloadIntent {
                        operation,
                        target: job.target.clone(),
                        conflict: self.conflict,
                    },
                    stop: &self.stop,
                    progress: &|p| {
                        emit_progress(
                            &self.events,
                            &self.stop,
                            Event::Progress {
                                index,
                                persisted: p.persisted,
                                sent: p.read,
                                total: p.total,
                                epoch: 0,
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

/// 下载默认操作 ID：实例、云对象引用、源版本与本地目标联合派生。
///
/// 远端更新即新运单；同版本重跑得到同一操作（续传或回执幂等）。
fn derive_download_operation(
    source: &waybill::service::ServiceIdentity,
    target_service: &waybill::service::ServiceIdentity,
    reference: &str,
    revision: &str,
    target: &str,
) -> String {
    operation_id(&[
        "download",
        source.service.as_str(),
        &source.instance,
        target_service.service.as_str(),
        &target_service.instance,
        reference,
        revision,
        target,
    ])
}

#[cfg(test)]
mod tests {
    use super::derive_download_operation;
    use waybill::service::{ServiceId, ServiceIdentity};
    fn service(instance: &str) -> ServiceIdentity {
        ServiceIdentity {
            service: ServiceId::parse("test:drive").unwrap(),
            instance: instance.into(),
        }
    }
    #[test]
    fn operation_binds_both_instances_source_version_and_target() {
        let source = service("source");
        let target = service("local");
        let operation =
            derive_download_operation(&source, &target, "file-1", "version-1", "/tmp/a");
        assert_eq!(
            operation,
            derive_download_operation(&source, &target, "file-1", "version-1", "/tmp/a")
        );
        for changed in [
            derive_download_operation(&service("other"), &target, "file-1", "version-1", "/tmp/a"),
            derive_download_operation(&source, &service("other"), "file-1", "version-1", "/tmp/a"),
            derive_download_operation(&source, &target, "file-2", "version-1", "/tmp/a"),
            derive_download_operation(&source, &target, "file-1", "version-2", "/tmp/a"),
            derive_download_operation(&source, &target, "file-1", "version-1", "/tmp/b"),
        ] {
            assert_ne!(operation, changed);
        }
    }
}
