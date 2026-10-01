//! 持久恢复信封与排他租约。私有 payload 不进入 Debug。
use crate::{
    BoxFuture,
    service::ServiceIdentity,
    source::SourceIdentity,
    upload::{Receipt, UploadIntent},
};
use serde::{Deserialize, Serialize};
use std::fmt;
/// 当前公共信封格式；未知版本拒绝恢复并保留记录。
pub const FORMAT_VERSION: u32 = 1;
/// service 私有的版本化状态。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverState {
    /// 驱动状态格式版本。
    pub version: u32,
    /// 敏感、不透明状态；由存储保护。
    pub payload: Vec<u8>,
}
impl fmt::Debug for DriverState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DriverState")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
/// 绑定操作、双方身份与实际确认进度的持久记录。
#[derive(Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 公共信封版本。
    pub version: u32,
    /// 上传意图。
    pub intent: UploadIntent,
    /// 目标 service 实例。
    pub service: ServiceIdentity,
    /// 稳定源身份。
    pub source: SourceIdentity,
    /// 最近一次服务端确认偏移；可以在对账时回退。
    pub acknowledged: u64,
    /// 已显式重建的次数，用于进度 epoch。
    pub restarts: u32,
    /// 驱动状态。
    pub driver: DriverState,
    /// 远端已完成、尚待消费者记账的回执。
    pub receipt: Option<Receipt>,
}
impl fmt::Debug for Checkpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Checkpoint")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}
/// 排他操作租约；释放后其他进程才可恢复或确认。
pub trait CheckpointLease: Send + Sync {
    /// 加载有界记录。
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>>;
    /// 同步持久化；失败不得报告上传成功。
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()>;
    /// 仅在消费者确认记账后删除。
    fn remove(&self) -> BoxFuture<'_, ()>;
}
/// 存储实现拥有原生锁与 IO；核心不直接接触文件路径。
pub trait CheckpointStore: Send + Sync {
    /// operation 为存储的排他键；不同实例复用相同 ID 时须由信封拒绝。
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>>;
}
