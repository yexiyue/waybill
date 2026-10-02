//! 整文件上传生命周期：先持久化尝试，再发送，暂存完成后单独发布。
use super::{UploadMode, UploadOptions, finish, load_flow, report};
use crate::{
    BoxFuture,
    checkpoint::DriverState,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::{Source, SourceIdentity},
    transfer::{Receipt, TransferEngine},
    upload::UploadIntent,
};
use futures_util::{Stream, stream, task::AtomicWaker};
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

/// 按需读取的有界请求体；每块不得复制成第二份完整数据或无界预读。
/// 驱动必须在请求结束前释放请求体，禁止后台继续发送。
pub type UploadBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send>>;

#[derive(Default)]
struct Sent {
    bytes: AtomicU64,
    changed: AtomicWaker,
}
impl Sent {
    fn get(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    fn add(&self, length: usize) {
        self.bytes.fetch_add(length as u64, Ordering::Relaxed);
        self.changed.wake();
    }
}

/// 整文件上传对账结果；暂存完整不等于目标已发布。
#[derive(Debug)]
pub enum StreamStatus {
    /// 可开始整文件写入。
    Ready {
        /// 驱动状态。
        state: DriverState,
        /// 之前已经尝试发送；必须由调用方允许从头重传。
        restart_required: bool,
    },
    /// 暂存内容已验证，状态包含发布意图；持久化后才可发布。
    Staged(DriverState),
    /// 最终对象已验证且绑定当前操作。
    Complete {
        /// 驱动状态。
        state: DriverState,
        /// 完成证据。
        receipt: Receipt,
    },
}

/// 不支持偏移续传的上传端口；完成对账和整文件重传是不同能力。
pub trait StreamUploadSink: Send + Sync {
    /// 稳定实例身份。
    fn identity(&self) -> ServiceIdentity;
    /// 实际能力；要求 stream_upload 与 durable_upload。
    fn capabilities(&self) -> Capabilities;
    /// 分配私有状态；不得修改远端对象。
    fn prepare<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState>;
    /// 核验暂存 / 发布对象；验证读取每块不超过 read_limit。
    /// 不得把长度、ETag 或客户端属性当成内容哈希。
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
        read_limit: usize,
    ) -> BoxFuture<'a, StreamStatus>;
    /// 生成本次写入意图（含条件版本）；不得修改远端。
    /// 引擎持久化返回状态后才会调用 write。
    fn begin<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, DriverState>;
    /// 消费完整请求体写入私有暂存对象；不得直接覆盖最终目标。
    /// 请求后无论响应是否成功，引擎都通过 probe 核验结果。
    fn write<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
        body: UploadBody,
    ) -> BoxFuture<'a, ()>;
    /// 发布已验证暂存对象；禁止覆盖不属于当前操作的目标。
    /// 返回后仍通过 probe 获取最终证据；失败保留发布意图。
    fn publish<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, ()>;
}

