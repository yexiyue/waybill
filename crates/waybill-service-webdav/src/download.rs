//! 强 ETag 绑定的精确范围读取；普通 ETag 不作为内容摘要。
use crate::{
    Webdav,
    client::{bounded, network, success},
};
use std::sync::Arc;
use waybill::{
    BoxFuture,
    download::{DownloadSource, RemoteIdentity},
    error::{Error, ErrorKind, Result},
    service::Capabilities,
};
const READ_LIMIT: usize = 8 * 1024 * 1024;

pub(crate) struct Media {
    service: Webdav,
    reference: String,
    etag: String,
    size: u64,
}
impl Webdav {
    pub(crate) async fn open_media(&self, reference: &str) -> Result<Arc<dyn DownloadSource>> {
        let metadata = self.stat(reference).await?;
        if metadata.object.is_directory() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "WebDAV directory cannot be downloaded",
            ));
        }
        let (size, etag) = self.head_identity(reference).await?;
        if metadata.object.size != Some(size)
            || metadata.etag.as_deref().is_some_and(|tag| tag != etag)
        {
            return Err(Error::new(
                ErrorKind::SourceChanged,
                "WebDAV metadata changed while opening source",
            ));
        }
        let media = Media {
            service: self.clone(),
            reference: reference.into(),
            etag,
            size,
        };
        // 不依据 Accept-Ranges 猜测服务器支持；首字节探针在下载暂存创建前验证。
        if size > 0 {
            media.read_range(0, 1).await?;
        }
        Ok(Arc::new(media))
    }
    pub(crate) async fn head_identity(&self, reference: &str) -> Result<(u64, String)> {
        let response = self.api.head(reference).await?;
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
        {
            return Err(Error::new(
                ErrorKind::Protocol,
                "encoded WebDAV representation",
            ));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .filter(|v| strong_etag(v))
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Unsupported,
                    "WebDAV strong ETag required for recovery",
                )
            })?
            .to_owned();
        let size = match response.headers().get("content-length") {
            Some(value) => value
                .to_str()
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| Error::new(ErrorKind::Protocol, "invalid WebDAV content length"))?,
            None => {
                // HEAD 可省略长度（Apache 的空文件如此）；PROPFIND 的长度必须绑定同一强版本。
                let metadata = self.stat(reference).await?;
                if metadata.object.is_directory() || metadata.etag.as_deref() != Some(etag.as_str())
                {
                    return Err(Error::new(
                        ErrorKind::SourceChanged,
                        "WebDAV metadata version changed",
                    ));
                }
                metadata.object.size.ok_or_else(|| {
                    Error::new(ErrorKind::Unsupported, "WebDAV content length required")
                })?
            }
        };
        Ok((size, etag))
    }
}
impl DownloadSource for Media {
    fn max_read_size(&self) -> usize {
        READ_LIMIT
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            range_download: true,
            ..Default::default()
        }
    }
    fn identity(&self) -> BoxFuture<'_, RemoteIdentity> {
        Box::pin(async {
            let (size, revision) = self.service.head_identity(&self.reference).await?;
            if revision != self.etag || size != self.size {
                return Err(Error::new(
                    ErrorKind::SourceChanged,
                    "WebDAV source identity changed",
                ));
            }
            Ok(RemoteIdentity {
                service: self.service.identity.clone(),
                reference: self.reference.clone(),
                revision,
                size,
                digest: None,
            })
        })
    }
    fn read_range(&self, offset: u64, length: usize) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async move {
            if length == 0
                || length > READ_LIMIT
                || offset
                    .checked_add(length as u64)
                    .is_none_or(|end| end > self.size)
            {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "WebDAV range exceeds bound",
                ));
            }
            let end = offset + length as u64 - 1;
            let response = self
                .service
                .api
                .request(reqwest::Method::GET, &self.reference)
                .await?
                .header("Range", format!("bytes={offset}-{end}"))
                .header("If-Match", &self.etag)
                .send()
                .await
                .map_err(network)?;
            if response.status().as_u16() == 412 {
                return Err(Error::new(
                    ErrorKind::SourceChanged,
                    "WebDAV source changed during range read",
                ));
            }
            success(&response)?;
            if response.status().as_u16() != 206 {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "WebDAV exact range response required",
                ));
            }
            let expected = format!("bytes {offset}-{end}/{}", self.size);
            if response
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                != Some(expected.as_str())
                || response.headers().get("etag").and_then(|v| v.to_str().ok())
                    != Some(self.etag.as_str())
                || response
                    .headers()
                    .get("content-encoding")
                    .is_some_and(|v| v != "identity")
            {
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "WebDAV range identity mismatch",
                ));
            }
            let data = bounded(response, length).await?;
            if data.len() != length {
                return Err(Error::new(ErrorKind::Protocol, "WebDAV short range body"));
            }
            Ok(data)
        })
    }
}
pub(crate) fn strong_etag(value: &str) -> bool {
    value.len() >= 2
        && value.len() <= 256
        && value.starts_with('"')
        && value.ends_with('"')
        && value.as_bytes()[1..value.len() - 1]
            .iter()
            .all(|b| matches!(b, 0x21 | 0x23..=0x7e))
}
