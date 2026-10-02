//! WebDAV 协议适配；凭证由宿主提供，恢复与本地发布复用公开引擎契约。
//! 配置 endpoint 包含访问根路径，实例绑定 endpoint 与实际认证用户名。
#![forbid(unsafe_code)]
#![deny(missing_docs)]
mod catalog;
mod client;
pub mod credential;
mod download;
mod path;
mod upload;
use std::{sync::Arc, time::Duration};
use waybill::{
    BoxFuture,
    download::DownloadSource,
    error::{Error, ErrorKind, Result},
    object::ObjectMetadata,
    service::{Capabilities, Service, ServiceId, ServiceIdentity, ServiceInfo},
};

/// 非敏感的 WebDAV 实例配置。
#[derive(Clone, Debug)]
pub struct WebdavConfig {
    /// HTTP(S) 端点及根目录；禁止 URL 中的用户名、密码、query 或 fragment。
    pub endpoint: String,
    /// 实际认证用户名；匿名访问可用宿主稳定命名空间。
    pub account: String,
    /// 普通请求从连接到响应正文读完的总时限；默认 60 秒。
    pub request_timeout: Duration,
    /// 整文件 PUT 从连接到响应正文读完的总时限；默认一小时。
    pub upload_timeout: Duration,
}
impl WebdavConfig {
    /// 使用默认请求时限构造配置；时限必须非零且不超过 24 小时。
    pub fn new(endpoint: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            account: account.into(),
            request_timeout: Duration::from_secs(60),
            upload_timeout: Duration::from_secs(3600),
        }
    }
}
/// WebDAV service；复制实例共享连接池，凭证仍在每次请求边界取得。
#[derive(Clone)]
pub struct Webdav {
    identity: ServiceIdentity,
    api: Arc<client::DavClient>,
}
impl Webdav {
    /// 在当前根内选择子目录；共享连接池和凭证提供方，恢复实例随根目录隔离。
    /// 调用方须通过 resolve 确认该引用为目录。
    pub fn at_root(&self, reference: &str) -> Result<Self> {
        if !reference.ends_with('/') {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV root must be directory",
            ));
        }
        let paths = path::Paths {
            root: self.api.paths.url(reference)?,
        };
        let api = self.api.at_root(paths);
        Ok(Self {
            identity: instance_identity(&api.paths, api.account())?,
            api: Arc::new(api),
        })
    }
    /// 构造 service；不获取凭证、不请求网络。HTTP 不提供传输加密。
    pub fn new(
        config: WebdavConfig,
        credentials: Arc<dyn credential::CredentialProvider>,
    ) -> Result<Self> {
        if config.account.is_empty()
            || config.account.len() > 256
            || config.account.chars().any(char::is_control)
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid WebDAV principal",
            ));
        }
        let paths = path::Paths::new(&config.endpoint)?;
        let identity = instance_identity(&paths, &config.account)?;
        Ok(Self {
            identity,
            api: Arc::new(client::DavClient::new(paths, config, credentials)?),
        })
    }
}
fn instance_identity(paths: &path::Paths, account: &str) -> Result<ServiceIdentity> {
    let namespace = serde_json::to_vec(&(paths.root.as_str(), account))
        .map_err(|_| Error::new(ErrorKind::InvalidInput, "invalid WebDAV namespace"))?;
    Ok(ServiceIdentity {
        service: ServiceId::parse("waybill:webdav")?,
        instance: blake3::hash(&namespace).to_hex().to_string(),
    })
}
impl Service for Webdav {
    fn info(&self) -> ServiceInfo {
        ServiceInfo {
            identity: self.identity.clone(),
            capabilities: Capabilities {
                range_download: true,
                stream_upload: true,
                durable_upload: true,
                ..Default::default()
            },
        }
    }
    fn resolve<'a>(&'a self, path: &'a str) -> BoxFuture<'a, ObjectMetadata> {
        Box::pin(async move { Ok(self.stat(path).await?.object) })
    }
    fn list<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Vec<ObjectMetadata>> {
        Box::pin(self.children(reference))
    }
    fn download_source<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Arc<dyn DownloadSource>> {
        Box::pin(self.open_media(reference))
    }
    fn stream_upload_sink(&self) -> Result<Arc<dyn waybill::upload::StreamUploadSink>> {
        Ok(Arc::new(self.clone()))
    }
}
#[cfg(test)]
mod tests;
