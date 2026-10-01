//! 上传状态机：远端对账先于继续发送，完成回执先于成功返回。
use crate::{
    BoxFuture,
    checkpoint::{Checkpoint, CheckpointStore, DriverState, FORMAT_VERSION},
    error::{Error, ErrorKind, Result},
    service::{Capabilities, ServiceIdentity},
    source::{Source, SourceIdentity},
};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
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
        if self.operation.is_empty()
            || self.operation.len() > 128
            || !self
                .operation
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
            || self.target.is_empty()
            || self.target.len() > 4096
            || self.target.split('/').any(|p| {
                p.is_empty()
                    || p == "."
                    || p == ".."
                    || p.len() > 255
                    || p.chars().any(|c| c.is_control() || c == '\\')
            })
        {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid upload intent"));
        }
        Ok(())
    }
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
    /// 最近服务端确认字节数，可在对账时回退。
    pub acknowledged: u64,
    /// 已持久记录的确认字节数。
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
/// 多个引擎共享的资源门禁。繁忙返回 ResourceBusy，宿主负责等待策略。
pub struct ResourceBudget {
    chunk_size: usize,
    active: AtomicUsize,
    concurrency: usize,
}
impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            chunk_size: 8 * 1024 * 1024,
            active: AtomicUsize::new(0),
            concurrency: 2,
        }
    }
}
impl ResourceBudget {
    /// 块为 256 KiB 的倍数且不超过 8 MiB，并发不超过 2。
    pub fn new(chunk_size: usize, concurrency: usize) -> Result<Self> {
        if chunk_size == 0
            || chunk_size > 8 * 1024 * 1024
            || chunk_size % (256 * 1024) != 0
            || !(1..=2).contains(&concurrency)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid resource budget",
            ));
        }
        Ok(Self {
            chunk_size,
            concurrency,
            active: AtomicUsize::new(0),
        })
    }
    /// 共享上传数据缓冲的最大字节数，不包括有界校验 / 元数据开销。
    pub fn max_data_bytes(&self) -> usize {
        self.chunk_size * self.concurrency
    }
    fn acquire(self: &Arc<Self>) -> Result<Permit> {
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.concurrency).then_some(n + 1)
            })
            .map_err(|_| Error::new(ErrorKind::ResourceBusy, "upload budget exhausted"))?;
        Ok(Permit(self.clone()))
    }
}
struct Permit(Arc<ResourceBudget>);
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
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
        if identity.blake3.len() != 64
            || !identity
                .blake3
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid source digest"));
        }
        let mut checkpoint = match lease.load().await? {
            Some(saved) => {
                if saved.version != FORMAT_VERSION {
                    return Err(Error::new(
                        ErrorKind::IncompatibleVersion,
                        "checkpoint version",
                    ));
                }
                if saved.intent != intent || saved.service != sink.identity() {
                    return Err(Error::new(
                        ErrorKind::IdentityMismatch,
                        "checkpoint binding",
                    ));
                }
                if saved.source != identity {
                    return Err(Error::new(
                        ErrorKind::SourceChanged,
                        "source identity changed",
                    ));
                }
                if saved.acknowledged > identity.size {
                    return Err(Error::new(ErrorKind::Checkpoint, "invalid saved offset"));
                }
                saved
            }
            None => {
                let driver = sink.prepare(&intent, &identity).await?;
                let saved = Checkpoint {
                    version: FORMAT_VERSION,
                    intent,
                    service: sink.identity(),
                    source: identity,
                    acknowledged: 0,
                    restarts: 0,
                    driver,
                    receipt: None,
                };
                lease.save(&saved).await?;
                saved
            }
        };
        let mut status = sink
            .probe(&checkpoint.intent, &checkpoint.source, &checkpoint.driver)
            .await?;
        let mut restarts = 0;
        let mut sent = 0u64;
        loop {
            match status {
                SessionStatus::Complete { state, receipt } => {
                    if receipt.operation != checkpoint.intent.operation
                        || receipt.service != checkpoint.service
                        || receipt.target != checkpoint.intent.target
                        || receipt.size != checkpoint.source.size
                        || receipt.object.is_empty()
                    {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "invalid completion receipt",
                        ));
                    }
                    if source.identity().await? != checkpoint.source {
                        return Err(Error::new(
                            ErrorKind::SourceChanged,
                            "source changed during upload",
                        ));
                    }
                    checkpoint.driver = state;
                    checkpoint.acknowledged = checkpoint.source.size;
                    checkpoint.receipt = Some(receipt.clone());
                    lease.save(&checkpoint).await?;
                    report(&checkpoint, sent, progress);
                    return Ok(receipt);
                }
                SessionStatus::Expired(state) => {
                    checkpoint.driver = state;
                    lease.save(&checkpoint).await?;
                    if checkpoint.receipt.is_some() {
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
                    checkpoint.restarts = checkpoint.restarts.checked_add(1).ok_or_else(|| {
                        Error::new(ErrorKind::Checkpoint, "restart counter overflow")
                    })?;
                    checkpoint.acknowledged = 0;
                    lease.save(&checkpoint).await?;
                    report(&checkpoint, sent, progress);
                    status = sink
                        .initialize(&checkpoint.intent, &checkpoint.source, &checkpoint.driver)
                        .await?;
                }
                SessionStatus::Uninitialized(state) => {
                    if checkpoint.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ObjectUnavailable,
                            "completed object unavailable",
                        ));
                    }
                    checkpoint.driver = state;
                    lease.save(&checkpoint).await?;
                    if stop.is_stopped() {
                        return Err(Error::new(ErrorKind::Paused, "upload paused"));
                    }
                    status = sink
                        .initialize(&checkpoint.intent, &checkpoint.source, &checkpoint.driver)
                        .await?;
                }
                SessionStatus::Ready { state, offset } => {
                    if checkpoint.receipt.is_some() {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "completed object no longer confirmed",
                        ));
                    }
                    if offset > checkpoint.source.size {
                        return Err(Error::new(
                            ErrorKind::Protocol,
                            "remote offset exceeds source",
                        ));
                    }
                    checkpoint.driver = state;
                    checkpoint.acknowledged = offset;
                    lease.save(&checkpoint).await?;
                    report(&checkpoint, sent, progress);
                    if stop.is_stopped() {
                        return Err(Error::new(ErrorKind::Paused, "upload paused"));
                    }
                    if offset == checkpoint.source.size {
                        return Err(Error::new(
                            ErrorKind::ResultUnknown,
                            "all bytes confirmed without completion",
                        ));
                    }
                    let length = (checkpoint.source.size - offset)
                        .min(self.budget.chunk_size as u64)
                        as usize;
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
                        .write_chunk(
                            &checkpoint.intent,
                            &checkpoint.source,
                            &checkpoint.driver,
                            offset,
                            data,
                        )
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
                            match sink
                                .probe(&checkpoint.intent, &checkpoint.source, &checkpoint.driver)
                                .await
                            {
                                Ok(done @ SessionStatus::Complete { .. }) => done,
                                Ok(SessionStatus::Ready { state, offset })
                                    if offset <= checkpoint.source.size =>
                                {
                                    checkpoint.driver = state;
                                    checkpoint.acknowledged = offset;
                                    lease.save(&checkpoint).await?;
                                    report(&checkpoint, sent, progress);
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
            if checkpoint.version != FORMAT_VERSION {
                return Err(Error::new(
                    ErrorKind::IncompatibleVersion,
                    "checkpoint version",
                ));
            }
            if checkpoint.receipt.as_ref() != Some(receipt) {
                return Err(Error::new(ErrorKind::IdentityMismatch, "receipt mismatch"));
            }
            lease.remove().await?;
        }
        Ok(())
    }
}
fn report(checkpoint: &Checkpoint, sent: u64, progress: &(dyn Fn(Progress) + Send + Sync)) {
    progress(Progress {
        sent,
        acknowledged: checkpoint.acknowledged,
        persisted: checkpoint.acknowledged,
        total: checkpoint.source.size,
        epoch: checkpoint.restarts,
        complete: checkpoint.receipt.is_some(),
    });
}
