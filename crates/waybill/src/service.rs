//! 开放 service 标识与按实例声明的能力。
use crate::{
    BoxFuture,
    download::{DownloadSource, DownloadTarget},
    error::{Error, ErrorKind, Result},
    object::ObjectMetadata,
    source::Source,
    upload::{StreamUploadSink, UploadSink},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
/// 开放命名空间标识，避免核心 provider 枚举。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ServiceId(String);
impl ServiceId {
    /// 接受 `namespace:name`，最长 128 字节，小写 ASCII 与 `-._`。
    pub fn parse(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let valid = value
            .split_once(':')
            .is_some_and(|(ns, name)| !ns.is_empty() && !name.is_empty() && !name.contains(':'))
            && value.len() <= 128
            && value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b":-._".contains(&c));
        if !valid {
            return Err(Error::new(ErrorKind::InvalidInput, "invalid service id"));
        }
        Ok(Self(value))
    }
    /// 返回规范标识。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl TryFrom<String> for ServiceId {
    type Error = Error;
    fn try_from(value: String) -> Result<Self> {
        Self::parse(value)
    }
}
impl std::str::FromStr for ServiceId {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        Self::parse(value)
    }
}
impl From<ServiceId> for String {
    fn from(value: ServiceId) -> Self {
        value.0
    }
}
/// 恢复绑定的后端与账户 / 端点 / 根目录实例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceIdentity {
    /// 后端标识。
    pub service: ServiceId,
    /// 稳定且不含凭证的宿主命名空间。
    pub instance: String,
}
impl ServiceIdentity {
    /// 校验稳定、非敏感的实例命名空间；构造服务和开始传输时调用。
    pub fn validate(&self) -> Result<()> {
        if self.instance.is_empty()
            || self.instance.len() > 256
            || self.instance.chars().any(char::is_control)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid service instance",
            ));
        }
        Ok(())
    }
}
/// 当前实例的首期能力，不外推浏览器或其他后端。
#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    /// 稳定范围读源。
    pub range_source: bool,
    /// 支持连续偏移上传。
    pub offset_upload: bool,
    /// 支持有界整文件流式上传；不表示支持按偏移续传。
    pub stream_upload: bool,
    /// 支持服务端结果对账与跨进程恢复；不等同于偏移续传。
    pub durable_upload: bool,
    /// 云端源支持精确范围读取。
    pub range_download: bool,
    /// 本地目标支持精确偏移随机写并逐块同步。
    pub random_write: bool,
    /// 本地目标支持同盘原子发布。
    pub durable_publish: bool,
}
/// service 元信息。
#[derive(Debug, Clone)]
pub struct ServiceInfo {
    /// 实例身份。
    pub identity: ServiceIdentity,
    /// 已实现能力。
    pub capabilities: Capabilities,
}
/// 外部 crate 可直接实现；读取与写入入口独立且默认明确拒绝。
pub trait Service: Send + Sync {
    /// 返回实例能力。
    fn info(&self) -> ServiceInfo;
    /// 解析配置根下的相对路径；根目录为 `/`，返回后端稳定对象引用。
    /// 路径与对象引用不同（例如 Drive 文件 ID），语法由 service 校验。
    fn resolve<'a>(&'a self, _path: &'a str) -> BoxFuture<'a, ObjectMetadata> {
        Box::pin(async {
            Err(Error::new(
                ErrorKind::Unsupported,
                "path resolution unavailable",
            ))
        })
    }
    /// 列举目录引用的直接子项；不递归，service 必须限制结果和响应大小。
    fn list<'a>(&'a self, _reference: &'a str) -> BoxFuture<'a, Vec<ObjectMetadata>> {
        Box::pin(async {
            Err(Error::new(
                ErrorKind::Unsupported,
                "directory listing unavailable",
            ))
        })
    }
    /// 以 service 自己解释的对象引用打开源。
    fn source<'a>(&'a self, _reference: &'a str) -> BoxFuture<'a, Arc<dyn Source>> {
        Box::pin(async { Err(Error::new(ErrorKind::Unsupported, "source unavailable")) })
    }
    /// 获取上传契约；最小只读 service 无需实现。
    fn upload_sink(&self) -> Result<Arc<dyn UploadSink>> {
        Err(Error::new(ErrorKind::Unsupported, "upload unavailable"))
    }
    /// 获取整文件上传契约；与连续偏移上传独立。
    fn stream_upload_sink(&self) -> Result<Arc<dyn StreamUploadSink>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "stream upload unavailable",
        ))
    }
    /// 以 service 自己解释的对象引用打开下载源。
    fn download_source<'a>(
        &'a self,
        _reference: &'a str,
    ) -> BoxFuture<'a, Arc<dyn DownloadSource>> {
        Box::pin(async {
            Err(Error::new(
                ErrorKind::Unsupported,
                "download source unavailable",
            ))
        })
    }
    /// 获取本地下载目标；云端 service 无需实现。
    fn download_target(&self) -> Result<Arc<dyn DownloadTarget>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "download target unavailable",
        ))
    }
}
