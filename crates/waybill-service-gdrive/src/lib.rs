//! Google Drive 上传恢复与下载 service；OAuth 授权和秘密存储属于宿主。
//!
//! 下载提供元数据复核与有界范围读取，不支持 Google 原生文档导出、
//! 跨操作内容去重或通用原子发布。生产端点仅允许 Google HTTPS。
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#[cfg(target_arch = "wasm32")]
compile_error!("waybill-service-gdrive is a native-only prototype");
mod client;
pub mod credential;
mod directory;
mod download;
mod object;
mod upload;
use client::DriveClient;
use credential::TokenProvider;
use std::sync::Arc;
use waybill::{
    BoxFuture,
    download::{DownloadSource, DownloadTarget},
    error::{Error, ErrorKind, Result},
    service::{Service, ServiceId, ServiceIdentity, ServiceInfo},
    upload::UploadSink,
};

pub use download::{RemoteFile, Resolved};

/// 不含秘密的实例配置；账户和 OAuth 应用身份由宿主稳定提供。
#[derive(Clone)]
pub struct GdriveConfig {
    /// 宿主的稳定账户标识。
    pub account: String,
    /// OAuth 应用标识；切换应用不能共享 appProperties 的恢复命名空间。
    pub oauth_application: String,
    /// Drive 根目录对象 ID；root 别名通过 v2 about 解析为真实根 ID，
    /// 不要求 drive.file 凭证具备根目录的 files.get 可见性。
    /// 显式 ID 仍需验证对象是可访问的文件夹。
    pub root: String,
}
/// 独立 GDrive service，消费者可注入核心引擎。
#[derive(Clone)]
pub struct Gdrive {
    identity: ServiceIdentity,
    config: GdriveConfig,
    api: Arc<DriveClient>,
}
impl Gdrive {
    /// 构造生产客户端；不接收 refresh token 或 client secret。
    pub fn new(config: GdriveConfig, credentials: Arc<dyn TokenProvider>) -> Result<Self> {
        if config.account.is_empty()
            || config.account.len() > 256
            || config.oauth_application.is_empty()
            || config.oauth_application.len() > 256
        {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "invalid Drive namespace",
            ));
        }
        object::validate_id(&config.root)?;
        let namespace =
            serde_json::to_vec(&(&config.account, &config.oauth_application, &config.root))
                .map_err(|_| Error::new(ErrorKind::InvalidInput, "Drive namespace"))?;
        let identity = ServiceIdentity {
            service: ServiceId::parse("waybill:gdrive")?,
            instance: blake3::hash(&namespace).to_hex().to_string(),
        };
        Ok(Self {
            identity,
            config,
            api: Arc::new(DriveClient::new(credentials)?),
        })
    }
}
impl Service for Gdrive {
    fn info(&self) -> ServiceInfo {
        ServiceInfo {
            identity: self.identity.clone(),
            capabilities: self.capabilities(),
        }
    }
    fn upload_sink(&self) -> Result<Arc<dyn UploadSink>> {
        Ok(Arc::new(self.clone()))
    }
    fn download_source<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Arc<dyn DownloadSource>> {
        Box::pin(async move {
            let media = self.media(reference)?;
            Ok(Arc::new(media) as Arc<dyn DownloadSource>)
        })
    }
    fn download_target(&self) -> Result<Arc<dyn DownloadTarget>> {
        Err(Error::new(
            ErrorKind::Unsupported,
            "download target unavailable",
        ))
    }
}
#[cfg(test)]
mod tests;
