//! 持久恢复信封与排他租约。私有 payload 不进入 Debug。
use crate::{
    BoxFuture,
    download::{DownloadIntent, Interval, RemoteIdentity},
    service::ServiceIdentity,
    source::SourceIdentity,
    upload::{Receipt, UploadIntent},
};
use serde::{Deserialize, Serialize};
use std::fmt;
/// 当前公共信封格式；未知版本拒绝恢复并保留记录。
/// v2 起按 `flow` 区分上传与下载记录；v1 平铺上传记录按上传流兼容加载。
pub const FORMAT_VERSION: u32 = 2;
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
/// 上传侧记录：绑定意图、目标实例、源身份与连续确认偏移。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadFlow {
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
impl UploadFlow {
    /// 以当前字段打包为 v2 信封；v1 记录在下次保存时升级。
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            version: FORMAT_VERSION,
            flow: Flow::Upload(self.clone()),
        }
    }
}
/// 下载侧记录：区间账本是完成度的唯一事实源，`.part` 长度不参与判断。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFlow {
    /// 下载意图。
    pub intent: DownloadIntent,
    /// 本地目标 service 实例。
    pub service: ServiceIdentity,
    /// 云端源身份。
    pub source: RemoteIdentity,
    /// 已写入并同步后记账的区间；合并有序。
    pub persisted: Vec<Interval>,
    /// 驱动状态。
    pub driver: DriverState,
    /// 已发布、尚待消费者记账的回执。
    pub receipt: Option<Receipt>,
}
impl DownloadFlow {
    /// 以当前字段打包为 v2 信封；保存时始终升级到最新格式。
    pub fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            version: FORMAT_VERSION,
            flow: Flow::Download(self.clone()),
        }
    }
}
/// 传输方向及其全部记录字段；`kind` 为方向标签。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Flow {
    /// 上传记录。
    Upload(UploadFlow),
    /// 下载记录。
    Download(DownloadFlow),
}
/// 绑定操作、双方身份与实际确认进度的持久记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// 公共信封版本。
    pub version: u32,
    /// 方向及记录字段。
    pub flow: Flow,
}
/// v1 平铺上传记录没有 `flow` 对象；由存储层在解码前包一层上传流
/// （见 waybill-service-fs 的 `decode_checkpoint`），核心只接受带方向
/// 标签的 v2 结构。v1 记录在下次保存时升级为 v2。
impl Checkpoint {
    /// 引擎可接受的信封版本：v2，或兼容加载的 v1 上传记录。
    pub fn supported(version: u32, flow: &Flow) -> bool {
        version == FORMAT_VERSION || (version == 1 && matches!(flow, Flow::Upload(_)))
    }
    /// 上传记录引用；下载记录返回 None。
    pub fn upload(&self) -> Option<&UploadFlow> {
        match &self.flow {
            Flow::Upload(flow) => Some(flow),
            Flow::Download(_) => None,
        }
    }
    /// 下载记录引用；上传记录返回 None。
    pub fn download(&self) -> Option<&DownloadFlow> {
        match &self.flow {
            Flow::Upload(_) => None,
            Flow::Download(flow) => Some(flow),
        }
    }
}
/// 排他操作租约；释放后其他进程才可恢复或确认。
pub trait CheckpointLease: Send + Sync {
    /// 加载有界记录。
    fn load(&self) -> BoxFuture<'_, Option<Checkpoint>>;
    /// 同步持久化；失败不得报告传输成功。
    fn save<'a>(&'a self, checkpoint: &'a Checkpoint) -> BoxFuture<'a, ()>;
    /// 仅在消费者确认记账后删除。
    fn remove(&self) -> BoxFuture<'_, ()>;
}
/// 存储实现拥有原生锁与 IO；核心不直接接触文件路径。
pub trait CheckpointStore: Send + Sync {
    /// operation 为存储的排他键；不同实例复用相同 ID 时须由信封拒绝。
    fn acquire<'a>(&'a self, operation: &'a str) -> BoxFuture<'a, Box<dyn CheckpointLease>>;
}
