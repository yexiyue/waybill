//! 对象存储访问与交付适配；Apache OpenDAL 拥有协议与凭证获取，waybill 拥有生命周期。
//! 宿主注入已配置的 Operator 与稳定账户命名空间，支持按需裁剪后端 features。
#![forbid(unsafe_code)]
#![deny(missing_docs)]
mod catalog;
mod download;
mod error;
mod transport;
mod upload;

pub use opendal;
use opendal::Operator;
use std::sync::Arc;
use waybill::{
    BoxFuture,
    download::DownloadSource,
    error::{Error, ErrorKind, Result},
    object::ObjectMetadata,
    service::{Capabilities, Service, ServiceId, ServiceIdentity, ServiceInfo},
    upload::StreamUploadSink,
};

/// 配置不包含凭证；namespace 必须随账户、端点或 bucket 改变，不能只是本机别名。
#[derive(Clone, Debug)]
pub struct ObjectStorageConfig {
    /// 宿主提供的稳定账户 / 端点 / bucket 命名空间。
    pub namespace: String,
    /// Operator 根下的相对前缀；`/` 代表整个配置根。
    pub root: String,
    /// 宿主确认当前服务端与 bucket 的条件写入生效后才能打开上传。
    /// OSS / COS 的防覆盖头在版本控制开启或暂停时可能不生效。
    /// 此位不创造 OpenDAL 缺少的能力，也不允许无条件覆盖。
    pub conditional_writes: bool,
}
impl ObjectStorageConfig {
    /// 默认只打开访问；上传需要宿主明确确认条件写入约束。
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            root: "/".into(),
            conditional_writes: false,
        }
    }
}

/// 统一对象存储 service；不复制协议客户端，不持久化 OpenDAL 私有 multipart 状态。
#[derive(Clone)]
pub struct ObjectStorage {
    bounded_operator: Operator,
    operator: Operator,
    identity: ServiceIdentity,
    config: ObjectStorageConfig,
    prefix: String,
    write_buffer: usize,
}
impl ObjectStorage {
    /// 接受已配置的 Operator，不发起网络请求。
    /// 凭证刷新由 Operator 的 provider / 宿主拥有；不要注入会无界缓存的 layers。
    pub fn new(operator: Operator, config: ObjectStorageConfig) -> Result<Self> {
        let context = operator
            .base_context()
            .with_http_transport(opendal::HttpTransporter::new(transport::BoundedTransport(
                operator.base_context().http_transport().clone(),
            )));
        Self::from_config(operator.clone().with_context(context), operator, config)
    }
    fn from_config(
        operator: Operator,
        native_operator: Operator,
        config: ObjectStorageConfig,
    ) -> Result<Self> {
        if config.namespace.is_empty()
            || config.namespace.len() > 4096
            || config.namespace.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let prefix = directory(&config.root)?;
        let info = operator.info();
        let namespace = serde_json::to_vec(&(
            info.scheme(),
            info.name(),
            info.root(),
            &config.namespace,
            &prefix,
        ))
        .map_err(|_| invalid())?;
        let identity = ServiceIdentity {
            service: ServiceId::parse(format!("opendal:{}", info.scheme()))?,
            instance: blake3::hash(&namespace).to_hex().to_string(),
        };
        let cap = info.capability();
        // 单 writer 顺序发送；缓冲固定为最小合法分片，另加一块请求体由核心计入预算。
        let write_buffer = cap.write_multi_min_size.unwrap_or(256 * 1024).max(1);
        Ok(Self {
            bounded_operator: operator,
            operator: native_operator,
            identity,
            config,
            prefix,
            write_buffer,
        })
    }
    /// OpenDAL 原始访问入口；其操作不具有 waybill 的 checkpoint / 回执保证。
    pub fn operator(&self) -> &Operator {
        &self.operator
    }
    /// 在当前配置根下选择子目录；共享连接和凭证，隔离恢复实例。
    pub fn at_root(&self, reference: &str) -> Result<Self> {
        let child = directory(reference)?;
        let mut config = self.config.clone();
        config.root = format!("{}{}", self.prefix, child);
        if config.root.is_empty() {
            config.root = "/".into();
        }
        Self::new(self.operator.clone(), config)
    }
    fn key(&self, reference: &str) -> Result<String> {
        if reference == "/" {
            return Ok(self.prefix.clone());
        }
        validate_path(reference)?;
        Ok(format!("{}{reference}", self.prefix))
    }
    fn native_capabilities(&self) -> opendal::Capability {
        self.bounded_operator.info().capability()
    }
    fn upload_supported(&self) -> bool {
        let c = self.native_capabilities();
        self.config.conditional_writes
            && self.download_supported()
            && c.write
            && c.write_can_multi
            && c.write_can_empty
            && upload::marker::supported(&self.bounded_operator, c)
            && c.write_with_if_not_exists
            && self.write_buffer < 16 * 1024 * 1024
    }
    fn download_supported(&self) -> bool {
        let c = self.native_capabilities();
        c.stat && c.read && c.read_with_if_match
    }
}
impl Service for ObjectStorage {
    fn info(&self) -> ServiceInfo {
        let upload = self.upload_supported();
        ServiceInfo {
            identity: self.identity.clone(),
            capabilities: Capabilities {
                range_download: self.download_supported(),
                stream_upload: upload,
                durable_upload: upload,
                ..Default::default()
            },
        }
    }
    fn resolve<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, ObjectMetadata> {
        Box::pin(self.resolve_object(reference))
    }
    fn list<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Vec<ObjectMetadata>> {
        Box::pin(self.children(reference))
    }
    fn download_source<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Arc<dyn DownloadSource>> {
        Box::pin(self.open_media(reference))
    }
    fn stream_upload_sink(&self) -> Result<Arc<dyn StreamUploadSink>> {
        if !self.upload_supported() {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "object upload requires bounded multipart, conditional writes, metadata and versioned reads",
            ));
        }
        Ok(Arc::new(self.clone()))
    }
}
fn invalid() -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "invalid object storage reference or namespace",
    )
}
fn validate_path(reference: &str) -> Result<()> {
    let path = reference.strip_suffix('/').unwrap_or(reference);
    if !waybill::object::valid_object_path(path)
        || path.split('/').any(|p| p.starts_with(".waybill-"))
    {
        return Err(invalid());
    }
    Ok(())
}
fn directory(reference: &str) -> Result<String> {
    if reference == "/" {
        return Ok(String::new());
    }
    validate_path(reference)?;
    Ok(format!("{}/", reference.trim_end_matches('/')))
}
#[cfg(all(test, feature = "s3"))]
mod tests;
