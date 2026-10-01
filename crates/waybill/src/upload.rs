//! 上传状态机：远端对账先于继续发送，完成回执先于成功返回。
use crate::transfer::{ConflictPolicy, Receipt, StopToken, TransferEngine, valid_operation};
use crate::{
    BoxFuture,
    checkpoint::{Checkpoint, DriverState, Flow, UploadFlow},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::{Source, SourceIdentity},
};
use serde::{Deserialize, Serialize};

/// 消费者指定的通用上传意图，不包含设备或接收会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadIntent {
    /// 持久稳定的操作 ID；同一操作不得更换源或目标。
    pub operation: String,
    /// 目标 service 解释的对象引用；GDrive 使用相对根目录的路径。
    pub target: String,
    /// 同名目标策略。
    pub conflict: ConflictPolicy,
}
impl UploadIntent {
    /// 校验有界操作标识和目标引用。
    pub fn validate(&self) -> Result<()> {
        if !valid_operation(&self.operation) || !crate::object::valid_reference(&self.target) {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid upload intent"));
        }
        Ok(())
    }
}
/// 服务端对账结果；每次状态更新都需持久化。
#[derive(Debug)]
pub enum SessionStatus {
    /// 服务端确认连续偏移。
    Ready {
        /// 驱动状态。
        state: DriverState,
        /// 服务器确认偏移。
        offset: u64,
    },
    /// 尚未初始化会话，但对象 ID 已持久化。
    Uninitialized(DriverState),
    /// 会话过期，保留原状态等待显式决策。
    Expired(DriverState),
    /// 已确认远端完成。
    Complete {
        /// 驱动状态。
        state: DriverState,
        /// 完成证据。
        receipt: Receipt,
    },
}
/// 连续偏移上传的块大小约束；末块不要求对齐。
#[derive(Debug, Clone, Copy)]
pub struct UploadChunkLimits {
    /// 单块最大字节数，必须非零。
    pub max_size: usize,
    /// 非末块长度的倍数，必须非零且不大于 max_size。
    pub alignment: usize,
}
impl UploadChunkLimits {
    fn select(self, budget: usize, source: usize) -> Result<usize> {
        if self.max_size == 0
            || self.alignment == 0
            || self.alignment > self.max_size
            || source == 0
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid upload chunk limits",
            ));
        }
        let size = budget.min(source).min(self.max_size);
        let aligned = size - size % self.alignment;
        if aligned == 0 {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "budget below upload alignment",
            ));
        }
        Ok(aligned)
    }
}
/// 连续偏移上传契约；驱动不拥有核心调度或 checkpoint IO。
pub trait UploadSink: Send + Sync {
    /// 当前后端的上传块约束；末块可小于对齐长度。
    fn chunk_limits(&self) -> UploadChunkLimits;
    /// 稳定实例身份。
    fn identity(&self) -> ServiceIdentity;
    /// 实际能力。
    fn capabilities(&self) -> Capabilities;
    /// 仅分配状态 / 对象 ID，不创建上传对象；核心先保存再 initialize。
    fn prepare<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
    ) -> BoxFuture<'a, DriverState>;
    /// 对账对象与会话；未确认结果返回 ResultUnknown，不创建新对象。
    fn probe<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus>;
    /// 初始化或显式重建会话，复用已持久化对象 ID。
    fn initialize<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
    ) -> BoxFuture<'a, SessionStatus>;
    /// 接收有界块的所有权；不复制成第二份完整块。
    fn write_chunk<'a>(
        &'a self,
        intent: &'a UploadIntent,
        source: &'a SourceIdentity,
        state: &'a DriverState,
        offset: u64,
        data: Vec<u8>,
    ) -> BoxFuture<'a, SessionStatus>;
}
/// 上传恢复策略。
#[derive(Debug, Clone, Copy, Default)]
pub struct UploadPolicy {
    /// 允许过期后从头重传；默认 false，每次运行最多两次。
    pub allow_restart: bool,
}
/// 不将发送、确认、持久化和完成混为一谈的进度。
#[derive(Debug, Clone, Copy)]
pub struct UploadProgress {
    /// 本次运行尝试发送字节数，可因重传超过文件长度。
    pub sent: u64,
    /// 已持久化的服务端确认字节数；引擎只在 checkpoint 落盘后上报，
    /// 对账回退时此值可以变小。
    pub persisted: u64,
    /// 源长度。
    pub total: u64,
    /// 显式重建次数。
    pub epoch: u32,
    /// 回执已持久化；百分比为 100 不代表此标志为 true。
    pub complete: bool,
}
/// 单次上传的意图、策略与宿主控制端口。
pub struct UploadOptions<'a> {
    /// 稳定上传意图。
    pub intent: UploadIntent,
    /// 显式恢复策略。
    pub policy: UploadPolicy,
    /// 宿主暂停信号。
    pub stop: StopToken,
    /// 有界同步通知；回调不能阻塞或保存数据缓冲。
    pub progress: Option<&'a (dyn Fn(UploadProgress) + Send + Sync)>,
}
impl<'a> UploadOptions<'a> {
    /// 使用稳定操作 ID 与目标路径创建默认选项；开始传输时统一校验。
    /// 默认拒绝同名目标，不暂停且不订阅进度。
    pub fn new(operation: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            intent: UploadIntent {
                operation: operation.into(),
                target: target.into(),
                conflict: ConflictPolicy::Reject,
            },
            stop: StopToken::default(),
            progress: None,
            policy: UploadPolicy::default(),
        }
    }
}
impl TransferEngine {
    /// 开始或恢复上传。源在整个调用期间必须不可变。
    pub async fn upload(
        &self,
        source: &dyn Source,
        sink: &dyn UploadSink,
        options: UploadOptions<'_>,
    ) -> Result<Receipt> {
        let UploadOptions {
            intent,
            policy,
            stop,
            progress,
        } = options;
        let progress = progress.unwrap_or(&|_| {});
        if stop.is_stopped() {
            return Err(Error::new(ErrorKind::Paused, "upload paused"));
        }
        intent.validate()?;
        let service = sink.identity();
        service.validate()?;
        let caps = sink.capabilities();
        if !caps.offset_upload || !caps.durable_upload {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "durable offset upload required",
            ));
        }
        let chunk_size = sink
            .chunk_limits()
            .select(self.budget.chunk_size(), source.max_read_size())?;
        let _permit = self.budget.acquire()?;
        let lease = self.store.acquire(&intent.operation).await?;
        if stop.is_stopped() {
            return Err(Error::new(ErrorKind::Paused, "upload paused"));
        }
        let identity = source.identity().await?;
        identity.validate()?;
        let mut flow = match lease.load().await? {
            Some(saved) => {
                if !Checkpoint::supported(saved.version) {
                    return Err(Error::new(
                        ErrorKind::IncompatibleVersion,
                        "checkpoint version",
                    ));
                }
                match saved.flow {
                    Flow::Upload(flow) => {
                        if flow.intent != intent || flow.service != service.clone() {
                            return Err(Error::new(
                                ErrorKind::IdentityMismatch,
                                "checkpoint binding",
                            ));
                        }
                        if flow.source != identity {
                            return Err(Error::new(
                                ErrorKind::SourceChanged,
                                "source identity changed",
                            ));
                        }
                        if flow.acknowledged > identity.size {
                            return Err(Error::new(ErrorKind::Checkpoint, "invalid saved offset"));
                        }
                        flow
                    }
                    Flow::Download(_) => {
                        return Err(Error::new(
                            ErrorKind::IdentityMismatch,
                            "checkpoint binding",
                        ));
                    }
                }
            }
            None => {
                let driver = sink.prepare(&intent, &identity).await?;
                let flow = UploadFlow {
                    intent,
                    service: service.clone(),
                    source: identity,
                    acknowledged: 0,
                    restarts: 0,
                    driver,
                    receipt: None,
                };
                lease.save(&flow.checkpoint()).await?;
                flow
            }
        };
        let mut status = sink.probe(&flow.intent, &flow.source, &flow.driver).await?;
        let mut restarts = 0;
        let mut sent = 0u64;
        loop {
            match status {
                SessionStatus::Complete { state, receipt } => {
                    if receipt.operation != flow.intent.operation
                        || receipt.service != flow.service
                        || receipt.target != flow.intent.target
                        || receipt.size != flow.source.size
                        || receipt.object.is_empty()
                    {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "invalid completion receipt",
                        ));
                    }
                    if source.identity().await? != flow.source {
                        return Err(Error::new(
                            ErrorKind::SourceChanged,
                            "source changed during upload",
                        ));
                    }
                    flow.driver = state;
                    flow.acknowledged = flow.source.size;
                    flow.receipt = Some(receipt.clone());
                    lease.save(&flow.checkpoint()).await?;
                    report(&flow, sent, progress);
                    return Ok(receipt);
                }
                SessionStatus::Expired(state) => {
                    flow.driver = state;
                    lease.save(&flow.checkpoint()).await?;
                    if flow.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ObjectUnavailable,
                            "completed object unavailable",
                        ));
                    }
                    if stop.is_stopped() {
                        return Err(Error::new(ErrorKind::Paused, "upload paused"));
                    }
                    if !policy.allow_restart || restarts >= 2 {
                        return Err(Error::new(
                            ErrorKind::SessionExpired,
                            "explicit restart required",
                        ));
                    }
                    restarts += 1;
                    flow.restarts = flow.restarts.checked_add(1).ok_or_else(|| {
                        Error::new(ErrorKind::Checkpoint, "restart counter overflow")
                    })?;
                    flow.acknowledged = 0;
                    lease.save(&flow.checkpoint()).await?;
                    report(&flow, sent, progress);
                    status = sink
                        .initialize(&flow.intent, &flow.source, &flow.driver)
                        .await?;
                }
                SessionStatus::Uninitialized(state) => {
                    if flow.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ObjectUnavailable,
                            "completed object unavailable",
                        ));
                    }
                    flow.driver = state;
                    lease.save(&flow.checkpoint()).await?;
                    if stop.is_stopped() {
                        return Err(Error::new(ErrorKind::Paused, "upload paused"));
                    }
                    status = sink
                        .initialize(&flow.intent, &flow.source, &flow.driver)
                        .await?;
                }
                SessionStatus::Ready { state, offset } => {
                    if flow.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "completed object no longer confirmed",
                        ));
                    }
                    if offset > flow.source.size {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "remote offset exceeds source",
                        ));
                    }
                    flow.driver = state;
                    flow.acknowledged = offset;
                    lease.save(&flow.checkpoint()).await?;
                    report(&flow, sent, progress);
                    if stop.is_stopped() {
                        return Err(Error::new(ErrorKind::Paused, "upload paused"));
                    }
                    if offset == flow.source.size {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "all bytes confirmed without completion",
                        ));
                    }
                    let length = (flow.source.size - offset).min(chunk_size as u64) as usize;
                    let data = source.read_range(offset, length).await?;
                    if data.len() != length {
                        return Err(Error::new(ErrorKind::Protocol, "short source range"));
                    }
                    if stop.is_stopped() {
                        return Err(Error::new(
                            ErrorKind::Paused,
                            "upload paused before request",
                        ));
                    }
                    sent = sent.saturating_add(length as u64);
                    status = match sink
                        .write_chunk(&flow.intent, &flow.source, &flow.driver, offset, data)
                        .await
                    {
                        Ok(next) => {
                            if matches!(&next, SessionStatus::Ready { offset: next, .. } if *next <= offset)
                            {
                                return Err(Error::new(
                                    ErrorKind::Retryable,
                                    "upload made no progress",
                                ));
                            }
                            next
                        }
                        Err(error) => {
                            // 请求可能已生效，必须先对账；对账失败则保留旧状态等待下次恢复。
                            match sink.probe(&flow.intent, &flow.source, &flow.driver).await {
                                Ok(done @ SessionStatus::Complete { .. }) => done,
                                Ok(SessionStatus::Ready { state, offset })
                                    if offset <= flow.source.size =>
                                {
                                    flow.driver = state;
                                    flow.acknowledged = offset;
                                    lease.save(&flow.checkpoint()).await?;
                                    report(&flow, sent, progress);
                                    return Err(error);
                                }
                                Ok(expired @ SessionStatus::Expired(_)) => expired,
                                _ => {
                                    return Err(Error::new(
                                        ErrorKind::ResultUnknown,
                                        "upload result needs reconciliation",
                                    ));
                                }
                            }
                        }
                    };
                }
            }
        }
    }
}
fn report(flow: &UploadFlow, sent: u64, progress: &(dyn Fn(UploadProgress) + Send + Sync)) {
    progress(UploadProgress {
        sent,
        persisted: flow.acknowledged,
        total: flow.source.size,
        epoch: flow.restarts,
        complete: flow.receipt.is_some(),
    });
}
