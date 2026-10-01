//! 共有传输生命周期：引擎资源、冲突策略、暂停与持久完成回执。
use crate::{
    budget::ResourceBudget,
    checkpoint::{Checkpoint, CheckpointStore, Flow},
    content::Verification,
    error::{Error, ErrorKind, Result},
    service::ServiceIdentity,
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
/// 操作标识：1..=128 字节的 ASCII 字母数字与 `-_.:`。
pub(crate) fn valid_operation(operation: &str) -> bool {
    !operation.is_empty()
        && operation.len() <= 128
        && operation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
}
/// 持久完成证据；消费者记账后以同一回执确认清理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// 操作身份。
    pub operation: String,
    /// 目标实例。
    pub service: ServiceIdentity,
    /// 意图中的目标引用；由目标 service 解释。
    pub target: String,
    /// 后端实际对象引用。
    pub object: String,
    /// 已交付的字节长度。
    pub size: u64,
    /// 实际验证证据；源哈希或客户端提交属性不能冒充服务端内容校验。
    pub verified: Verification,
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
/// 共用 checkpoint 存储与资源预算的双向传输引擎。
///
/// 引擎不拥有执行器、授权或业务账本。一次构造可执行多个上传、下载操作；
/// 同一实例的两种方向自动共享预算，多个实例可通过 `with_budget` 共享门禁。
#[derive(Clone)]
pub struct TransferEngine {
    pub(crate) store: Arc<dyn CheckpointStore>,
    pub(crate) budget: Arc<ResourceBudget>,
}
impl TransferEngine {
    /// 注入持久存储，使用默认的有界资源预算。
    pub fn new(store: impl CheckpointStore + 'static) -> Self {
        Self {
            store: Arc::new(store),
            budget: Arc::new(ResourceBudget::default()),
        }
    }
    /// 替换资源预算；多个引擎传入同一个 Arc 才有统一上界。
    pub fn with_budget(mut self, budget: Arc<ResourceBudget>) -> Self {
        self.budget = budget;
        self
    }
    /// 消费者提交业务账本后，以准确回执删除对应 checkpoint。
    ///
    /// 同时适用于上传和下载；不删除源、暂存或交付对象。记录不存在时幂等成功。
    /// 确认后不得复用该操作 ID 启动新任务；幂等范围仅覆盖记录保留期间。
    pub async fn confirm(&self, receipt: &Receipt) -> Result<()> {
        if !valid_operation(&receipt.operation) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid receipt operation",
            ));
        }
        let lease = self.store.acquire(&receipt.operation).await?;
        if let Some(checkpoint) = lease.load().await? {
            if !Checkpoint::supported(checkpoint.version) {
                return Err(Error::new(
                    ErrorKind::IncompatibleVersion,
                    "checkpoint version",
                ));
            }
            let saved = match &checkpoint.flow {
                Flow::Upload(flow) => flow.receipt.as_ref(),
                Flow::Download(flow) => flow.receipt.as_ref(),
            };
            if saved != Some(receipt) {
                return Err(Error::new(ErrorKind::IdentityMismatch, "receipt mismatch"));
            }
            lease.remove().await?;
        }
        Ok(())
    }
}