impl TransferEngine {
    /// 开始或恢复整文件上传；失败后默认保留暂存并要求显式允许重传。
    /// Arc 使按需请求体拥有稳定源，HTTP 驱动无须复制整个文件。
    pub async fn upload_stream(
        &self,
        source: Arc<dyn Source>,
        sink: &dyn StreamUploadSink,
        options: UploadOptions<'_>,
    ) -> Result<Receipt> {
        let UploadOptions {
            intent,
            policy,
            stop,
            progress,
        } = options;
        let progress = progress.unwrap_or(&|_| {});
        check_stop(&stop)?;
        intent.validate()?;
        let service = sink.identity();
        service.validate()?;
        let capabilities = sink.capabilities();
        if !capabilities.stream_upload || !capabilities.durable_upload {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "durable stream upload required",
            ));
        }
        let read_limit = source.max_read_size().min(self.budget.chunk_size());
        if read_limit == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid source read limit",
            ));
        }
        let permit = Arc::new(self.budget.acquire()?);
        let lease = self.store.acquire(&intent.operation).await?;
        check_stop(&stop)?;
        let identity = source.identity().await?;
        identity.validate()?;
        let mut flow = load_flow(
            lease.as_ref(),
            intent.clone(),
            service,
            identity.clone(),
            UploadMode::Stream,
            sink.prepare(&intent, &identity),
        )
        .await?;
        let mut status = sink
            .probe(&flow.intent, &flow.source, &flow.driver, read_limit)
            .await?;
        let sent = Arc::new(Sent::default());
        let mut published = false;
        loop {
            match status {
                StreamStatus::Complete { state, receipt } => {
                    finish(source.as_ref(), lease.as_ref(), &mut flow, state, &receipt).await?;
                    report(&flow, sent.get(), progress);
                    return Ok(receipt);
                }
                StreamStatus::Ready {
                    state,
                    restart_required,
                } => {
                    if flow.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ObjectUnavailable,
                            "completed object unavailable",
                        ));
                    }
                    if published {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "publish result needs reconciliation",
                        ));
                    }
                    flow.driver = state;
                    flow.acknowledged = 0;
                    lease.save(&flow.checkpoint()).await?;
                    report(&flow, sent.get(), progress);
                    check_stop(&stop)?;
                    if restart_required && !policy.allow_restart {
                        return Err(Error::new(
                            ErrorKind::SessionExpired,
                            "explicit whole-file restart required",
                        ));
                    }
                    if source.identity().await? != flow.source {
                        return Err(Error::new(
                            ErrorKind::SourceChanged,
                            "source changed before upload",
                        ));
                    }
                    if restart_required {
                        flow.restarts = flow.restarts.checked_add(1).ok_or_else(|| {
                            Error::new(ErrorKind::Checkpoint, "restart counter overflow")
                        })?;
                    }
                    flow.driver = sink.begin(&flow.intent, &flow.source, &flow.driver).await?;
                    lease.save(&flow.checkpoint()).await?;
                    check_stop(&stop)?;
                    let body: UploadBody = Box::pin(stream::try_unfold(
                        (
                            source.clone(),
                            stop.clone(),
                            sent.clone(),
                            permit.clone(),
                            0u64,
                            flow.source.size,
                        ),
                        move |(source, stop, sent, permit, offset, size)| async move {
                            check_stop(&stop)?;
                            if offset == size {
                                return Ok(None);
                            }
                            let length = (size - offset).min(read_limit as u64) as usize;
                            let data = source.read_range(offset, length).await?;
                            if data.len() != length {
                                return Err(Error::new(ErrorKind::Protocol, "short source range"));
                            }
                            check_stop(&stop)?;
                            sent.add(length);
                            Ok(Some((
                                data,
                                (source, stop, sent, permit, offset + length as u64, size),
                            )))
                        },
                    ));
                    let result = {
                        let write = sink.write(&flow.intent, &flow.source, &flow.driver, body);
                        let mut write = std::pin::pin!(write);
                        futures_util::future::poll_fn(|cx| {
                            // HTTP 驱动可能在其他任务消费 body，不能只等待请求完成唤醒。
                            sent.changed.register(cx.waker());
                            report(&flow, sent.get(), progress);
                            write.as_mut().poll(cx)
                        })
                        .await
                    };
                    status = sink
                        .probe(&flow.intent, &flow.source, &flow.driver, read_limit)
                        .await?;
                    if let StreamStatus::Ready { state, .. } = &status {
                        flow.driver = state.clone();
                        lease.save(&flow.checkpoint()).await?;
                        check_stop(&stop)?;
                        return Err(result.err().unwrap_or_else(|| {
                            Error::new(ErrorKind::ResultUnknown, "stream upload not confirmed")
                        }));
                    }
                }
                StreamStatus::Staged(state) => {
                    if flow.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "published object no longer confirmed",
                        ));
                    }
                    flow.driver = state;
                    flow.acknowledged = flow.source.size;
                    lease.save(&flow.checkpoint()).await?;
                    report(&flow, sent.get(), progress);
                    check_stop(&stop)?;
                    if published {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "publish result needs reconciliation",
                        ));
                    }
                    if source.identity().await? != flow.source {
                        return Err(Error::new(
                            ErrorKind::SourceChanged,
                            "source changed before publish",
                        ));
                    }
                    let result = sink.publish(&flow.intent, &flow.source, &flow.driver).await;
                    published = true;
                    status = sink
                        .probe(&flow.intent, &flow.source, &flow.driver, read_limit)
                        .await?;
                    if !matches!(status, StreamStatus::Complete { .. }) {
                        result?;
                    }
                }
            }
        }
    }
}
fn check_stop(stop: &crate::transfer::StopToken) -> Result<()> {
    if stop.is_stopped() {
        Err(Error::new(ErrorKind::Paused, "upload paused"))
    } else {
        Ok(())
    }
}
