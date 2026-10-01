//! 上传状态机：远端对账先于继续发送，完成回执先于成功返回。
use crate::{
    BoxFuture,
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointStore, DriverState, Flow, UploadFlow},
    download::Verification,
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::{Source, SourceIdentity},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// 目标冲突策略；不同进程对同名对象不具备跨进程原子隔离。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictPolicy {
    /// 默认拒绝同名目标。
    #[default]
    Reject,
    /// 冲突时添加稳定操作后缀，不覆盖其他对象。
    OperationSuffix,
}
/// 消费者指定的通用上传意图，不包含设备或接收会话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadIntent {
    /// 持久稳定的操作 ID；同一操作不得更换源或目标。
    pub operation: String,
    /// 相对于 service 根目录的目标路径。
    pub target: String,
    /// 同名目标策略。
    pub conflict: ConflictPolicy,
}
impl UploadIntent {
    /// 校验有界操作标识和目标路径。
    pub fn validate(&self) -> Result<()> {
        if !valid_operation(&self.operation) || !valid_target(&self.target) {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid upload intent"));
        }
        Ok(())
    }
}
/// 操作标识：1..=128 字节的 ASCII 字母数字与 `-_.:`。
pub(crate) fn valid_operation(operation: &str) -> bool {
    !operation.is_empty()
        && operation.len() <= 128
        && operation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}
/// 目标路径：段非空且不含 `.`、`..`、反斜杠或控制字符。
fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= 4096
        && target.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment.len() <= 255
                && !segment.chars().any(|c| c.is_control() || c == '\\')
        })
}
/// BLAKE3 小写十六进制摘要（64 字符）。
fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
/// 持久完成证据；消费者记账后以同一回执确认清理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// 操作身份。
    pub operation: String,
    /// 目标实例。
    pub service: ServiceIdentity,
    /// 目标相对路径。
    pub target: String,
    /// 后端实际对象引用。
    pub object: String,
    /// 远端确认长度。
    pub size: u64,
    /// 交付内容的验证证据；v1 回执按未验证兼容解码。
    #[serde(default)]
    pub verified: Verification,
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
/// 连续偏移上传契约；驱动不拥有核心调度或 checkpoint IO。
pub trait UploadSink: Send + Sync {
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
/// 显式暂停信号；丢弃 run future 同样保留最近持久 checkpoint。
#[derive(Clone, Default)]
pub struct StopToken(Arc<AtomicBool>);
impl StopToken {
    /// 停止安排后续块；不会删除源、checkpoint 或远端对象。
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// 是否已请求停止。
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
/// 上传恢复策略。
#[derive(Debug, Clone, Copy, Default)]
pub struct UploadPolicy {
    /// 允许过期后从头重传；默认 false，每次运行最多两次。
    pub allow_restart: bool,
}
/// 不将发送、确认、持久化和完成混为一谈的进度。
#[derive(Debug, Clone, Copy)]
pub struct Progress {
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
pub struct RunOptions<'a> {
    /// 稳定上传意图。
    pub intent: UploadIntent,
    /// 显式恢复策略。
    pub policy: UploadPolicy,
    /// 宿主暂停信号。
    pub stop: &'a StopToken,
    /// 有界同步通知；回调不能阻塞或保存数据缓冲。
    pub progress: &'a (dyn Fn(Progress) + Send + Sync),
}
/// 只依赖公开端口的上传引擎。
pub struct UploadEngine {
    budget: Arc<ResourceBudget>,
}
impl UploadEngine {
    /// 注入共享预算；跨 service 实例共享同一个 Arc 才有统一上界。
    pub fn new(budget: Arc<ResourceBudget>) -> Self {
        Self { budget }
    }
    /// 开始或恢复上传。源在整个调用期间必须不可变。
    pub async fn run(
        &self,
        source: &dyn Source,
        sink: &dyn UploadSink,
        store: &dyn CheckpointStore,
        options: RunOptions<'_>,
    ) -> Result<Receipt> {
        let RunOptions {
            intent,
            policy,
            stop,
            progress,
        } = options;
        intent.validate()?;
        let caps = sink.capabilities();
        if !caps.offset_upload || !caps.durable_upload {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "durable offset upload required",
            ));
        }
        let _permit = self.budget.acquire()?;
        let lease = store.acquire(&intent.operation).await?;
        if stop.is_stopped() {
            return Err(Error::new(ErrorKind::Paused, "upload paused"));
        }
        let identity = source.identity().await?;
        if !valid_digest(&identity.blake3) {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid source digest"));
        }
        let mut flow = match lease.load().await? {
            Some(saved) => {
                if !Checkpoint::supported(saved.version, &saved.flow) {
                    return Err(Error::new(
                        ErrorKind::IncompatibleVersion,
                        "checkpoint version",
                    ));
                }
                match saved.flow {
                    Flow::Upload(flow) => {
                        if flow.intent != intent || flow.service != sink.identity() {
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
                    service: sink.identity(),
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
                    let length =
                        (flow.source.size - offset).min(self.budget.chunk_size() as u64) as usize;
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
    /// 消费者已提交业务记账后，以准确回执删除 checkpoint；源文件不受影响。
    pub async fn confirm(&self, store: &dyn CheckpointStore, receipt: &Receipt) -> Result<()> {
        let lease = store.acquire(&receipt.operation).await?;
        if let Some(checkpoint) = lease.load().await? {
            if !Checkpoint::supported(checkpoint.version, &checkpoint.flow) {
                return Err(Error::new(
                    ErrorKind::IncompatibleVersion,
                    "checkpoint version",
                ));
            }
            if checkpoint.upload().and_then(|flow| flow.receipt.as_ref()) != Some(receipt) {
                return Err(Error::new(ErrorKind::IdentityMismatch, "receipt mismatch"));
            }
            lease.remove().await?;
        }
        Ok(())
    }
}
fn report(flow: &UploadFlow, sent: u64, progress: &(dyn Fn(Progress) + Send + Sync)) {
    progress(Progress {
        sent,
        persisted: flow.acknowledged,
        total: flow.source.size,
        epoch: flow.restarts,
        complete: flow.receipt.is_some(),
    });
}
