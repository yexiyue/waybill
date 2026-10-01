//! service 边界的稳定错误分类。
use std::{error::Error as StdError, fmt};
/// 可以由消费者匹配并映射为恢复动作的错误类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 当前能力不支持该操作。
    Unsupported,
    /// 配置或输入不合法。
    InvalidInput,
    /// 暂存或源版本发生变化。
    SourceChanged,
    /// checkpoint 绑定的实例、目标或操作不一致。
    IdentityMismatch,
    /// 公共或驱动状态格式不兼容。
    IncompatibleVersion,
    /// checkpoint 损坏或 IO 失败。
    Checkpoint,
    /// 源读取 IO 失败。
    Io,
    /// 同一操作正在由另一个执行者处理。
    OperationBusy,
    /// 共享资源预算已用尽。
    ResourceBusy,
    /// 会话过期；记录仍可供显式重建。
    SessionExpired,
    /// 无法确认远端结果；禁止盲目创建新对象。
    ResultUnknown,
    /// 可重试网络或限流错误。
    Retryable,
    /// 宿主需要重新授权。
    Authentication,
    /// 目标名称冲突。
    Conflict,
    /// 引用的对象不存在。
    NotFound,
    /// 后端响应不符合协议。
    Protocol,
    /// 已完成对象不可用；不自动复活。
    ObjectUnavailable,
    /// 用户暂停；恢复状态保留。
    Paused,
}
/// 诊断信息只允许受控静态描述；后端原始错误不进入默认格式化。
#[derive(thiserror::Error)]
#[error("{kind:?}: {context}")]
pub struct Error {
    /// 稳定错误类别。
    pub kind: ErrorKind,
    context: &'static str,
    #[source]
    cause: Option<Box<dyn StdError + Send + Sync>>,
}
impl Error {
    /// 创建不含敏感数据的错误。
    pub fn new(kind: ErrorKind, context: &'static str) -> Self {
        Self {
            kind,
            context,
            cause: None,
        }
    }
    /// 保存底层诊断来源；调用方不得把 source 链直接写日志。
    pub fn with_source(mut self, cause: impl StdError + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }
}
impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
/// 核心与 service 统一的操作结果。
pub type Result<T> = std::result::Result<T, Error>;
